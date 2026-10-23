//! Windows adapter: low-level mouse hook, sleeping emitter and independent guard.
//!
//! The hook thread owns all native input decisions. The emitter owns the engine
//! and the only `SendInput` caller. The guard only observes and latches faults;
//! it never produces input.

use crate::cli::RunOptions;
use crate::core::{Engine, PixelGrid, Point};
use crate::platform::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE};
use crate::runtime::{
    self, FaultCode, Mapping, Metrics, MetricsSnapshot, Msg, ReportMeta, Shared, State,
};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc};

/// Hook thread reports whether the tap installed; the host waits on it.
type ReadySender = Sender<Result<(), String>>;
type ReadyReceiver = Receiver<Result<(), String>>;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, FILETIME, GetLastError, HANDLE, HWND, LPARAM, LRESULT,
    POINT, RECT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    CancelWaitableTimer, CreateEventW, CreateMutexW, CreateWaitableTimerExW, GetCurrentProcess,
    GetProcessTimes, GetThreadTimes, SetEvent, SetWaitableTimer, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_MOUSE, MOUSEINPUT, RegisterHotKey, SendInput, UnregisterHotKey,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    MSG, MSLLHOOKSTRUCT, MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW,
    RegisterClassW, SetWindowsHookExW, SystemParametersInfoW, TranslateMessage,
    UnhookWindowsHookEx, UnregisterClassW, WNDCLASSW,
};
use windows_sys::core::BOOL;

// Locally defined constants keep the adapter independent of optional bindings.
const WH_MOUSE_LL: i32 = 14;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_HOTKEY: u32 = 0x0312;
const WM_INPUT: u32 = 0x00FF;
const WM_DISPLAYCHANGE: u32 = 0x007E;
const WM_POWERBROADCAST: u32 = 0x0218;
const WM_QUIT: u32 = 0x0012;
const WM_APP: u32 = 0x8000;
const WM_APP_TOGGLE: u32 = WM_APP + 1;
const WM_APP_DISABLE: u32 = WM_APP + 2;
const PBT_APMSUSPEND: u64 = 0x0004;
const LLMHF_INJECTED: u32 = 0x0001;
const LLMHF_LOWER_IL_INJECTED: u32 = 0x0002;
const INPUT_MOUSE_TYPE: u32 = INPUT_MOUSE;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_NOREPEAT: u32 = 0x4000;
const VK_ESCAPE: u32 = 0x1B;
const VK_M: u32 = 0x4D;
const HOTKEY_TOGGLE: i32 = 1;
const HOTKEY_EMERGENCY: i32 = 2;
const SPI_GETMOUSESPEED: u32 = 0x0070;
const TIMER_ALL_ACCESS: u32 = 0x1F0003;
const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x00000002;
const INFINITE: u32 = 0xFFFF_FFFF;
const QS_ALLINPUT_MASK: u32 = 0x04FF;
const MWMO_INPUTAVAILABLE_FLAG: u32 = 0x0004;
const PM_REMOVE_MSG: u32 = 0x0001;
const RIDEV_INPUTSINK_FLAG: u32 = 0x0000_0100;
const RIM_TYPE_MOUSE: u32 = 0;
const HWND_MESSAGE_PARENT: isize = -3;
const VK_LBUTTON: i32 = 0x01;
const VK_RBUTTON: i32 = 0x02;
const VK_MBUTTON: i32 = 0x04;
const VK_XBUTTON1: i32 = 0x05;
const VK_XBUTTON2: i32 = 0x06;

const MAGIC_OWN: u64 = 0xC5;
const MAGIC_DIAGNOSTIC: u64 = 0xD3;
const BUTTON_LEFT: u32 = 1;
const BUTTON_RIGHT: u32 = 2;
const BUTTON_MIDDLE: u32 = 4;
const BUTTON_X1: u32 = 8;
const BUTTON_X2: u32 = 16;
const TRACE_CAPACITY: usize = 8192;
const TRACE_STRIDE: usize = 6;

const TRACE_NATIVE: u64 = 1;
const TRACE_OWN_ACK: u64 = 2;
const TRACE_OWN_STALE: u64 = 3;
const TRACE_FOREIGN: u64 = 4;
const TRACE_BARRIER: u64 = 5;
const TRACE_OUTPUT: u64 = 6;
const TRACE_STATE: u64 = 7;

static CONTEXT: AtomicPtr<HookContext> = AtomicPtr::new(null_mut());
static CONSOLE_STOP_EVENT: AtomicUsize = AtomicUsize::new(0);
static CONSOLE_STOPPED: AtomicBool = AtomicBool::new(false);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Normal,
    Observe,
    SourceProbe,
    FaultCheck,
    ReplayOutput,
}

impl Scenario {
    fn kind(self) -> &'static str {
        match self {
            Scenario::Normal => "run",
            Scenario::Observe => "observe",
            Scenario::SourceProbe => "source-probe",
            Scenario::FaultCheck => "fault-check",
            Scenario::ReplayOutput => "replay-output",
        }
    }
}

pub struct SessionConfig {
    pub scenario: Scenario,
    pub duration: Option<Duration>,
    pub report: Option<PathBuf>,
    pub mapping: Mapping,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            scenario: Scenario::Normal,
            duration: None,
            report: None,
            mapping: Mapping::RelativeToCommitted,
        }
    }
}

/// Fixed-size lock-free trace used by the diagnostic companion.
pub struct TraceRing {
    slots: Box<[AtomicU64]>,
    next: AtomicUsize,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TraceRecord {
    pub kind: u64,
    pub at_ns: u64,
    pub values: [f64; 4],
}

impl TraceRing {
    fn new(capacity: usize) -> Self {
        let slots = (0..capacity * TRACE_STRIDE)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            next: AtomicUsize::new(0),
            capacity,
        }
    }

