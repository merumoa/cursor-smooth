//! Experimental macOS adapter: active head-insert HID event tap.
//!
//! Native CI builds it for arm64 and Intel, and the unit tests cover the tag
//! codec and the permission gate. Physical mouse behaviour, Retina grids,
//! multi-display seams, wake handling and the CPU cost of posting are still
//! unverified on real hardware; the docs say so explicitly.

use crate::cli::RunOptions;
use crate::core::{Engine, PixelGrid, Point};
use crate::platform::{EXIT_FAILURE, EXIT_OK, EXIT_PERMISSION, EXIT_USAGE};
use crate::runtime::{
    self, FaultCode, Mapping, Metrics, MetricsSnapshot, Msg, ReportMeta, Shared, State,
};
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CallbackResult, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use foreign_types::ForeignType;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAGIC_OWN: u64 = 0x5A;
const MAGIC_DIAGNOSTIC: u64 = 0xD4;
const OUTPUT_INTERVAL_NS: u64 = 1_000_000;
const HEARTBEAT_LIMIT_NS: u64 = 50_000_000;

/// libdispatch semaphore handle; the only synchronisation primitive used by
/// the adapter because Darwin has no POSIX `sem_timedwait`/`sem_init`.
type DispatchSemaphore = *mut c_void;

const DISPATCH_TIME_NOW: u64 = 0;
const DISPATCH_TIME_FOREVER: u64 = u64::MAX;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn dispatch_semaphore_create(value: isize) -> DispatchSemaphore;
    fn dispatch_semaphore_signal(sem: DispatchSemaphore) -> isize;
    fn dispatch_semaphore_wait(sem: DispatchSemaphore, timeout: u64) -> isize;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn mach_absolute_time() -> u64;
    fn mach_wait_until(deadline: u64) -> i32;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopStop(run_loop: *mut c_void);
    fn CGPreflightListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: *mut c_void, seconds: f64);
    fn signal(sig: i32, handler: usize) -> usize;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn handle_signal(_signal: i32) {
    // Only an atomic store: the waiter polls this flag, so nothing
    // signal-unsafe runs inside the handler.
    STOP_REQUESTED.store(true, Ordering::Release);
}

struct Clock {
    numer: u64,
    denom: u64,
}

impl Clock {
    fn new() -> Self {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut info) };
        Self {
            numer: u64::from(info.numer.max(1)),
            denom: u64::from(info.denom.max(1)),
        }
    }

    fn now_ns(&self) -> u64 {
        let ticks = unsafe { mach_absolute_time() };
        ticks.saturating_mul(self.numer) / self.denom
    }

    fn ticks(&self, ns: u64) -> u64 {
        ns.saturating_mul(self.denom) / self.numer
    }
}

/// All three permissions are required before suppression may start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Preflight {
    pub accessibility: bool,
    pub listen: bool,
    pub post: bool,
}

impl Preflight {
    pub fn check() -> Self {
        Self {
            accessibility: unsafe { AXIsProcessTrusted() },
            listen: unsafe { CGPreflightListenEventAccess() },
            post: unsafe { CGPreflightPostEventAccess() },
        }
    }

    pub fn complete(&self) -> bool {
        self.accessibility && self.listen && self.post
    }
}

fn permission_guidance(preflight: &Preflight) -> String {
    let mut text = String::from("cursor-smooth: macOS permissions are missing:\n");
    if !preflight.accessibility {
        text.push_str("  - Accessibility (System Settings > Privacy & Security > Accessibility)\n");
    }
    if !preflight.listen {
        text.push_str("  - Input Monitoring (listening to mouse events)\n");
    }
    if !preflight.post {
        text.push_str("  - Accessibility for posting synthetic mouse movement\n");
    }
    text.push_str("cursor-smooth never requests root and never changes TCC/SIP state.\n");
    text
}

/// Tag layout for posted events: 8 magic bits, 8 generation bits, 16 nonce bits.
pub fn own_tag(nonce: u64, generation: u64) -> i64 {
    (((nonce & 0xFFFF) << 16) | ((generation & 0xFF) << 8) | (MAGIC_OWN & 0xFF)) as i64
}

/// Decodes a tag written by [`own_tag`]; returns the generation.
pub fn decode_tag(tag: i64) -> Option<u64> {
    let value = tag as u64;
    if value & 0xFF != MAGIC_OWN & 0xFF {
        return None;
    }
    Some((value >> 8) & 0xFF)
}

