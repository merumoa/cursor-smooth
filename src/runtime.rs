//! Cross-thread state, bounded channels and performance accounting.
//!
//! Every rule that keeps the tool fail-open lives here: one atomic state and
//! generation, one producer for the input queue, and metrics that are written
//! only by their owning thread.

use crate::core::{Engine, Point};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// One output scheduling opportunity per millisecond.
pub const OUTPUT_INTERVAL_NS: u64 = 1_000_000;
/// An emitter without progress for this long is considered stalled.
pub const HEARTBEAT_LIMIT_NS: u64 = 50_000_000;
/// Input queue capacity; overflow suspends instead of dropping suppressed input.
pub const QUEUE_CAPACITY: usize = 256;
/// Fixed lateness sample capacity (two output opportunities per sample at worst).
pub const LATENESS_CAPACITY: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
    Off = 0,
    Warming = 1,
    Active = 2,
    Draining = 3,
    ButtonsPassthrough = 4,
    Suspended = 5,
}

impl State {
    pub fn from_u32(value: u32) -> State {
        match value {
            1 => State::Warming,
            2 => State::Active,
            3 => State::Draining,
            4 => State::ButtonsPassthrough,
            5 => State::Suspended,
            _ => State::Off,
        }
    }

    /// Only these states may suppress native motion.
    pub fn suppressing(self) -> bool {
        matches!(self, State::Active | State::Draining)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum FaultCode {
    None = 0,
    EmitterStall = 1,
    QueueOverflow = 2,
    InjectionFailed = 3,
    HookMissing = 4,
    CursorReadFailed = 5,
    CursorTeleport = 6,
    DisplayChanged = 7,
    Sleep = 8,
    Emergency = 9,
    HookInstallFailed = 10,
}

impl FaultCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FaultCode::None => "none",
            FaultCode::EmitterStall => "emitter_stall",
            FaultCode::QueueOverflow => "queue_overflow",
            FaultCode::InjectionFailed => "injection_failed",
            FaultCode::HookMissing => "hook_missing",
            FaultCode::CursorReadFailed => "cursor_read_failed",
            FaultCode::CursorTeleport => "cursor_teleport",
            FaultCode::DisplayChanged => "display_changed",
            FaultCode::Sleep => "sleep",
            FaultCode::Emergency => "emergency",
            FaultCode::HookInstallFailed => "hook_install_failed",
        }
    }

    pub fn from_u32(value: u32) -> FaultCode {
        match value {
            1 => FaultCode::EmitterStall,
            2 => FaultCode::QueueOverflow,
            3 => FaultCode::InjectionFailed,
            4 => FaultCode::HookMissing,
            5 => FaultCode::CursorReadFailed,
            6 => FaultCode::CursorTeleport,
            7 => FaultCode::DisplayChanged,
            8 => FaultCode::Sleep,
            9 => FaultCode::Emergency,
            10 => FaultCode::HookInstallFailed,
            _ => FaultCode::None,
        }
    }
}

/// How the platform adapter derives displacement from native events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// `native_point - committed_cursor` reproduces the authoritative delta.
    RelativeToCommitted,
    /// `native_point` itself is the authoritative path.
    IndependentAbsolute,
}

/// Bounded single-producer message set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Msg {
    Native { at_ns: u64, value: Point },
    Barrier { at_ns: u64, anchor: Point },
    Stop,
}

pub struct Queue {
    pub producer: rtrb::Producer<Msg>,
    pub consumer: rtrb::Consumer<Msg>,
}

/// Creates the single preallocated input queue.
pub fn make_queue() -> Queue {
    let (producer, consumer) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
    Queue { producer, consumer }
}

/// Non-blocking push; `false` means the queue is full.
pub fn push(producer: &mut rtrb::Producer<Msg>, msg: Msg) -> bool {
    producer.push(msg).is_ok()
}