    fn record(&self, kind: u64, at_ns: u64, values: [f64; 4]) {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.capacity;
        let base = index * TRACE_STRIDE;
        self.slots[base].store(kind, Ordering::Relaxed);
        self.slots[base + 1].store(at_ns, Ordering::Relaxed);
        for (offset, value) in values.iter().enumerate() {
            self.slots[base + 2 + offset].store(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> Vec<TraceRecord> {
        let written = self.next.load(Ordering::Acquire).min(self.capacity);
        let mut records = Vec::with_capacity(written);
        for index in 0..written {
            let base = index * TRACE_STRIDE;
            records.push(TraceRecord {
                kind: self.slots[base].load(Ordering::Acquire),
                at_ns: self.slots[base + 1].load(Ordering::Acquire),
                values: [
                    f64::from_bits(self.slots[base + 2].load(Ordering::Acquire)),
                    f64::from_bits(self.slots[base + 3].load(Ordering::Acquire)),
                    f64::from_bits(self.slots[base + 4].load(Ordering::Acquire)),
                    f64::from_bits(self.slots[base + 5].load(Ordering::Acquire)),
                ],
            });
        }
        records
    }
}

/// Diagnostic-only controls. Product runs keep every flag false.
pub struct Diagnostics {
    pub allow_cursor_move: AtomicBool,
    pub inject_failure: AtomicBool,
    pub stall_ms: AtomicU64,
    pub pause_consumer: AtomicBool,
    pub remove_hook: AtomicBool,
    pub injections: AtomicU64,
    pub nonce: u64,
    input_trace: Option<TraceRing>,
    output_trace: Option<TraceRing>,
    state_trace: Option<TraceRing>,
}

impl Diagnostics {
    fn new(enabled: bool) -> Self {
        Self {
            allow_cursor_move: AtomicBool::new(false),
            inject_failure: AtomicBool::new(false),
            stall_ms: AtomicU64::new(0),
            pause_consumer: AtomicBool::new(false),
            remove_hook: AtomicBool::new(false),
            injections: AtomicU64::new(0),
            nonce: diagnostic_nonce(),
            input_trace: enabled.then(|| TraceRing::new(TRACE_CAPACITY)),
            output_trace: enabled.then(|| TraceRing::new(TRACE_CAPACITY)),
            state_trace: enabled.then(|| TraceRing::new(TRACE_CAPACITY)),
        }
    }

    fn record_input(&self, kind: u64, at_ns: u64, values: [f64; 4]) {
        if let Some(trace) = &self.input_trace {
            trace.record(kind, at_ns, values);
        }
    }

    fn record_output(&self, at_ns: u64, values: [f64; 4]) {
        if let Some(trace) = &self.output_trace {
            trace.record(TRACE_OUTPUT, at_ns, values);
        }
    }

    fn record_state(&self, at_ns: u64, state: State, fault: FaultCode) {
        if let Some(trace) = &self.state_trace {
            trace.record(
                TRACE_STATE,
                at_ns,
                [state as u32 as f64, fault as u32 as f64, 0.0, 0.0],
            );
        }
    }

    pub fn input_trace(&self) -> Vec<TraceRecord> {
        self.input_trace
            .as_ref()
            .map_or_else(Vec::new, |t| t.snapshot())
    }

    pub fn output_trace(&self) -> Vec<TraceRecord> {
        self.output_trace
            .as_ref()
            .map_or_else(Vec::new, |t| t.snapshot())
    }

    pub fn state_trace(&self) -> Vec<TraceRecord> {
        self.state_trace
            .as_ref()
            .map_or_else(Vec::new, |t| t.snapshot())
    }
}

fn diagnostic_nonce() -> u64 {
    let clock = Clock::new();
    (clock.now_ns() as u32 as u64) << 16 | std::process::id() as u64
}

struct Clock {
    freq: i64,
}

impl Clock {
    fn new() -> Self {
        let mut freq = 0_i64;
        unsafe { QueryPerformanceFrequency(&mut freq) };
        Self { freq: freq.max(1) }
    }

    fn now_ns(&self) -> u64 {
        let mut counter = 0_i64;
        unsafe { QueryPerformanceCounter(&mut counter) };
        ((counter as u128 * 1_000_000_000_u128) / self.freq as u128) as u64
    }
}

struct Native {
    _instance: usize,
    stop: usize,
    state_event: usize,
    work: usize,
    timer: usize,
}

impl Native {
    fn handle(&self, raw: usize) -> HANDLE {
        raw as HANDLE
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        for raw in [self.stop, self.state_event, self.work, self.timer] {
            if raw != 0 {
                unsafe { CloseHandle(raw as HANDLE) };
            }
        }
        if self._instance != 0 {
            unsafe { CloseHandle(self._instance as HANDLE) };
        }
    }
}

struct GridState {
    origin_x: AtomicU64,
    origin_y: AtomicU64,
    width: AtomicU64,
    height: AtomicU64,
    generation: AtomicU64,
}

impl GridState {
    fn new(grid: PixelGrid) -> Self {
        Self {
            origin_x: AtomicU64::new(grid.origin.x as i64 as u64),
            origin_y: AtomicU64::new(grid.origin.y as i64 as u64),
            width: AtomicU64::new(grid.size.x as i64 as u64),
            height: AtomicU64::new(grid.size.y as i64 as u64),
            generation: AtomicU64::new(1),
        }
    }

    fn update(&self, grid: PixelGrid) {
        self.origin_x
            .store(grid.origin.x as i64 as u64, Ordering::Release);
        self.origin_y
            .store(grid.origin.y as i64 as u64, Ordering::Release);
        self.width
            .store(grid.size.x as i64 as u64, Ordering::Release);
        self.height
            .store(grid.size.y as i64 as u64, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn snapshot(&self) -> (PixelGrid, u64) {
        (
            PixelGrid::new(
                Point::new(
                    self.origin_x.load(Ordering::Acquire) as i64 as f64,
                    self.origin_y.load(Ordering::Acquire) as i64 as f64,
                ),
                Point::new(
                    self.width.load(Ordering::Acquire) as i64 as f64,
                    self.height.load(Ordering::Acquire) as i64 as f64,
                ),
            ),
            self.generation.load(Ordering::Acquire),
        )
    }
}

fn enable_dpi_awareness() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 }
}

struct MonitorCollector {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    count: u32,
}

impl MonitorCollector {
    fn new() -> Self {
        Self {
            min_x: i32::MAX,
            min_y: i32::MAX,
            max_x: i32::MIN,
            max_y: i32::MIN,
            count: 0,
        }
    }

    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let collector = unsafe { &mut *(data as *mut MonitorCollector) };
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
            collector.count += 1;
            collector.min_x = collector.min_x.min(info.rcMonitor.left);
            collector.min_y = collector.min_y.min(info.rcMonitor.top);
            collector.max_x = collector.max_x.max(info.rcMonitor.right);
            collector.max_y = collector.max_y.max(info.rcMonitor.bottom);
        }
        1
    }

    fn grid(&self) -> Option<PixelGrid> {
        if self.count == 0 || self.max_x <= self.min_x || self.max_y <= self.min_y {
            return None;
        }
        Some(PixelGrid::new(
            Point::new(self.min_x as f64, self.min_y as f64),
            Point::new(
                (self.max_x - self.min_x) as f64,
                (self.max_y - self.min_y) as f64,
            ),
        ))
    }
}

fn virtual_grid() -> PixelGrid {
    let mut collector = MonitorCollector::new();
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(MonitorCollector::callback),
            &mut collector as *mut _ as LPARAM,
        );
    }
    collector
        .grid()
        .unwrap_or_else(|| PixelGrid::new(Point::new(0.0, 0.0), Point::new(1920.0, 1080.0)))
}

fn os_description() -> String {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfo) -> i32;
    unsafe {
        let name = wide("ntdll.dll");
        let library = LoadLibraryW(name.as_ptr());
        if library.is_null() {
            return "Windows (build unknown)".to_string();
        }
        let symbol = GetProcAddress(library, c"RtlGetVersion".as_ptr() as *const u8);
        let Some(symbol) = symbol else {
            return "Windows (build unknown)".to_string();
        };
        let function: RtlGetVersion = std::mem::transmute(symbol);
        let mut info: OsVersionInfo = std::mem::zeroed();
        info.size = std::mem::size_of::<OsVersionInfo>() as u32;
        if function(&mut info) == 0 {
            format!("Windows {}.{}.{}", info.major, info.minor, info.build)
        } else {
            "Windows (build unknown)".to_string()
        }
    }
}

fn cpu_name() -> String {
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown CPU".to_string())
}

fn logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|value| value.get() as u32)
        .unwrap_or(1)
}

fn process_cpu_ms() -> (u64, u64) {
    unsafe {
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        if GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return (0, 0);
        }
        let to_ms = |time: &FILETIME| -> u64 {
            (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) / 10_000
        };
        (to_ms(&user), to_ms(&kernel))
    }
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } != 0 {
        Some((point.x, point.y))
    } else {
        None
    }
}