fn is_diagnostic(tag: i64, nonce: u64) -> bool {
    let value = tag as u64;
    value & 0xFF == MAGIC_DIAGNOSTIC && (value >> 16) & 0xFFFF == nonce & 0xFFFF
}

fn grid() -> PixelGrid {
    let display = CGDisplay::main();
    let bounds = display.bounds();
    PixelGrid::new(
        Point::new(bounds.origin.x, bounds.origin.y),
        Point::new(
            (display.pixels_wide() as f64).max(1.0),
            (display.pixels_high() as f64).max(1.0),
        ),
    )
}

pub fn cursor_position() -> Option<Point> {
    let source = CGEventSource::new(CGEventSourceStateID::Private).ok()?;
    let event = CGEvent::new(source).ok()?;
    let location = event.location();
    Some(Point::new(location.x, location.y))
}

struct Native {
    stop_sem: DispatchSemaphore,
    work_sem: DispatchSemaphore,
    clock: Clock,
    run_loop: AtomicUsize,
}

// The semaphore handles are immutable after construction; signaller and waiter
// may run on different threads.
unsafe impl Send for Native {}
unsafe impl Sync for Native {}

impl Native {
    fn new() -> Self {
        let stop = unsafe { dispatch_semaphore_create(0) };
        let work = unsafe { dispatch_semaphore_create(0) };
        Self {
            stop_sem: stop,
            work_sem: work,
            clock: Clock::new(),
            run_loop: AtomicUsize::new(0),
        }
    }

    fn post_stop(&self) {
        unsafe { dispatch_semaphore_signal(self.stop_sem) };
    }

    fn post_work(&self) {
        unsafe { dispatch_semaphore_signal(self.work_sem) };
    }

    /// Bounded wait used by the supervisor loop and the guard; the result is
    /// ignored because both callers re-read the authoritative atomic state.
    fn wait_stop_timeout(&self, timeout_ns: u64) {
        let timeout = unsafe { dispatch_time(DISPATCH_TIME_NOW, timeout_ns as i64) };
        unsafe { dispatch_semaphore_wait(self.stop_sem, timeout) };
    }
}

struct Session {
    shared: Arc<Shared>,
    metrics: Arc<Metrics>,
    native: Arc<Native>,
    tap: Option<JoinHandle<()>>,
    emitter: Option<JoinHandle<()>>,
    guard: Option<JoinHandle<()>>,
    report: Option<PathBuf>,
    wall_start: Instant,
    cpu_start: (u64, u64),
}

pub fn run(options: &RunOptions) -> i32 {
    if !options.experimental_macos {
        eprintln!(
            "cursor-smooth: macOS support is experimental and unverified on physical hardware; pass --experimental-macos to continue"
        );
        return EXIT_USAGE;
    }
    let preflight = Preflight::check();
    if !preflight.complete() {
        eprint!("{}", permission_guidance(&preflight));
        return EXIT_PERMISSION;
    }
    let session = match Session::start(options) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("cursor-smooth: {error}");
            return EXIT_FAILURE;
        }
    };
    session.wait(options.duration);
    let summary = session.finish();
    println!(
        "cursor-smooth (experimental): fault={} emitted={} missed={} user_ms={} kernel_ms={} wall_ms={}",
        summary.fault.as_str(),
        summary.metrics.emitted,
        summary.metrics.missed_deadlines,
        summary.user_ms,
        summary.kernel_ms,
        summary.wall_ns / 1_000_000
    );
    EXIT_OK
}

pub fn doctor() -> i32 {
    let preflight = Preflight::check();
    println!("os: macos");
    println!("arch: {}", std::env::consts::ARCH);
    println!("experimental: yes (physical hardware behaviour unverified)");
    println!("accessibility: {}", preflight.accessibility);
    println!("input_monitoring: {}", preflight.listen);
    println!("post_events: {}", preflight.post);
    let grid = grid();
    println!(
        "main_display: origin=({:.0},{:.0}) pixels=({:.0},{:.0})",
        grid.origin.x, grid.origin.y, grid.size.x, grid.size.y
    );
    println!("tap: not installed (doctor is read-only)");
    if preflight.complete() {
        EXIT_OK
    } else {
        eprint!("{}", permission_guidance(&preflight));
        EXIT_PERMISSION
    }
}