pub struct Shared {
    state: AtomicU32,
    generation: AtomicU64,
    fault: AtomicU32,
    heartbeat_ns: AtomicU64,
    emit_ns: AtomicU64,
    pending: AtomicBool,
    warmed: AtomicBool,
    committed: AtomicU64,
    displayed: AtomicU64,
    raw_moves: AtomicU64,
    hook_moves: AtomicU64,
    stop: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            state: AtomicU32::new(State::Off as u32),
            generation: AtomicU64::new(1),
            fault: AtomicU32::new(FaultCode::None as u32),
            heartbeat_ns: AtomicU64::new(0),
            emit_ns: AtomicU64::new(0),
            pending: AtomicBool::new(false),
            warmed: AtomicBool::new(false),
            committed: AtomicU64::new(pack_point(0, 0)),
            displayed: AtomicU64::new(pack_point(0, 0)),
            raw_moves: AtomicU64::new(0),
            hook_moves: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }

    pub fn state(&self) -> State {
        State::from_u32(self.state.load(Ordering::Acquire))
    }

    pub fn set_state(&self, state: State) {
        self.state.store(state as u32, Ordering::Release);
    }

    pub fn suppressing(&self) -> bool {
        self.state().suppressing()
    }

    pub fn warmed(&self) -> bool {
        self.warmed.load(Ordering::Acquire)
    }

    pub fn set_warmed(&self, value: bool) {
        self.warmed.store(value, Ordering::Release);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Invalidates queued output: stale generations are not allowed to move
    /// the cursor after a barrier.
    pub fn invalidate(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Latches a fault and disables suppression. The first cause is preserved.
    pub fn suspend(&self, code: FaultCode) -> bool {
        let _ = self.fault.compare_exchange(
            FaultCode::None as u32,
            code as u32,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.set_state(State::Suspended);
        self.invalidate();
        self.set_pending(false);
        self.state() == State::Suspended
    }

    /// Explicit user toggle; clears a latched fault.
    pub fn clear_fault(&self) {
        self.fault.store(FaultCode::None as u32, Ordering::Release);
    }

    pub fn fault(&self) -> FaultCode {
        FaultCode::from_u32(self.fault.load(Ordering::Acquire))
    }

    pub fn heartbeat(&self, at_ns: u64) {
        self.heartbeat_ns.store(at_ns, Ordering::Release);
    }

    pub fn heartbeat_ns(&self) -> u64 {
        self.heartbeat_ns.load(Ordering::Acquire)
    }

    pub fn set_emit_ns(&self, at_ns: u64) {
        self.emit_ns.store(at_ns, Ordering::Release);
    }

    pub fn emit_ns(&self) -> u64 {
        self.emit_ns.load(Ordering::Acquire)
    }

    pub fn set_pending(&self, value: bool) {
        self.pending.store(value, Ordering::Release);
    }

    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub fn set_committed(&self, x: i32, y: i32) {
        self.committed.store(pack_point(x, y), Ordering::Release);
    }

    pub fn committed(&self) -> (i32, i32) {
        unpack_point(self.committed.load(Ordering::Acquire))
    }

    /// Last position actually emitted (or passed through) to the desktop.
    pub fn set_displayed(&self, x: i32, y: i32) {
        self.displayed.store(pack_point(x, y), Ordering::Release);
    }

    pub fn displayed(&self) -> (i32, i32) {
        unpack_point(self.displayed.load(Ordering::Acquire))
    }

    pub fn note_raw_move(&self) {
        self.raw_moves.fetch_add(1, Ordering::AcqRel);
    }

    pub fn note_hook_move(&self) {
        self.hook_moves.fetch_add(1, Ordering::AcqRel);
    }

    pub fn move_counts(&self) -> (u64, u64) {
        (
            self.raw_moves.load(Ordering::Acquire),
            self.hook_moves.load(Ordering::Acquire),
        )
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

fn pack_point(x: i32, y: i32) -> u64 {
    ((x as u32 as u64) << 32) | y as u32 as u64
}

fn unpack_point(packed: u64) -> (i32, i32) {
    ((packed >> 32) as u32 as i32, packed as u32 as i32)
}

/// Emitter-owned counters. Only the emitter writes; readers take snapshots.
pub struct Metrics {
    pub input_events: AtomicU64,
    pub native_moves: AtomicU64,
    pub output_ticks: AtomicU64,
    pub emitted: AtomicU64,
    pub skipped_identical: AtomicU64,
    pub timer_wakes: AtomicU64,
    pub idle_wakes: AtomicU64,
    pub missed_deadlines: AtomicU64,
    pub max_pending_gap_ns: AtomicU64,
    pub burst_count: AtomicU64,
    lateness: Box<[AtomicU32]>,
    lateness_next: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub input_events: u64,
    pub native_moves: u64,
    pub output_ticks: u64,
    pub emitted: u64,
    pub skipped_identical: u64,
    pub timer_wakes: u64,
    pub idle_wakes: u64,
    pub missed_deadlines: u64,
    pub max_pending_gap_ns: u64,
    pub burst_count: u64,
    pub lateness_p50_ns: u64,
    pub lateness_p99_ns: u64,
    pub lateness_max_ns: u64,
    pub lateness_samples: usize,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let lateness = (0..LATENESS_CAPACITY)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            input_events: AtomicU64::new(0),
            native_moves: AtomicU64::new(0),
            output_ticks: AtomicU64::new(0),
            emitted: AtomicU64::new(0),
            skipped_identical: AtomicU64::new(0),
            timer_wakes: AtomicU64::new(0),
            idle_wakes: AtomicU64::new(0),
            missed_deadlines: AtomicU64::new(0),
            max_pending_gap_ns: AtomicU64::new(0),
            burst_count: AtomicU64::new(0),
            lateness,
            lateness_next: AtomicUsize::new(0),
        }
    }

    /// Records how late a 1 ms output deadline was (microseconds, clamped).
    pub fn record_lateness(&self, late_ns: u64) {
        let micros = (late_ns / 1_000).min(u32::MAX as u64) as u32;
        let index = self.lateness_next.fetch_add(1, Ordering::Relaxed) % LATENESS_CAPACITY;
        self.lateness[index].store(micros, Ordering::Relaxed);
    }

    pub fn note_gap(&self, gap_ns: u64) {
        self.max_pending_gap_ns.fetch_max(gap_ns, Ordering::AcqRel);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let written = self
            .lateness_next
            .load(Ordering::Acquire)
            .min(LATENESS_CAPACITY);
        let mut values = Vec::with_capacity(written);
        for slot in self.lateness.iter().take(written) {
            values.push(slot.load(Ordering::Acquire) as u64 * 1_000);
        }
        values.sort_unstable();
        let pick = |fraction: f64| -> u64 {
            if values.is_empty() {
                return 0;
            }
            let index = ((values.len() as f64 - 1.0) * fraction).round() as usize;
            values[index.min(values.len() - 1)]
        };
        MetricsSnapshot {
            input_events: self.input_events.load(Ordering::Acquire),
            native_moves: self.native_moves.load(Ordering::Acquire),
            output_ticks: self.output_ticks.load(Ordering::Acquire),
            emitted: self.emitted.load(Ordering::Acquire),
            skipped_identical: self.skipped_identical.load(Ordering::Acquire),
            timer_wakes: self.timer_wakes.load(Ordering::Acquire),
            idle_wakes: self.idle_wakes.load(Ordering::Acquire),
            missed_deadlines: self.missed_deadlines.load(Ordering::Acquire),
            max_pending_gap_ns: self.max_pending_gap_ns.load(Ordering::Acquire),
            burst_count: self.burst_count.load(Ordering::Acquire),
            lateness_p50_ns: pick(0.50),
            lateness_p99_ns: pick(0.99),
            lateness_max_ns: values.last().copied().unwrap_or(0),
            lateness_samples: values.len(),
        }
    }
}

/// Metadata captured around a supervised run.
#[derive(Clone, Debug)]
pub struct ReportMeta {
    pub kind: String,
    pub os: String,
    pub arch: String,
    pub cpu: String,
    pub logical_cpus: u32,
    pub wall_ns: u64,
    pub user_ms: u64,
    pub kernel_ms: u64,
    pub endpoint_error_px: u64,
    pub notes: String,
}

impl ReportMeta {
    pub fn one_core_pct(&self) -> f64 {
        if self.wall_ns == 0 {
            return 0.0;
        }
        100.0 * (self.user_ms + self.kernel_ms) as f64 / (self.wall_ns as f64 / 1_000_000.0)
    }

    pub fn machine_pct(&self) -> f64 {
        if self.logical_cpus == 0 {
            0.0
        } else {
            self.one_core_pct() / self.logical_cpus as f64
        }
    }
}

pub const REPORT_HEADER: &str = "schema_version\tkind\tos\tarch\tcpu\tlogical_cpus\twall_ms\tuser_ms\tkernel_ms\tone_core_pct\tmachine_pct\tinput_events\tnative_moves\toutput_ticks\temitted\tskipped_identical\tidle_wakes\tmissed_deadlines\tlateness_p50_us\tlateness_p99_us\tlateness_max_us\tmax_pending_gap_us\tendpoint_error_px\tnotes";

pub fn report_row(meta: &ReportMeta, metrics: &MetricsSnapshot) -> String {
    format!(
        "1\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{}\t{}\t{:.4}\t{:.4}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}",
        meta.kind,
        meta.os,
        meta.arch,
        meta.cpu,
        meta.logical_cpus,
        meta.wall_ns as f64 / 1_000_000.0,
        meta.user_ms,
        meta.kernel_ms,
        meta.one_core_pct(),
        meta.machine_pct(),
        metrics.input_events,
        metrics.native_moves,
        metrics.output_ticks,
        metrics.emitted,
        metrics.skipped_identical,
        metrics.idle_wakes,
        metrics.missed_deadlines,
        metrics.lateness_p50_ns as f64 / 1_000.0,
        metrics.lateness_p99_ns as f64 / 1_000.0,
        metrics.lateness_max_ns as f64 / 1_000.0,
        metrics.max_pending_gap_ns as f64 / 1_000.0,
        meta.endpoint_error_px,
        meta.notes,
    )
}

/// Writes a report to a path that must not exist yet.
pub fn write_report(path: &Path, rows: &[(ReportMeta, MetricsSnapshot)]) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(file, "{REPORT_HEADER}")?;
    for (meta, metrics) in rows {
        writeln!(file, "{}", report_row(meta, metrics))?;
    }
    file.flush()
}

/// Writes a report over an already reserved path.
pub fn write_report_over(
    path: &Path,
    meta: &ReportMeta,
    metrics: &MetricsSnapshot,
) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(path)?;
    writeln!(file, "{REPORT_HEADER}")?;
    writeln!(file, "{}", report_row(meta, metrics))?;
    file.flush()
}