pub fn set_cursor(x: i32, y: i32) -> bool {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y) != 0 }
}

// Windows delivers only the low 32 bits of `SendInput`'s dwExtraInfo to
// low-level mouse hooks (measured: 0x1122334455667788 arrives as 0x55667788),
// so the tag must fit in 16 bits of nonce, 8 bits of generation and the magic.
fn own_extra(nonce: u64, generation: u64, magic: u64) -> usize {
    (((nonce & 0xFFFF) << 16) | ((generation & 0xFF) << 8) | (magic & 0xFF)) as usize
}

enum ExtraKind {
    Own(u64),
    Diagnostic,
    Foreign,
}

fn classify_extra(extra: usize, nonce: u64) -> ExtraKind {
    let value = extra as u64;
    if (value >> 16) & 0xFFFF != nonce & 0xFFFF {
        return ExtraKind::Foreign;
    }
    match value & 0xFF {
        MAGIC_OWN => ExtraKind::Own((value >> 8) & 0xFF),
        MAGIC_DIAGNOSTIC => ExtraKind::Diagnostic,
        _ => ExtraKind::Foreign,
    }
}

fn buttons_down_now() -> u32 {
    let mut mask = 0;
    for (key, bit) in [
        (VK_LBUTTON, BUTTON_LEFT),
        (VK_RBUTTON, BUTTON_RIGHT),
        (VK_MBUTTON, BUTTON_MIDDLE),
        (VK_XBUTTON1, BUTTON_X1),
        (VK_XBUTTON2, BUTTON_X2),
    ] {
        if unsafe { GetAsyncKeyState(key) } as u16 & 0x8000 != 0 {
            mask |= bit;
        }
    }
    mask
}

fn normalize(x: i32, y: i32, grid: &PixelGrid) -> (i32, i32) {
    let span_x = (grid.size.x - 1.0).max(1.0);
    let span_y = (grid.size.y - 1.0).max(1.0);
    let dx = ((x as f64 - grid.origin.x) * 65535.0 / span_x).round();
    let dy = ((y as f64 - grid.origin.y) * 65535.0 / span_y).round();
    (dx.clamp(0.0, 65535.0) as i32, dy.clamp(0.0, 65535.0) as i32)
}

fn send_move(x: i32, y: i32, extra: usize, grid: &PixelGrid) -> bool {
    let (dx, dy) = normalize(x, y, grid);
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE_TYPE;
    input.Anonymous.mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: extra,
    };
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 1 }
}

/// Injects a relative diagnostic move (diagnostic builds treat it as native).
pub fn inject_relative(dx: i32, dy: i32, nonce: u64) -> bool {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE_TYPE;
    input.Anonymous.mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: own_extra(nonce, 0, MAGIC_DIAGNOSTIC),
    };
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 1 }
}

/// Injects a diagnostic button transition used by the fault drills.
pub fn inject_button(down: bool, nonce: u64) -> bool {
    const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE_TYPE;
    input.Anonymous.mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: 0,
        dwFlags: if down {
            MOUSEEVENTF_LEFTDOWN
        } else {
            MOUSEEVENTF_LEFTUP
        },
        time: 0,
        dwExtraInfo: own_extra(nonce, 0, MAGIC_DIAGNOSTIC),
    };
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 1 }
}

/// CPU time of the calling thread (observer/generator accounting).
pub fn thread_cpu_ms() -> (u64, u64) {
    unsafe {
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let thread = windows_sys::Win32::System::Threading::GetCurrentThread();
        if GetThreadTimes(thread, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return (0, 0);
        }
        let to_ms = |time: &FILETIME| -> u64 {
            (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) / 10_000
        };
        (to_ms(&user), to_ms(&kernel))
    }
}

/// Monotonic timestamp on the same clock the adapter uses internally.
pub fn monotonic_ns() -> u64 {
    Clock::new().now_ns()
}

pub fn process_cpu() -> (u64, u64) {
    process_cpu_ms()
}

pub fn platform_description() -> (String, String, u32) {
    (os_description(), cpu_name(), logical_cpus())
}

pub fn pointer_speed() -> u32 {
    let mut speed = 0_u32;
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSESPEED,
            0,
            &mut speed as *mut u32 as *mut c_void,
            0,
        );
    }
    speed
}

struct HookContext {
    shared: Arc<Shared>,
    diag: Arc<Diagnostics>,
    grid: Arc<GridState>,
    clock: Clock,
    producer: rtrb::Producer<Msg>,
    mapping: Mapping,
    committed: (i32, i32),
    buttons_down: u32,
    state_event: usize,
    work_event: usize,
    hook: isize,
    hooked: bool,
    raw_buffer: [u8; 256],
}

impl HookContext {
    fn now(&self) -> u64 {
        self.clock.now_ns()
    }

    fn push(&mut self, message: Msg) -> bool {
        let pushed = runtime::push(&mut self.producer, message);
        if pushed {
            unsafe { SetEvent(self.work_event as HANDLE) };
        }
        pushed
    }

    fn anchor(&mut self) -> Point {
        let (x, y) = self.shared.committed();
        let (x, y) = if x == 0 && y == 0 {
            cursor_position().unwrap_or((x, y))
        } else {
            (x, y)
        };
        self.committed = (x, y);
        Point::new(x as f64, y as f64)
    }

    fn set_state(&self, state: State) {
        self.shared.set_state(state);
        self.shared.set_pending(false);
        unsafe { SetEvent(self.native_state_event()) };
        self.diag
            .record_state(self.now(), state, self.shared.fault());
    }

    fn native_state_event(&self) -> HANDLE {
        self.state_event as HANDLE
    }

    fn begin_warming(&mut self) {
        self.shared.clear_fault();
        self.shared.set_warmed(false);
        self.shared.invalidate();
        let anchor = self.anchor();
        let now = self.now();
        let _ = self.push(Msg::Barrier { at_ns: now, anchor });
        self.buttons_down = buttons_down_now();
        self.committed = (anchor.x as i32, anchor.y as i32);
        self.shared
            .set_committed(self.committed.0, self.committed.1);
        self.shared
            .set_displayed(self.committed.0, self.committed.1);
        let state = if self.buttons_down != 0 {
            State::ButtonsPassthrough
        } else {
            State::Warming
        };
        self.set_state(state);
        self.diag
            .record_input(TRACE_BARRIER, now, [anchor.x, anchor.y, 1.0, 0.0]);
    }

    fn disable(&mut self) {
        self.shared.invalidate();
        let anchor = self.anchor();
        let now = self.now();
        let _ = self.push(Msg::Barrier { at_ns: now, anchor });
        self.set_state(State::Off);
        self.diag
            .record_input(TRACE_BARRIER, now, [anchor.x, anchor.y, 0.0, 0.0]);
    }

    fn emergency(&mut self) {
        suspend(&self.shared, &self.diag, &self.clock, FaultCode::Emergency);
        let anchor = self.anchor();
        let now = self.now();
        let _ = self.push(Msg::Barrier { at_ns: now, anchor });
        self.diag
            .record_state(now, State::Suspended, FaultCode::Emergency);
        unsafe { SetEvent(self.native_state_event()) };
    }