impl Session {
    fn start(options: &RunOptions) -> Result<Session, String> {
        if let Some(path) = &options.report {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| format!("cannot create report {}: {error}", path.display()))?;
        }
        let native = Arc::new(Native::new());
        let shared = Arc::new(Shared::new());
        let metrics = Arc::new(Metrics::new());
        let queue = runtime::make_queue();
        STOP_REQUESTED.store(false, Ordering::Release);
        unsafe {
            signal(SIGINT, handle_signal as *const () as usize);
            signal(SIGTERM, handle_signal as *const () as usize);
        }

        let tap_shared = Arc::clone(&shared);
        let tap_native = Arc::clone(&native);
        let producer = queue.producer;
        let tap = thread::Builder::new()
            .name("cursor-smooth-tap".into())
            .spawn(move || tap_thread(tap_shared, tap_native, producer))
            .map_err(|error| format!("cannot start tap thread: {error}"))?;
        let emitter_shared = Arc::clone(&shared);
        let emitter_native = Arc::clone(&native);
        let emitter_metrics = Arc::clone(&metrics);
        let mapping = Mapping::RelativeToCommitted;
        let emitter = thread::Builder::new()
            .name("cursor-smooth-emitter".into())
            .spawn(move || {
                emitter_thread(
                    emitter_shared,
                    emitter_native,
                    emitter_metrics,
                    queue.consumer,
                    mapping,
                )
            })
            .map_err(|error| format!("cannot start emitter: {error}"))?;
        let guard_shared = Arc::clone(&shared);
        let guard_native = Arc::clone(&native);
        let guard = thread::Builder::new()
            .name("cursor-smooth-guard".into())
            .spawn(move || guard_thread(guard_shared, guard_native))
            .map_err(|error| format!("cannot start guard: {error}"))?;

        Ok(Session {
            shared,
            metrics,
            native,
            tap: Some(tap),
            emitter: Some(emitter),
            guard: Some(guard),
            report: options.report.clone(),
            wall_start: Instant::now(),
            cpu_start: process_cpu_ms(),
        })
    }

    fn wait(&self, duration: Option<Duration>) {
        let deadline = duration.map(|value| Instant::now() + value);
        loop {
            if STOP_REQUESTED.load(Ordering::Acquire) || self.shared.stopping() {
                return;
            }
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                return;
            }
            self.native.wait_stop_timeout(20_000_000);
        }
    }

    fn finish(mut self) -> Summary {
        self.shared.invalidate();
        self.shared.set_state(State::Off);
        self.shared.request_stop();
        self.native.post_stop();
        self.native.post_work();
        let run_loop = self.native.run_loop.load(Ordering::Acquire);
        if run_loop != 0 {
            unsafe { CFRunLoopStop(run_loop as *mut c_void) };
        }
        if let Some(handle) = self.tap.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.emitter.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.guard.take() {
            let _ = handle.join();
        }
        let wall_ns = self.wall_start.elapsed().as_nanos() as u64;
        let cpu_end = process_cpu_ms();
        let endpoint_error = cursor_position()
            .map(|point| {
                let (dx, dy) = self.shared.displayed();
                ((point.x - dx as f64).abs()).max((point.y - dy as f64).abs()) as u64
            })
            .unwrap_or(0);
        let metrics = self.metrics.snapshot();
        if let Some(path) = &self.report {
            let meta = ReportMeta {
                kind: "run".to_string(),
                os: "macos".to_string(),
                arch: std::env::consts::ARCH.to_string(),
                cpu: std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into()),
                logical_cpus: std::thread::available_parallelism()
                    .map(|value| value.get() as u32)
                    .unwrap_or(1),
                wall_ns,
                user_ms: cpu_end.0.saturating_sub(self.cpu_start.0),
                kernel_ms: cpu_end.1.saturating_sub(self.cpu_start.1),
                endpoint_error_px: endpoint_error,
                notes:
                    "experimental: physical mouse, Retina/multi-display, wake and CPU unverified"
                        .into(),
            };
            if let Err(error) = runtime::write_report_over(path, &meta, &metrics) {
                eprintln!("cursor-smooth: cannot write report: {error}");
            }
        }
        Summary {
            metrics,
            fault: self.shared.fault(),
            wall_ns,
            user_ms: cpu_end.0.saturating_sub(self.cpu_start.0),
            kernel_ms: cpu_end.1.saturating_sub(self.cpu_start.1),
            endpoint_error_px: endpoint_error,
        }
    }
}