/// Applies one queue message to the engine.
pub fn apply_message(engine: &mut Engine, mapping: Mapping, message: Msg) -> bool {
    match message {
        Msg::Native { at_ns, value } => {
            let result = match mapping {
                Mapping::RelativeToCommitted => engine.push_native(at_ns, value),
                Mapping::IndependentAbsolute => engine.push_absolute(at_ns, value),
            };
            result.is_ok()
        }
        Msg::Barrier { at_ns, anchor } => {
            engine.discard_and_reanchor(at_ns, anchor);
            true
        }
        Msg::Stop => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_latches_first_cause_and_invalidates_generation() {
        let shared = Shared::new();
        shared.set_state(State::Active);
        let before = shared.generation();
        shared.suspend(FaultCode::EmitterStall);
        assert_eq!(shared.state(), State::Suspended);
        assert_eq!(shared.fault(), FaultCode::EmitterStall);
        assert!(shared.generation() > before);
        shared.suspend(FaultCode::QueueOverflow);
        assert_eq!(shared.fault(), FaultCode::EmitterStall);
        shared.clear_fault();
        assert_eq!(shared.fault(), FaultCode::None);
    }

    #[test]
    fn only_active_and_draining_suppress() {
        assert!(!State::Off.suppressing());
        assert!(!State::Warming.suppressing());
        assert!(State::Active.suppressing());
        assert!(State::Draining.suppressing());
        assert!(!State::ButtonsPassthrough.suppressing());
        assert!(!State::Suspended.suppressing());
    }

    #[test]
    fn committed_point_round_trips_negative_coordinates() {
        let shared = Shared::new();
        shared.set_committed(-1920, 1080);
        assert_eq!(shared.committed(), (-1920, 1080));
        shared.set_committed(3839, -50);
        assert_eq!(shared.committed(), (3839, -50));
        shared.set_displayed(-1, 7);
        assert_eq!(shared.displayed(), (-1, 7));
    }

    #[test]
    fn queue_reports_overflow_instead_of_dropping() {
        let mut queue = make_queue();
        for index in 0..QUEUE_CAPACITY {
            assert!(push(
                &mut queue.producer,
                Msg::Native {
                    at_ns: index as u64,
                    value: Point::new(1.0, 0.0)
                }
            ));
        }
        assert!(!push(
            &mut queue.producer,
            Msg::Native {
                at_ns: 0,
                value: Point::new(1.0, 0.0)
            }
        ));
        assert_eq!(queue.consumer.slots(), QUEUE_CAPACITY);
    }

    #[test]
    fn lateness_percentiles_use_recorded_samples() {
        let metrics = Metrics::new();
        for late_us in [0_u64, 100, 200, 300, 4_000] {
            metrics.record_lateness(late_us * 1_000);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.lateness_samples, 5);
        assert_eq!(snapshot.lateness_p50_ns, 200_000);
        assert_eq!(snapshot.lateness_max_ns, 4_000_000);
        assert!(snapshot.lateness_p99_ns >= snapshot.lateness_p50_ns);
    }

    #[test]
    fn empty_metrics_are_zero_not_nan() {
        let snapshot = Metrics::new().snapshot();
        assert_eq!(snapshot, MetricsSnapshot::default());
        let meta = ReportMeta {
            kind: "idle".into(),
            os: "test".into(),
            arch: "x64".into(),
            cpu: "none".into(),
            logical_cpus: 0,
            wall_ns: 0,
            user_ms: 0,
            kernel_ms: 0,
            endpoint_error_px: 0,
            notes: String::new(),
        };
        assert_eq!(meta.machine_pct(), 0.0);
        assert!(report_row(&meta, &snapshot).starts_with("1\tidle\t"));
    }

    #[test]
    fn cpu_percentages_use_one_core_and_machine_denominators() {
        let meta = ReportMeta {
            kind: "idle".into(),
            os: "test".into(),
            arch: "x64".into(),
            cpu: "none".into(),
            logical_cpus: 16,
            wall_ns: 60_000_000_000,
            user_ms: 600,
            kernel_ms: 0,
            endpoint_error_px: 0,
            notes: String::new(),
        };
        assert!((meta.one_core_pct() - 1.0).abs() < 1e-9);
        assert!((meta.machine_pct() - 0.0625).abs() < 1e-12);
    }

    #[test]
    fn barrier_message_reanchors_engine() {
        let mut engine = Engine::new(Point::ZERO);
        engine
            .push_native(10_000_000, Point::new(4.0, 0.0))
            .unwrap();
        let applied = apply_message(
            &mut engine,
            Mapping::RelativeToCommitted,
            Msg::Barrier {
                at_ns: 20_000_000,
                anchor: Point::new(50.0, 50.0),
            },
        );
        assert!(applied);
        assert_eq!(engine.native_position(), Point::new(50.0, 50.0));
        assert!(!engine.has_pending());
    }
}