    unsafe fn on_move(&mut self, info: &MSLLHOOKSTRUCT) -> LRESULT {
        let pt = (info.pt.x, info.pt.y);
        let now = self.now();
        let injected = info.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
        match classify_extra(info.dwExtraInfo, self.diag.nonce) {
            ExtraKind::Own(generation) => {
                if generation & 0xFF == self.shared.generation() & 0xFF {
                    self.committed = pt;
                    self.shared.set_committed(pt.0, pt.1);
                    self.shared.set_displayed(pt.0, pt.1);
                    self.diag.record_input(
                        TRACE_OWN_ACK,
                        now,
                        [pt.0 as f64, pt.1 as f64, generation as f64, 0.0],
                    );
                    return unsafe { CallNextHookEx(null_mut(), 0, 0, 0) };
                }
                self.diag.record_input(
                    TRACE_OWN_STALE,
                    now,
                    [pt.0 as f64, pt.1 as f64, generation as f64, 0.0],
                );
                return 1;
            }
            ExtraKind::Foreign if injected => {
                let anchor = Point::new(pt.0 as f64, pt.1 as f64);
                self.committed = pt;
                self.shared.set_committed(pt.0, pt.1);
                self.shared.set_displayed(pt.0, pt.1);
                self.shared.set_warmed(false);
                let _ = self.push(Msg::Barrier { at_ns: now, anchor });
                self.diag
                    .record_input(TRACE_FOREIGN, now, [pt.0 as f64, pt.1 as f64, 0.0, 0.0]);
                return unsafe { CallNextHookEx(null_mut(), 0, 0, 0) };
            }
            _ => {}
        }

        self.shared.note_hook_move();
        let state = self.shared.state();
        let delta = Point::new(
            (pt.0 - self.committed.0) as f64,
            (pt.1 - self.committed.1) as f64,
        );
        self.diag.record_input(
            TRACE_NATIVE,
            now,
            [
                pt.0 as f64,
                pt.1 as f64,
                self.committed.0 as f64,
                self.committed.1 as f64,
            ],
        );
        let suppress = state.suppressing() && self.buttons_down == 0;
        if suppress && (delta.x != 0.0 || delta.y != 0.0) {
            let value = match self.mapping {
                Mapping::RelativeToCommitted => delta,
                Mapping::IndependentAbsolute => Point::new(pt.0 as f64, pt.1 as f64),
            };
            let pushed = self.push(Msg::Native { at_ns: now, value });
            if !pushed {
                suspend(
                    &self.shared,
                    &self.diag,
                    &self.clock,
                    FaultCode::QueueOverflow,
                );
                unsafe { SetEvent(self.native_state_event()) };
                self.committed = pt;
                self.shared.set_committed(pt.0, pt.1);
                return unsafe { CallNextHookEx(null_mut(), 0, 0, 0) };
            }
            self.committed = pt;
            self.shared.set_committed(pt.0, pt.1);
            return 1;
        }
        if state == State::Warming && self.buttons_down == 0 && (delta.x != 0.0 || delta.y != 0.0) {
            let _ = self.push(Msg::Native {
                at_ns: now,
                value: delta,
            });
            if self.shared.warmed() {
                self.set_state(State::Active);
            }
        }
        self.committed = pt;
        self.shared.set_committed(pt.0, pt.1);
        self.shared.set_displayed(pt.0, pt.1);
        unsafe { CallNextHookEx(null_mut(), 0, 0, 0) }
    }