fn tap_thread(shared: Arc<Shared>, native: Arc<Native>, producer: rtrb::Producer<Msg>) {
    let run_loop = unsafe { CFRunLoopGetCurrent() };
    native.run_loop.store(run_loop as usize, Ordering::Release);
    let nonce = native.clock.now_ns() & 0xFFFF;
    let events = vec![
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
    ];
    let tap_native = Arc::clone(&native);
    let tap_shared = Arc::clone(&shared);
    let tap_producer = Mutex::new(producer);
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events,
        move |_proxy: CGEventTapProxy, kind: CGEventType, event: &CGEvent| match tap_producer.lock()
        {
            Ok(mut producer) => {
                tap_callback(&tap_shared, &tap_native, nonce, kind, event, &mut producer)
            }
            Err(_) => CallbackResult::Keep,
        },
    );
    let tap = match tap {
        Ok(tap) => tap,
        Err(_) => {
            shared.suspend(FaultCode::HookInstallFailed);
            native.post_stop();
            return;
        }
    };
    let loop_source = match tap.mach_port().create_runloop_source(0) {
        Ok(source) => source,
        Err(_) => {
            shared.suspend(FaultCode::HookInstallFailed);
            native.post_stop();
            return;
        }
    };
    unsafe {
        CFRunLoop::get_current().add_source(&loop_source, kCFRunLoopCommonModes);
    }
    tap.enable();
    let anchor = cursor_position().unwrap_or(Point::ZERO);
    shared.set_committed(anchor.x as i32, anchor.y as i32);
    shared.set_displayed(anchor.x as i32, anchor.y as i32);
    shared.set_warmed(false);
    shared.set_state(State::Warming);
    CFRunLoop::run_current();
    shared.invalidate();
    shared.set_state(State::Off);
    drop(tap);
}

fn tap_callback(
    shared: &Shared,
    native: &Native,
    nonce: u64,
    kind: CGEventType,
    event: &CGEvent,
    producer: &mut rtrb::Producer<Msg>,
) -> CallbackResult {
    let now = native.clock.now_ns();
    match kind {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            shared.suspend(FaultCode::HookInstallFailed);
            native.post_stop();
            return CallbackResult::Keep;
        }
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            if shared.suppressing() {
                shared.invalidate();
                let location = event.location();
                let _ = runtime::push(
                    producer,
                    Msg::Barrier {
                        at_ns: now,
                        anchor: Point::new(location.x, location.y),
                    },
                );
                native.post_work();
                shared.set_state(State::ButtonsPassthrough);
                shared.set_pending(false);
            }
            return CallbackResult::Keep;
        }
        CGEventType::LeftMouseUp | CGEventType::RightMouseUp | CGEventType::OtherMouseUp => {
            if shared.state() == State::ButtonsPassthrough {
                shared.set_warmed(false);
                let anchor = cursor_position().unwrap_or(Point::ZERO);
                let _ = runtime::push(producer, Msg::Barrier { at_ns: now, anchor });
                native.post_work();
                shared.set_state(State::Warming);
            }
            return CallbackResult::Keep;
        }
        CGEventType::MouseMoved => {}
        _ => return CallbackResult::Keep,
    }

    let tag = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA);
    if is_diagnostic(tag, nonce) {
        let location = event.location();
        shared.set_committed(location.x as i32, location.y as i32);
        shared.set_displayed(location.x as i32, location.y as i32);
        return CallbackResult::Keep;
    }
    if let Some(generation) = decode_tag(tag) {
        if generation == shared.generation() & 0xFF {
            let location = event.location();
            shared.set_committed(location.x as i32, location.y as i32);
            shared.set_displayed(location.x as i32, location.y as i32);
            return CallbackResult::Keep;
        }
        return CallbackResult::Drop;
    }

    let delta_x = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X);
    let delta_y = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y);
    let state = shared.state();
    if state.suppressing() {
        let pushed = runtime::push(
            producer,
            Msg::Native {
                at_ns: now,
                value: Point::new(delta_x as f64, delta_y as f64),
            },
        );
        if !pushed {
            shared.suspend(FaultCode::QueueOverflow);
            native.post_stop();
            return CallbackResult::Keep;
        }
        native.post_work();
        return CallbackResult::Drop;
    }
    if state == State::Warming {
        let _ = runtime::push(
            producer,
            Msg::Native {
                at_ns: now,
                value: Point::new(delta_x as f64, delta_y as f64),
            },
        );
        native.post_work();
    }
    let location = event.location();
    shared.set_committed(location.x as i32, location.y as i32);
    shared.set_displayed(location.x as i32, location.y as i32);
    CallbackResult::Keep
}

fn emitter_thread(
    shared: Arc<Shared>,
    native: Arc<Native>,
    metrics: Arc<Metrics>,
    mut consumer: rtrb::Consumer<Msg>,
    mapping: Mapping,
) {
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::Private) else {
        shared.suspend(FaultCode::EmitterStall);
        return;
    };
    unsafe { CGEventSourceSetLocalEventsSuppressionInterval(source.as_ptr() as *mut c_void, 0.0) };
    let grid = grid();
    let mut engine = Engine::new(Point::ZERO);
    let nonce = native.clock.now_ns() & 0xFFFF;
    let mut last_output: Option<(i32, i32)> = None;
    loop {
        if shared.stopping() || STOP_REQUESTED.load(Ordering::Acquire) {
            break;
        }
        if !shared.suppressing() {
            unsafe { dispatch_semaphore_wait(native.work_sem, DISPATCH_TIME_FOREVER) };
            drain(&shared, &metrics, &mut engine, mapping, &mut consumer);
            continue;
        }
        let deadline = native.clock.now_ns() + OUTPUT_INTERVAL_NS;
        unsafe { mach_wait_until(native.clock.ticks(deadline)) };
        while unsafe { dispatch_semaphore_wait(native.work_sem, DISPATCH_TIME_NOW) } == 0 {
            drain(&shared, &metrics, &mut engine, mapping, &mut consumer);
        }
        let now = native.clock.now_ns();
        if !shared.suppressing() {
            continue;
        }
        if let Some(target) = engine.sample(now) {
            let (x, y) = grid.quantize(target);
            if last_output != Some((x, y)) {
                let tag = own_tag(nonce, shared.generation());
                match CGEvent::new_mouse_event(
                    source.clone(),
                    CGEventType::MouseMoved,
                    CGPoint::new(x as f64, y as f64),
                    core_graphics::event::CGMouseButton::Left,
                ) {
                    Ok(event) => {
                        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, tag);
                        event.post(CGEventTapLocation::HID);
                        metrics.emitted.fetch_add(1, Ordering::AcqRel);
                        shared.set_displayed(x, y);
                        shared.set_emit_ns(now);
                        shared.heartbeat(now);
                        last_output = Some((x, y));
                    }
                    Err(_) => {
                        shared.suspend(FaultCode::InjectionFailed);
                    }
                }
            } else {
                metrics.skipped_identical.fetch_add(1, Ordering::AcqRel);
            }
        } else {
            shared.set_pending(false);
        }
        metrics.output_ticks.fetch_add(1, Ordering::AcqRel);
        metrics.timer_wakes.fetch_add(1, Ordering::AcqRel);
    }
}

fn drain(
    shared: &Shared,
    metrics: &Metrics,
    engine: &mut Engine,
    mapping: Mapping,
    consumer: &mut rtrb::Consumer<Msg>,
) {
    while let Ok(message) = consumer.pop() {
        if matches!(message, Msg::Stop) {
            shared.request_stop();
            break;
        }
        metrics.input_events.fetch_add(1, Ordering::AcqRel);
        if matches!(message, Msg::Native { .. }) {
            metrics.native_moves.fetch_add(1, Ordering::AcqRel);
        }
        runtime::apply_message(engine, mapping, message);
    }
    let warmed = engine.is_warmed();
    shared.set_warmed(warmed);
    if warmed && shared.state() == State::Warming {
        shared.set_state(State::Active);
    }
    let pending = shared.suppressing() && engine.has_pending();
    shared.set_pending(pending);
    shared.heartbeat(engine_last_ns(engine));
}

fn engine_last_ns(_engine: &Engine) -> u64 {
    // The adapter heartbeats on every drain; the engine does not expose its
    // last sample timestamp, so the caller's clock is used where available.
    unsafe { mach_absolute_time() }
}