    fn button_bit(message: u32, info: &MSLLHOOKSTRUCT) -> u32 {
        match message {
            WM_LBUTTONDOWN | WM_LBUTTONUP => BUTTON_LEFT,
            WM_RBUTTONDOWN | WM_RBUTTONUP => BUTTON_RIGHT,
            WM_MBUTTONDOWN | WM_MBUTTONUP => BUTTON_MIDDLE,
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if (info.mouseData >> 16) as u16 == 1 {
                    BUTTON_X1
                } else {
                    BUTTON_X2
                }
            }
            _ => 0,
        }
    }

    unsafe fn on_button(&mut self, message: u32, info: &MSLLHOOKSTRUCT, down: bool) -> LRESULT {
        let bit = Self::button_bit(message, info);
        let pt = (info.pt.x, info.pt.y);
        let now = self.now();
        if down {
            self.buttons_down |= bit;
            let was_active = self.shared.suppressing() || self.shared.state() == State::Warming;
            if was_active {
                self.shared.invalidate();
                let anchor = Point::new(pt.0 as f64, pt.1 as f64);
                let _ = self.push(Msg::Barrier { at_ns: now, anchor });
                self.set_state(State::ButtonsPassthrough);
                self.diag
                    .record_input(TRACE_BARRIER, now, [anchor.x, anchor.y, 2.0, 0.0]);
            }
        } else {
            self.buttons_down &= !bit;
            if self.buttons_down == 0 && self.shared.state() == State::ButtonsPassthrough {
                self.begin_warming();
            }
        }
        self.committed = pt;
        self.shared.set_committed(pt.0, pt.1);
        self.shared.set_displayed(pt.0, pt.1);
        unsafe { CallNextHookEx(null_mut(), 0, 0, 0) }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    }
    let pointer = CONTEXT.load(Ordering::Acquire);
    if pointer.is_null() {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    }
    let context = unsafe { &mut *pointer };
    let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    let message = wparam as u32;
    match message {
        WM_MOUSEMOVE => unsafe { context.on_move(info) },
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => unsafe {
            context.on_button(message, info, true)
        },
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => unsafe {
            context.on_button(message, info, false)
        },
        _ => unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) },
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = CONTEXT.load(Ordering::Acquire);
    match message {
        WM_HOTKEY => {
            if !pointer.is_null() {
                let context = unsafe { &mut *pointer };
                match wparam as i32 {
                    HOTKEY_TOGGLE => {
                        let state = context.shared.state();
                        match state {
                            State::Off | State::Suspended => context.begin_warming(),
                            _ => context.disable(),
                        }
                    }
                    HOTKEY_EMERGENCY => context.emergency(),
                    _ => {}
                }
            }
            0
        }
        WM_INPUT => {
            if !pointer.is_null() {
                let context = unsafe { &mut *pointer };
                let mut size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
                let mut header: RAWINPUTHEADER = unsafe { std::mem::zeroed() };
                let read = unsafe {
                    GetRawInputData(
                        lparam as _,
                        RID_INPUT,
                        &mut header as *mut _ as *mut c_void,
                        &mut size,
                        std::mem::size_of::<RAWINPUTHEADER>() as u32,
                    )
                };
                if read != u32::MAX && header.dwType == RIM_TYPE_MOUSE {
                    context.shared.note_raw_move();
                }
                let mut size = context.raw_buffer.len() as u32;
                unsafe {
                    GetRawInputData(
                        lparam as _,
                        RID_INPUT,
                        context.raw_buffer.as_mut_ptr() as *mut c_void,
                        &mut size,
                        std::mem::size_of::<RAWINPUTHEADER>() as u32,
                    );
                }
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_DISPLAYCHANGE => {
            if !pointer.is_null() {
                let context = unsafe { &mut *pointer };
                context.grid.update(virtual_grid());
                if context.shared.suppressing() {
                    suspend(
                        &context.shared,
                        &context.diag,
                        &context.clock,
                        FaultCode::DisplayChanged,
                    );
                    unsafe { SetEvent(context.native_state_event()) };
                }
            }
            0
        }
        WM_POWERBROADCAST => {
            if !pointer.is_null() && (wparam as u64) & 0xFFFF_FFFF == PBT_APMSUSPEND {
                let context = unsafe { &mut *pointer };
                suspend(
                    &context.shared,
                    &context.diag,
                    &context.clock,
                    FaultCode::Sleep,
                );
                unsafe { SetEvent(context.native_state_event()) };
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_APP_TOGGLE => {
            if !pointer.is_null() {
                let context = unsafe { &mut *pointer };
                let state = context.shared.state();
                match state {
                    State::Off | State::Suspended => context.begin_warming(),
                    _ => context.disable(),
                }
            }
            0
        }
        WM_APP_DISABLE => {
            if !pointer.is_null() {
                let context = unsafe { &mut *pointer };
                context.disable();
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn suspend(shared: &Shared, diag: &Diagnostics, clock: &Clock, code: FaultCode) {
    shared.suspend(code);
    diag.record_state(clock.now_ns(), State::Suspended, code);
}

/// Diagnostic: cadence of the same high-resolution periodic timer the emitter
/// uses, with no work in the loop. Separates OS timer cost from engine cost.
pub fn measure_timer_cadence(seconds: u64) -> u32 {
    let timer = unsafe {
        CreateWaitableTimerExW(
            null_mut(),
            null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if timer.is_null() {
        return 0;
    }
    let mut wakes = 0_u32;
    if arm_timer(timer) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if unsafe { WaitForSingleObject(timer, 1_000) } == 0 {
                wakes += 1;
            }
        }
    }
    // One-shot re-armed every wake, in case the periodic timer is coalesced.
    let mut oneshot = 0_u32;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let due = -10_000_i64;
    while Instant::now() < deadline {
        if unsafe { SetWaitableTimer(timer, &due, 0, None, null(), 0) } != 0
            && unsafe { WaitForSingleObject(timer, 1_000) } == 0
        {
            oneshot += 1;
        }
    }
    eprintln!("timer cadence: periodic={wakes} oneshot={oneshot}");
    cancel_timer(timer);
    unsafe { CloseHandle(timer) };
    wakes
}

fn arm_timer(timer: HANDLE) -> bool {
    let due = -10_000_i64;
    unsafe { SetWaitableTimer(timer, &due, 1, None, null(), 0) != 0 }
}

fn cancel_timer(timer: HANDLE) {
    unsafe {
        CancelWaitableTimer(timer);
        WaitForSingleObject(timer, 0);
    }
}

struct EmitterContext {
    shared: Arc<Shared>,
    metrics: Arc<Metrics>,
    diag: Arc<Diagnostics>,
    grid: Arc<GridState>,
    native: Arc<Native>,
    clock: Clock,
    engine: Engine,
    mapping: Mapping,
    last_output: Option<(i32, i32)>,
    last_output_ns: u64,
    timer_armed: bool,
    deadline_ns: u64,
}

impl EmitterContext {
    fn drain(&mut self, consumer: &mut rtrb::Consumer<Msg>) {
        while let Ok(message) = consumer.pop() {
            if matches!(message, Msg::Stop) {
                self.shared.request_stop();
                break;
            }
            self.metrics.input_events.fetch_add(1, Ordering::AcqRel);
            if matches!(message, Msg::Native { .. }) {
                self.metrics.native_moves.fetch_add(1, Ordering::AcqRel);
            }
            runtime::apply_message(&mut self.engine, self.mapping, message);
        }
        let warmed = self.engine.is_warmed();
        self.shared.set_warmed(warmed);
        if warmed && self.shared.state() == State::Warming {
            self.shared.set_state(State::Active);
            self.diag
                .record_state(self.clock.now_ns(), State::Active, FaultCode::None);
            unsafe { SetEvent(self.native.handle(self.native.state_event)) };
        }
        let pending = self.shared.suppressing() && self.engine.has_pending();
        self.shared.set_pending(pending);
        self.shared.heartbeat(self.clock.now_ns());
        if pending {
            if !self.timer_armed {
                if arm_timer(self.native.handle(self.native.timer)) {
                    self.timer_armed = true;
                    self.deadline_ns = self.clock.now_ns() + runtime::OUTPUT_INTERVAL_NS;
                    self.metrics.burst_count.fetch_add(1, Ordering::AcqRel);
                } else {
                    suspend(
                        &self.shared,
                        &self.diag,
                        &self.clock,
                        FaultCode::EmitterStall,
                    );
                }
            }
        } else if self.timer_armed {
            cancel_timer(self.native.handle(self.native.timer));
            self.timer_armed = false;
        }
    }

    fn tick(&mut self) {
        if !self.shared.suppressing() {
            return;
        }
        let now = self.clock.now_ns();
        self.metrics.output_ticks.fetch_add(1, Ordering::AcqRel);
        if !self.shared.suppressing() {
            self.metrics.idle_wakes.fetch_add(1, Ordering::AcqRel);
            if self.timer_armed {
                cancel_timer(self.native.handle(self.native.timer));
                self.timer_armed = false;
            }
            return;
        }
        let (grid, _) = self.grid.snapshot();
        match self.engine.sample(now) {
            Some(target) => {
                let (x, y) = grid.quantize(target);
                if self.last_output != Some((x, y)) {
                    if self.diag.inject_failure.load(Ordering::Acquire) {
                        suspend(
                            &self.shared,
                            &self.diag,
                            &self.clock,
                            FaultCode::InjectionFailed,
                        );
                    } else {
                        let extra = own_extra(self.diag.nonce, self.shared.generation(), MAGIC_OWN);
                        if send_move(x, y, extra, &grid) {
                            self.metrics.emitted.fetch_add(1, Ordering::AcqRel);
                            self.diag
                                .record_output(now, [x as f64, y as f64, target.x, target.y]);
                            if self.last_output_ns != 0 {
                                self.metrics.note_gap(now - self.last_output_ns);
                            }
                            self.last_output_ns = now;
                            self.last_output = Some((x, y));
                            self.shared.set_displayed(x, y);
                        } else {
                            suspend(
                                &self.shared,
                                &self.diag,
                                &self.clock,
                                FaultCode::InjectionFailed,
                            );
                        }
                    }
                } else {
                    self.metrics
                        .skipped_identical
                        .fetch_add(1, Ordering::AcqRel);
                }
            }
            None => {
                self.shared.set_pending(false);
                if self.timer_armed {
                    cancel_timer(self.native.handle(self.native.timer));
                    self.timer_armed = false;
                }
            }
        }
        if self.deadline_ns != 0 {
            let late = now.saturating_sub(self.deadline_ns);
            let slots = late / runtime::OUTPUT_INTERVAL_NS;
            if slots > 1 {
                self.metrics
                    .missed_deadlines
                    .fetch_add(slots - 1, Ordering::AcqRel);
                self.deadline_ns = now;
            } else {
                self.deadline_ns += runtime::OUTPUT_INTERVAL_NS;
            }
            self.metrics.record_lateness(late);
        }
        self.shared.heartbeat(now);
        if !self.engine.has_pending() {
            self.shared.set_pending(false);
            if self.timer_armed {
                cancel_timer(self.native.handle(self.native.timer));
                self.timer_armed = false;
            }
        }
    }
}

fn emitter_thread(mut context: EmitterContext, mut consumer: rtrb::Consumer<Msg>) {
    loop {
        let stall = context.diag.stall_ms.swap(0, Ordering::AcqRel);
        if stall > 0 {
            thread::sleep(Duration::from_millis(stall));
        }
        let handles = [
            context.native.handle(context.native.timer),
            context.native.handle(context.native.work),
            context.native.handle(context.native.stop),
        ];
        let result = unsafe { WaitForMultipleObjects(3, handles.as_ptr(), 0, INFINITE) };
        match result {
            WAIT_OBJECT_0 => {
                context.metrics.timer_wakes.fetch_add(1, Ordering::AcqRel);
                if !context.shared.suppressing() {
                    context.metrics.idle_wakes.fetch_add(1, Ordering::AcqRel);
                    cancel_timer(context.native.handle(context.native.timer));
                    context.timer_armed = false;
                    continue;
                }
                if context.diag.pause_consumer.load(Ordering::Acquire) {
                    continue;
                }
                if now_before(&context, context.deadline_ns) {
                    continue;
                }
                context.tick();
            }
            1 => {
                if context.diag.pause_consumer.load(Ordering::Acquire) {
                    continue;
                }
                context.drain(&mut consumer);
            }
            2 => break,
            WAIT_FAILED => {
                suspend(
                    &context.shared,
                    &context.diag,
                    &context.clock,
                    FaultCode::EmitterStall,
                );
                break;
            }
            _ => break,
        }
    }
}

fn now_before(context: &EmitterContext, deadline: u64) -> bool {
    deadline != 0 && context.clock.now_ns() + 200_000 < deadline
}

fn guard_thread(shared: Arc<Shared>, native: Arc<Native>, clock: Clock, diag: Arc<Diagnostics>) {
    let mut last_counts = shared.move_counts();
    let mut readback_failures = 0_u32;
    let mut iterations = 0_u32;
    loop {
        let suppressing = shared.suppressing();
        let timeout = if suppressing { 50 } else { INFINITE };
        let handles = [
            native.handle(native.state_event),
            native.handle(native.stop),
        ];
        let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, timeout) };
        match result {
            1 => break,
            258 => {}
            WAIT_FAILED => {
                suspend(&shared, &diag, &clock, FaultCode::EmitterStall);
                continue;
            }
            _ => {}
        }
        if shared.stopping() {
            break;
        }
        if !shared.suppressing() {
            last_counts = shared.move_counts();
            readback_failures = 0;
            continue;
        }
        let now = clock.now_ns();
        let heartbeat = shared.heartbeat_ns();
        if shared.pending() && now.saturating_sub(heartbeat) > runtime::HEARTBEAT_LIMIT_NS {
            suspend(&shared, &diag, &clock, FaultCode::EmitterStall);
            continue;
        }
        let counts = shared.move_counts();
        if counts.0 > last_counts.0 && counts.1 == last_counts.1 {
            suspend(&shared, &diag, &clock, FaultCode::HookMissing);
            continue;
        }
        last_counts = counts;
        if shared.fault() == FaultCode::None && shared.state().suppressing() {
            iterations = iterations.wrapping_add(1);
            {
                match cursor_position() {
                    Some((x, y)) => {
                        let (dx, dy) = shared.displayed();
                        let distance = ((x - dx).abs()).max((y - dy).abs());
                        if distance > 8 {
                            // Motion we did not produce means the hook stopped
                            // seeing native moves; motion right after our own
                            // emission means the emission did not apply.
                            let last_emit = shared.emit_ns();
                            let quiet_for = now.saturating_sub(last_emit);
                            readback_failures += 1;
                            if readback_failures >= 2 {
                                let cause = if last_emit == 0 || quiet_for > 100_000_000 {
                                    FaultCode::HookMissing
                                } else {
                                    FaultCode::CursorTeleport
                                };
                                suspend(&shared, &diag, &clock, cause);
                            }
                        } else {
                            readback_failures = 0;
                        }
                    }
                    None => {
                        suspend(&shared, &diag, &clock, FaultCode::CursorReadFailed);
                    }
                }
            }
        } else {
            readback_failures = 0;
        }
    }
}

pub struct Session {
    pub shared: Arc<Shared>,
    pub metrics: Arc<Metrics>,
    pub diagnostics: Arc<Diagnostics>,
    grid: Arc<GridState>,
    native: Arc<Native>,
    hook_thread_id: Arc<AtomicU32>,
    emitter: Option<JoinHandle<()>>,
    guard: Option<JoinHandle<()>>,
    hook_thread: Option<JoinHandle<()>>,
    scenario: Scenario,
    report: Option<PathBuf>,
    wall_start: Instant,
    cpu_start: (u64, u64),
    _mutex: Arc<Native>,
    clock: Clock,
}

impl Session {
    pub fn start(config: SessionConfig) -> Result<Session, String> {
        enable_dpi_awareness();
        let instance = unsafe {
            let name = wide("Local\\cursor-smooth-session");
            CreateMutexW(null_mut(), 1, name.as_ptr())
        };
        if instance.is_null() {
            return Err(format!("cannot create instance mutex: {}", last_error()));
        }
        if last_error() == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(instance) };
            return Err("another cursor-smooth session is already running".to_string());
        }
        if let Some(path) = &config.report {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {}
                Err(error) => {
                    unsafe { CloseHandle(instance) };
                    return Err(format!("cannot create report {}: {error}", path.display()));
                }
            }
        }

        let clock = Clock::new();
        // Manual reset: the hook, emitter and guard all wait on this one handle.
        let stop = unsafe { CreateEventW(null_mut(), 1, 0, null()) };
        let state_event = unsafe { CreateEventW(null_mut(), 0, 0, null()) };
        let work = unsafe { CreateEventW(null_mut(), 0, 0, null()) };
        let timer = unsafe {
            CreateWaitableTimerExW(
                null_mut(),
                null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if stop.is_null() || state_event.is_null() || work.is_null() || timer.is_null() {
            unsafe { CloseHandle(instance) };
            return Err(format!("cannot create native events: {}", last_error()));
        }
        let native = Arc::new(Native {
            _instance: instance as usize,
            stop: stop as usize,
            state_event: state_event as usize,
            work: work as usize,
            timer: timer as usize,
        });

        let shared = Arc::new(Shared::new());
        let metrics = Arc::new(Metrics::new());
        let diagnostics = Arc::new(Diagnostics::new(cfg!(feature = "diagnostics")));
        let grid = Arc::new(GridState::new(virtual_grid()));
        let queue = runtime::make_queue();
        let hook_thread_id = Arc::new(AtomicU32::new(0));

        let (ready_tx, ready_rx): (ReadySender, ReadyReceiver) = mpsc::channel();
        let auto_start = matches!(config.scenario, Scenario::Normal);

        let hook_context = HookSetup {
            shared: Arc::clone(&shared),
            diag: Arc::clone(&diagnostics),
            grid: Arc::clone(&grid),
            native: Arc::clone(&native),
            clock: Clock { freq: clock.freq },
            producer: queue.producer,
            mapping: config.mapping,
            thread_id: Arc::clone(&hook_thread_id),
            ready: ready_tx,
            auto_start,
        };
        let emitter_context = EmitterContext {
            shared: Arc::clone(&shared),
            metrics: Arc::clone(&metrics),
            diag: Arc::clone(&diagnostics),
            grid: Arc::clone(&grid),
            native: Arc::clone(&native),
            clock: Clock { freq: clock.freq },
            engine: Engine::new(Point::new(0.0, 0.0)),
            mapping: config.mapping,
            last_output: None,
            last_output_ns: 0,
            timer_armed: false,
            deadline_ns: 0,
        };
        let guard_shared = Arc::clone(&shared);
        let guard_native = Arc::clone(&native);
        let guard_clock = Clock { freq: clock.freq };
        let guard_diag = Arc::clone(&diagnostics);

        CONSOLE_STOP_EVENT.store(stop as usize, Ordering::Release);
        CONSOLE_STOPPED.store(false, Ordering::Release);
        unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) };

        let emitter = thread::Builder::new()
            .name("cursor-smooth-emitter".into())
            .spawn(move || emitter_thread(emitter_context, queue.consumer))
            .map_err(|error| format!("cannot start emitter: {error}"))?;
        let guard = thread::Builder::new()
            .name("cursor-smooth-guard".into())
            .spawn(move || guard_thread(guard_shared, guard_native, guard_clock, guard_diag))
            .map_err(|error| format!("cannot start guard: {error}"))?;
        let hook_thread = thread::Builder::new()
            .name("cursor-smooth-hook".into())
            .spawn(move || hook_thread_main(hook_context))
            .map_err(|error| format!("cannot start hook thread: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                shared.request_stop();
                unsafe { SetEvent(native.handle(native.stop)) };
                let _ = hook_thread.join();
                let _ = emitter.join();
                let _ = guard.join();
                return Err(error);
            }
            Err(_) => {
                shared.request_stop();
                unsafe { SetEvent(native.handle(native.stop)) };
                let _ = hook_thread.join();
                let _ = emitter.join();
                let _ = guard.join();
                return Err("hook thread did not become ready".to_string());
            }
        }

        Ok(Session {
            shared,
            metrics,
            diagnostics,
            grid,
            native: Arc::clone(&native),
            hook_thread_id,
            emitter: Some(emitter),
            guard: Some(guard),
            hook_thread: Some(hook_thread),
            scenario: config.scenario,
            report: config.report,
            wall_start: Instant::now(),
            cpu_start: process_cpu_ms(),
            _mutex: native,
            clock,
        })
    }

    pub fn state(&self) -> State {
        self.shared.state()
    }

    /// Posts the same transition as the toggle hotkey.
    pub fn toggle(&self) {
        unsafe {
            PostThreadMessageW(
                self.hook_thread_id.load(Ordering::Acquire),
                WM_APP_TOGGLE,
                0,
                0,
            );
        }
    }

    pub fn disable(&self) {
        unsafe {
            PostThreadMessageW(
                self.hook_thread_id.load(Ordering::Acquire),
                WM_APP_DISABLE,
                0,
                0,
            );
        }
    }

    /// Session clock, shared by hook, emitter, guard and traces.
    pub fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    pub fn wait_for_state(&self, target: State, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.shared.state() == target {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        self.shared.state() == target
    }

    /// Blocks until the duration elapses, Ctrl+C, or an internal stop.
    pub fn wait(&self, duration: Option<Duration>) {
        let deadline = duration.map(|value| Instant::now() + value);
        loop {
            if self.shared.stopping() || CONSOLE_STOPPED.load(Ordering::Acquire) {
                return;
            }
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                return;
            }
            let remaining = deadline
                .map(|value| value.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_millis(100))
                .min(Duration::from_millis(100));
            unsafe {
                WaitForSingleObject(
                    self.native.handle(self.native.stop),
                    remaining.as_millis() as u32,
                );
            }
        }
    }

    pub fn finish(mut self) -> Summary {
        self.shared.invalidate();
        self.shared.set_state(State::Off);
        self.shared.request_stop();
        unsafe {
            SetEvent(self.native.handle(self.native.stop));
            PostThreadMessageW(self.hook_thread_id.load(Ordering::Acquire), WM_QUIT, 0, 0);
        }
        if let Some(handle) = self.hook_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.emitter.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.guard.take() {
            let _ = handle.join();
        }
        unsafe { SetConsoleCtrlHandler(Some(console_handler), 0) };
        CONSOLE_STOP_EVENT.store(0, Ordering::Release);

        let wall_ns = self.wall_start.elapsed().as_nanos() as u64;
        let cpu_end = process_cpu_ms();
        let endpoint_error = cursor_position()
            .map(|(x, y)| {
                let (dx, dy) = self.shared.displayed();
                ((x - dx).abs()).max((y - dy).abs()) as u64
            })
            .unwrap_or(0);
        let summary = Summary {
            metrics: self.metrics.snapshot(),
            fault: self.shared.fault(),
            wall_ns,
            user_ms: cpu_end.0.saturating_sub(self.cpu_start.0),
            kernel_ms: cpu_end.1.saturating_sub(self.cpu_start.1),
            endpoint_error_px: endpoint_error,
            input_trace: self.diagnostics.input_trace(),
            output_trace: self.diagnostics.output_trace(),
            state_trace: self.diagnostics.state_trace(),
        };
        if let Some(path) = &self.report {
            let meta = ReportMeta {
                kind: self.scenario.kind().to_string(),
                os: os_description(),
                arch: std::env::consts::ARCH.to_string(),
                cpu: cpu_name(),
                logical_cpus: logical_cpus(),
                wall_ns,
                user_ms: summary.user_ms,
                kernel_ms: summary.kernel_ms,
                endpoint_error_px: endpoint_error,
                notes: format!(
                    "tested_build={}; mapping=relative_to_committed; fault={}; logical_cpu_ms_denominator",
                    os_description(),
                    summary.fault.as_str()
                ),
            };
            if let Err(error) = runtime::write_report_over(path, &meta, &summary.metrics) {
                eprintln!("cursor-smooth: cannot write report: {error}");
            }
        }
        let _ = self.clock;
        let _ = self.grid;
        let _ = self._mutex;
        summary
    }
}

struct HookSetup {
    shared: Arc<Shared>,
    diag: Arc<Diagnostics>,
    grid: Arc<GridState>,
    native: Arc<Native>,
    clock: Clock,
    producer: rtrb::Producer<Msg>,
    mapping: Mapping,
    thread_id: Arc<AtomicU32>,
    ready: Sender<Result<(), String>>,
    auto_start: bool,
}

fn hook_thread_main(setup: HookSetup) {
    let HookSetup {
        shared,
        diag,
        grid,
        native,
        clock,
        producer,
        mapping,
        thread_id,
        ready,
        auto_start,
    } = setup;

    let instance = unsafe { GetModuleHandleW(null()) };
    let class_name = wide("CursorSmoothHiddenWindow");
    let title = wide("CursorSmooth");
    let mut window_class: WNDCLASSW = unsafe { std::mem::zeroed() };
    window_class.style = 0;
    window_class.lpfnWndProc = Some(window_proc);
    window_class.cbClsExtra = 0;
    window_class.cbWndExtra = 0;
    window_class.hInstance = instance;
    window_class.hIcon = null_mut();
    window_class.hCursor = null_mut();
    window_class.hbrBackground = null_mut();
    window_class.lpszMenuName = null();
    window_class.lpszClassName = class_name.as_ptr();

    let mut class_registered = false;
    let mut hwnd: HWND = null_mut();
    let mut hook: isize = 0;
    let mut hotkeys = [0_i32; 2];
    let mut failure: Option<String> = None;

    unsafe {
        if RegisterClassW(&window_class) == 0 {
            failure = Some(format!("RegisterClassW failed: {}", last_error()));
        }
    }
    if failure.is_none() {
        class_registered = true;
        hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE_PARENT as HWND,
                null_mut(),
                instance,
                null_mut(),
            )
        };
        if hwnd.is_null() {
            failure = Some(format!("CreateWindowExW failed: {}", last_error()));
        }
    }
    if failure.is_none() {
        if unsafe {
            RegisterHotKey(
                hwnd,
                HOTKEY_TOGGLE,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                VK_M,
            )
        } == 0
        {
            failure = Some("Ctrl+Alt+M is already registered by another program".to_string());
        } else {
            hotkeys[0] = HOTKEY_TOGGLE;
        }
    }
    if failure.is_none() {
        if unsafe {
            RegisterHotKey(
                hwnd,
                HOTKEY_EMERGENCY,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                VK_ESCAPE,
            )
        } == 0
        {
            failure = Some("Ctrl+Alt+Esc is already registered by another program".to_string());
        } else {
            hotkeys[1] = HOTKEY_EMERGENCY;
        }
    }
    if failure.is_none() {
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK_FLAG,
            hwndTarget: hwnd,
        };
        if unsafe {
            RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        } == 0
        {
            failure = Some(format!("RegisterRawInputDevices failed: {}", last_error()));
        }
    }
    if failure.is_none() {
        let installed = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0) };
        if installed.is_null() {
            failure = Some(format!("SetWindowsHookExW failed: {}", last_error()));
        } else {
            hook = installed as isize;
        }
    }

    if let Some(error) = failure {
        unsafe {
            for id in hotkeys {
                if id != 0 {
                    UnregisterHotKey(hwnd, id);
                }
            }
            if !hwnd.is_null() {
                DestroyWindow(hwnd);
            }
            if class_registered {
                UnregisterClassW(class_name.as_ptr(), instance);
            }
        }
        let _ = ready.send(Err(error));
        return;
    }

    let mut context = Box::new(HookContext {
        shared: Arc::clone(&shared),
        diag: Arc::clone(&diag),
        grid: Arc::clone(&grid),
        clock,
        producer,
        mapping,
        committed: cursor_position().unwrap_or((0, 0)),
        buttons_down: buttons_down_now(),
        state_event: native.state_event,
        work_event: native.work,
        hook,
        hooked: true,
        raw_buffer: [0; 256],
    });
    context
        .shared
        .set_committed(context.committed.0, context.committed.1);
    context
        .shared
        .set_displayed(context.committed.0, context.committed.1);
    thread_id.store(
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() },
        Ordering::Release,
    );
    CONTEXT.store(&mut *context as *mut HookContext, Ordering::Release);

    if auto_start {
        if context.buttons_down != 0 {
            context.set_state(State::ButtonsPassthrough);
        } else {
            context.begin_warming();
        }
    } else {
        context.set_state(State::Off);
    }

    let _ = ready.send(Ok(()));

    loop {
        if shared.stopping() {
            break;
        }
        if diag.remove_hook.load(Ordering::Acquire) && context.hooked {
            unsafe { UnhookWindowsHookEx(context.hook as _) };
            context.hooked = false;
            diag.remove_hook.store(false, Ordering::Release);
        }
        let mut message: MSG = unsafe { std::mem::zeroed() };
        let mut quit = false;
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE_MSG) } != 0 {
            match message.message {
                WM_QUIT => {
                    quit = true;
                    break;
                }
                WM_APP_TOGGLE => match context.shared.state() {
                    State::Off | State::Suspended => context.begin_warming(),
                    _ => context.disable(),
                },
                WM_APP_DISABLE => context.disable(),
                _ => unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }
        if quit || shared.stopping() {
            break;
        }
        let handles = [native.handle(native.stop)];
        let result = unsafe {
            MsgWaitForMultipleObjectsEx(
                1,
                handles.as_ptr(),
                INFINITE,
                QS_ALLINPUT_MASK,
                MWMO_INPUTAVAILABLE_FLAG,
            )
        };
        if result == WAIT_OBJECT_0 {
            break;
        }
        if result == WAIT_FAILED {
            shared.suspend(FaultCode::EmitterStall);
            break;
        }
    }

    shared.invalidate();
    shared.set_state(State::Off);
    shared.set_pending(false);
    CONTEXT.store(null_mut(), Ordering::Release);
    unsafe {
        if context.hooked {
            UnhookWindowsHookEx(context.hook as _);
        }
        for id in hotkeys {
            if id != 0 {
                UnregisterHotKey(hwnd, id);
            }
        }
        DestroyWindow(hwnd);
        UnregisterClassW(class_name.as_ptr(), instance);
    }
    drop(context);
}