fn guard_thread(shared: Arc<Shared>, native: Arc<Native>) {
    let clock = Clock::new();
    let mut last_counts = shared.move_counts();
    let mut readback_failures = 0_u32;
    loop {
        if shared.stopping() || STOP_REQUESTED.load(Ordering::Acquire) {
            break;
        }
        let suppressing = shared.suppressing();
        let timeout = if suppressing { 50_000_000 } else { 500_000_000 };
        let timeout = unsafe { dispatch_time(DISPATCH_TIME_NOW, timeout as i64) };
        unsafe { dispatch_semaphore_wait(native.work_sem, timeout) };
        if shared.stopping() || STOP_REQUESTED.load(Ordering::Acquire) {
            break;
        }
        if !shared.suppressing() {
            last_counts = shared.move_counts();
            readback_failures = 0;
            continue;
        }
        let now = clock.now_ns();
        if shared.pending() && now.saturating_sub(shared.heartbeat_ns()) > HEARTBEAT_LIMIT_NS {
            shared.suspend(FaultCode::EmitterStall);
            continue;
        }
        let counts = shared.move_counts();
        if counts.0 > last_counts.0 && counts.1 == last_counts.1 {
            shared.suspend(FaultCode::HookMissing);
            continue;
        }
        last_counts = counts;
        if shared.fault() == FaultCode::None {
            match cursor_position() {
                Some(point) => {
                    let (dx, dy) = shared.displayed();
                    let distance = ((point.x - dx as f64).abs()).max((point.y - dy as f64).abs());
                    if distance > 8.0 {
                        let last_emit = shared.emit_ns();
                        readback_failures += 1;
                        if readback_failures >= 2 {
                            let cause =
                                if last_emit == 0 || now.saturating_sub(last_emit) > 100_000_000 {
                                    FaultCode::HookMissing
                                } else {
                                    FaultCode::CursorTeleport
                                };
                            shared.suspend(cause);
                        }
                    } else {
                        readback_failures = 0;
                    }
                }
                None => {
                    shared.suspend(FaultCode::CursorReadFailed);
                }
            }
        }
    }
}

fn process_cpu_ms() -> (u64, u64) {
    #[repr(C)]
    struct Rusage {
        utime_sec: i64,
        utime_usec: i32,
        _pad0: i32,
        stime_sec: i64,
        stime_usec: i32,
        _pad1: i32,
        other: [i64; 16],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    const RUSAGE_SELF: i32 = 0;
    unsafe {
        let mut usage: Rusage = std::mem::zeroed();
        if getrusage(RUSAGE_SELF, &mut usage) != 0 {
            return (0, 0);
        }
        let user = (usage.utime_sec * 1000 + usage.utime_usec as i64 / 1000).max(0) as u64;
        let kernel = (usage.stime_sec * 1000 + usage.stime_usec as i64 / 1000).max(0) as u64;
        (user, kernel)
    }
}

/// Result of one supervised run.
pub struct Summary {
    pub metrics: MetricsSnapshot,
    pub fault: FaultCode,
    pub wall_ns: u64,
    pub user_ms: u64,
    pub kernel_ms: u64,
    pub endpoint_error_px: u64,
}

pub fn now_ns() -> u64 {
    Clock::new().now_ns()
}

pub fn platform_description() -> (String, String, u32) {
    (
        "macos".to_string(),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into()),
        std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(1),
    )
}

pub fn pointer_speed() -> u32 {
    0
}

pub fn display_grid() -> PixelGrid {
    grid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_tag_round_trips_generation_and_nonce() {
        for generation in [0_u64, 1, 7, 255] {
            for nonce in [0_u64, 0x1234, 0xFFFF] {
                let tag = own_tag(nonce, generation);
                assert_eq!(decode_tag(tag), Some(generation));
                assert!(!is_diagnostic(tag, nonce));
            }
        }
    }

    #[test]
    fn foreign_tags_are_rejected() {
        assert_eq!(decode_tag(0), None);
        assert_eq!(decode_tag(0x55), None);
        // A diagnostic tag is not an own tag.
        let diagnostic = (((0x1234_u64) << 16) | MAGIC_DIAGNOSTIC) as i64;
        assert_eq!(decode_tag(diagnostic), None);
        assert!(is_diagnostic(diagnostic, 0x1234));
        assert!(!is_diagnostic(diagnostic, 0x4321));
    }

    #[test]
    fn permission_gate_requires_every_permission() {
        assert!(!Preflight::default().complete());
        let partial = Preflight {
            accessibility: true,
            listen: true,
            post: false,
        };
        assert!(!partial.complete());
        let full = Preflight {
            accessibility: true,
            listen: true,
            post: true,
        };
        assert!(full.complete());
    }
}