unsafe extern "system" fn console_handler(ctrl_type: u32) -> BOOL {
    let _ = ctrl_type;
    CONSOLE_STOPPED.store(true, Ordering::Release);
    let raw = CONSOLE_STOP_EVENT.load(Ordering::Acquire);
    if raw != 0 {
        unsafe { SetEvent(raw as HANDLE) };
    }
    1
}

/// Result of one supervised run.
pub struct Summary {
    pub metrics: MetricsSnapshot,
    pub fault: FaultCode,
    pub wall_ns: u64,
    pub user_ms: u64,
    pub kernel_ms: u64,
    pub endpoint_error_px: u64,
    pub input_trace: Vec<TraceRecord>,
    pub output_trace: Vec<TraceRecord>,
    pub state_trace: Vec<TraceRecord>,
}

pub fn run(options: &RunOptions) -> i32 {
    let config = SessionConfig {
        scenario: Scenario::Normal,
        duration: options.duration,
        report: options.report.clone(),
        mapping: Mapping::RelativeToCommitted,
    };
    let session = match Session::start(config) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("cursor-smooth: {error}");
            return EXIT_FAILURE;
        }
    };
    session.wait(options.duration);
    let summary = session.finish();
    println!(
        "cursor-smooth: fault={} emitted={} missed={} idle_wakes={} user_ms={} kernel_ms={} wall_ms={}",
        summary.fault.as_str(),
        summary.metrics.emitted,
        summary.metrics.missed_deadlines,
        summary.metrics.idle_wakes,
        summary.user_ms,
        summary.kernel_ms,
        summary.wall_ns / 1_000_000
    );
    if summary.fault == FaultCode::HookInstallFailed {
        EXIT_FAILURE
    } else {
        EXIT_OK
    }
}

pub fn doctor() -> i32 {
    println!("os: {}", os_description());
    println!("arch: {}", std::env::consts::ARCH);
    println!("cpu: {}", cpu_name());
    println!("logical_cpus: {}", logical_cpus());
    let grid = virtual_grid();
    println!(
        "virtual_desktop: origin=({:.0},{:.0}) size=({:.0},{:.0})",
        grid.origin.x, grid.origin.y, grid.size.x, grid.size.y
    );
    println!(
        "dpi_awareness: {}",
        if enable_dpi_awareness() {
            "per-monitor-v2"
        } else {
            "unchanged"
        }
    );
    println!("mouse_speed_setting: {}", pointer_speed());
    let instance = unsafe {
        let name = wide("Local\\cursor-smooth-doctor");
        CreateMutexW(null_mut(), 1, name.as_ptr())
    };
    println!(
        "session_mutex: {}",
        if instance.is_null() {
            "unavailable"
        } else {
            unsafe { CloseHandle(instance) };
            "available"
        }
    );
    println!("hook: not installed (doctor is read-only)");
    println!("mapping: relative_to_committed (see source-probe evidence)");
    EXIT_OK
}

pub fn exit_usage() -> i32 {
    EXIT_USAGE
}
