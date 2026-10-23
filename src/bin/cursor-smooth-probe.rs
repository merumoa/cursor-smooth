#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

//! Diagnostic companion for cursor-smooth.
//!
//! `replay --self-check` proves the analyzer accepts a good trace and rejects
//! known defects. The remaining commands drive the real Windows hook; they are
//! explicitly opt-in and always restore the pointer.

use cursor_smooth::cli::PROBE_USAGE;
use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[cfg(windows)]
use cursor_smooth::platform::windows as win;
#[cfg(windows)]
use cursor_smooth::runtime::{FaultCode, State};

#[cfg(windows)]
fn raise_timer_resolution() {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    // Diagnostics only: the generator must deliver a real 8 ms cadence for the
    // cadence and gap gates to measure the engine rather than the scheduler.
    unsafe { timeBeginPeriod(1) };
}

#[cfg(not(windows))]
fn raise_timer_resolution() {}

fn main() -> ExitCode {
    raise_timer_resolution();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print!("{PROBE_USAGE}");
        return ExitCode::SUCCESS;
    }
    let code = match args[0].as_str() {
        "replay" => replay(&args[1..]),
        "source-probe" => source_probe(&args[1..]),
        "fault-check" => fault_check(&args[1..]),
        "benchmark" => benchmark(&args[1..]),
        "inject-cost" => inject_cost(&args[1..]),
        "timer-bench" => timer_bench(&args[1..]),
        other => {
            eprintln!("cursor-smooth-probe: unknown command: {other}");
            2
        }
    };
    ExitCode::from(code as u8)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|value| value == name)
}

fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|value| value == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn parse_u64(args: &[String], name: &str) -> Option<u64> {
    value(args, name).and_then(|raw| raw.parse().ok())
}

// ---------------------------------------------------------------- replay gate

#[derive(Clone, Copy, Debug)]
struct Sample {
    at_ns: u64,
    x: f64,
    y: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Gates {
    adequate: bool,
    gain: bool,
    endpoint: bool,
    gap: bool,
    idle: bool,
    cpu: bool,
}

impl Gates {
    fn passed(self) -> bool {
        self.adequate && self.gain && self.endpoint && self.gap && self.idle && self.cpu
    }
}

fn path_length(samples: &[Sample]) -> f64 {
    samples
        .windows(2)
        .map(|pair| {
            let dx = pair[1].x - pair[0].x;
            let dy = pair[1].y - pair[0].y;
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

fn gap_stats(samples: &[Sample]) -> (u64, u64) {
    if samples.len() < 2 {
        return (u64::MAX, u64::MAX);
    }
    let mut gaps: Vec<u64> = samples
        .windows(2)
        .map(|pair| pair[1].at_ns.saturating_sub(pair[0].at_ns))
        .collect();
    gaps.sort_unstable();
    let index = ((gaps.len() as f64 - 1.0) * 0.99).round() as usize;
    (gaps[index.min(gaps.len() - 1)], *gaps.last().unwrap())
}

fn evaluate(inputs: &[Sample], outputs: &[Sample], idle_wakes: u64, cpu_ms_per_sec: f64) -> Gates {
    let input_length = path_length(inputs);
    let output_length = path_length(outputs);
    let adequate = inputs.len() >= 100 && input_length >= 64.0 && outputs.len() >= 100;
    let gain = if input_length > 0.0 {
        let ratio = output_length / input_length;
        (0.98..=1.02).contains(&ratio)
    } else {
        false
    };
    let endpoint = match (inputs.last(), outputs.last()) {
        (Some(input), Some(output)) => {
            ((input.x - output.x).abs()).max((input.y - output.y).abs()) <= 1.5
        }
        _ => false,
    };
    let (_, max_gap) = gap_stats(outputs);
    let gap = max_gap <= 4_000_000;
    let idle = idle_wakes == 0;
    let cpu = cpu_ms_per_sec <= 10.0;
    Gates {
        adequate,
        gain,
        endpoint,
        gap,
        idle,
        cpu,
    }
}

fn synthetic_path(count: usize, interval_ns: u64, step: f64) -> Vec<Sample> {
    (0..count)
        .map(|index| Sample {
            at_ns: index as u64 * interval_ns,
            x: 100.0 + step * index as f64,
            y: 200.0 + step * 0.5 * index as f64,
        })
        .collect()
}

fn sample_at(inputs: &[Sample], at_ns: u64) -> Sample {
    if at_ns <= inputs[0].at_ns {
        return inputs[0];
    }
    for pair in inputs.windows(2) {
        if at_ns <= pair[1].at_ns {
            let span = pair[1].at_ns - pair[0].at_ns;
            let fraction = if span == 0 {
                0.0
            } else {
                (at_ns - pair[0].at_ns) as f64 / span as f64
            };
            return Sample {
                at_ns,
                x: pair[0].x + (pair[1].x - pair[0].x) * fraction,
                y: pair[0].y + (pair[1].y - pair[0].y) * fraction,
            };
        }
    }
    *inputs.last().unwrap()
}

fn replay(args: &[String]) -> i32 {
    if !flag(args, "--self-check") {
        eprintln!("usage: cursor-smooth-probe replay --self-check");
        return 2;
    }
    let inputs = synthetic_path(125, 8_000_000, 4.0);
    let outputs: Vec<Sample> = (0..=1_000_u64)
        .map(|tick| sample_at(&inputs, tick * 1_000_000))
        .collect();
    let mut failures = 0;

    let good = evaluate(&inputs, &outputs, 0, 5.0);
    if !good.passed() {
        eprintln!("FAIL good trace rejected: {good:?}");
        failures += 1;
    }

    let doubled: Vec<Sample> = outputs
        .iter()
        .map(|sample| Sample {
            x: sample.x * 2.0,
            y: sample.y * 2.0,
            ..*sample
        })
        .collect();
    let defective = evaluate(&inputs, &doubled, 0, 5.0);
    if defective.gain {
        eprintln!("FAIL doubled-gain defect passed the gain gate");
        failures += 1;
    }

    let stalled: Vec<Sample> = outputs
        .iter()
        .copied()
        .filter(|sample| !(400_000_000..410_000_000).contains(&sample.at_ns))
        .collect();
    let defective = evaluate(&inputs, &stalled, 0, 5.0);
    if defective.gap {
        eprintln!("FAIL 10 ms output stall passed the gap gate");
        failures += 1;
    }

    let empty: Vec<Sample> = Vec::new();
    let defective = evaluate(&empty, &outputs, 60_000, 0.05);
    if defective.adequate {
        eprintln!("FAIL zero-input trace passed the adequacy gate");
        failures += 1;
    }
    if defective.idle {
        eprintln!("FAIL idle polling passed the idle gate");
        failures += 1;
    }

    let frozen = vec![
        Sample {
            at_ns: 0,
            x: 10.0,
            y: 10.0,
        };
        500
    ];
    if evaluate(&frozen, &outputs, 0, 5.0).adequate {
        eprintln!("FAIL motionless trace passed the adequacy gate");
        failures += 1;
    }

    if evaluate(&inputs, &outputs, 0, 25.0).cpu {
        eprintln!("FAIL CPU hog passed the CPU gate");
        failures += 1;
    }

    println!(
        "replay self-check: {}",
        if failures == 0 { "pass" } else { "fail" }
    );
    if failures == 0 { 0 } else { 1 }
}

// ------------------------------------------------------------ Windows drills

#[cfg(not(windows))]
fn source_probe(_args: &[String]) -> i32 {
    eprintln!("cursor-smooth-probe: source-probe requires Windows");
    1
}

#[cfg(not(windows))]
fn fault_check(_args: &[String]) -> i32 {
    eprintln!("cursor-smooth-probe: fault-check requires Windows");
    1
}

#[cfg(not(windows))]
fn benchmark(_args: &[String]) -> i32 {
    eprintln!("cursor-smooth-probe: benchmark requires Windows");
    1
}

#[cfg(windows)]
fn inject_sequence(count: usize, interval: Duration, amplitude: i32, nonce: u64) {
    let start = Instant::now();
    for index in 0..count {
        let delta = if index % 2 == 0 {
            amplitude
        } else {
            -amplitude
        };
        win::inject_relative(delta, 0, nonce);
        let target = interval * (index as u32 + 1);
        let elapsed = start.elapsed();
        if target > elapsed {
            std::thread::sleep(target - elapsed);
        }
    }
}

#[cfg(windows)]
fn inject_closed_loop(count: usize, interval: Duration, step: i32, nonce: u64) {
    // Continuous motion in a small square: every sample changes the target, so
    // the engine always has a pending interpolated goal to reach.
    let leg = 4;
    let start = Instant::now();
    for index in 0..count {
        let (dx, dy) = match (index / leg) % 4 {
            0 => (step, 0),
            1 => (0, step),
            2 => (-step, 0),
            _ => (0, -step),
        };
        win::inject_relative(dx, dy, nonce);
        let target = interval * (index as u32 + 1);
        let elapsed = start.elapsed();
        if target > elapsed {
            std::thread::sleep(target - elapsed);
        }
    }
}

#[cfg(windows)]
fn source_probe(args: &[String]) -> i32 {
    let cycles = parse_u64(args, "--cycles").unwrap_or(8).clamp(2, 40) as usize;
    let allow = flag(args, "--allow-cursor-move");
    if !allow {
        let seconds = parse_u64(args, "--seconds").unwrap_or(10).clamp(1, 120);
        return observe_physical(seconds);
    }
    let saved = win::cursor_position();
    let session = match win::Session::start(win::SessionConfig {
        scenario: win::Scenario::SourceProbe,
        duration: None,
        report: None,
        mapping: cursor_smooth::runtime::Mapping::RelativeToCommitted,
    }) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("source-probe: cannot start session: {error}");
            return 1;
        }
    };
    win::set_cursor(400, 400);
    session.toggle();
    let nonce = session.diagnostics.nonce;
    let interval = Duration::from_millis(8);
    inject_sequence(2, interval, 8, nonce);
    std::thread::sleep(Duration::from_millis(30));
    inject_sequence(1, interval, 6, nonce);
    if !session.wait_for_state(State::Active, Duration::from_secs(2)) {
        eprintln!("source-probe: smoothing never became active");
        let _ = session.finish();
        if let Some((x, y)) = saved {
            win::set_cursor(x, y);
        }
        return 1;
    }
    std::thread::sleep(Duration::from_millis(20));

    // Closed loop: right, down, left, up. The pointer stays inside a small box,
    // so no large absolute jump can contaminate the measured deltas.
    let leg = 4;
    let step = 6;
    for _ in 0..cycles {
        for _ in 0..leg {
            win::inject_relative(step, 0, nonce);
            std::thread::sleep(interval);
        }
        for _ in 0..leg {
            win::inject_relative(0, step, nonce);
            std::thread::sleep(interval);
        }
        for _ in 0..leg {
            win::inject_relative(-step, 0, nonce);
            std::thread::sleep(interval);
        }
        for _ in 0..leg {
            win::inject_relative(0, -step, nonce);
            std::thread::sleep(interval);
        }
    }
    let moves = cycles * leg * 4;
    let flush_deadline = Instant::now() + Duration::from_millis(300);
    while session.shared.pending() && Instant::now() < flush_deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(20));
    let cursor_after = win::cursor_position();
    let summary = session.finish();
    if let Some((x, y)) = saved {
        win::set_cursor(x, y);
    }

    let active_time = summary
        .state_trace
        .iter()
        .find(|record| record.values[0] as u32 == State::Active as u32)
        .map(|record| record.at_ns)
        .unwrap_or(0);
    let native: Vec<win::TraceRecord> = summary
        .input_trace
        .iter()
        .filter(|record| record.kind == 1 && record.at_ns >= active_time)
        .copied()
        .collect();
    let outputs: Vec<win::TraceRecord> = summary
        .output_trace
        .iter()
        .filter(|record| record.at_ns >= active_time)
        .copied()
        .collect();

    if native.len() < moves / 2 || outputs.len() < 32 {
        eprintln!(
            "source-probe: insufficient samples (native {}, outputs {})",
            native.len(),
            outputs.len()
        );
        return 1;
    }
    let tail = native.len().min(moves);
    let native = &native[native.len() - tail..];
    let mut native_total = (0.0_f64, 0.0_f64);
    let mut native_length = 0.0_f64;
    for record in native {
        let dx = record.values[0] - record.values[2];
        let dy = record.values[1] - record.values[3];
        native_total.0 += dx;
        native_total.1 += dy;
        native_length += dx.abs() + dy.abs();
    }
    let mut emitted_total = (0.0_f64, 0.0_f64);
    let mut emitted_length = 0.0_f64;
    for pair in outputs.windows(2) {
        let dx = pair[1].values[0] - pair[0].values[0];
        let dy = pair[1].values[1] - pair[0].values[1];
        emitted_total.0 += dx;
        emitted_total.1 += dy;
        emitted_length += dx.abs() + dy.abs();
    }
    let displacement_error = (native_total.0 - emitted_total.0)
        .abs()
        .max((native_total.1 - emitted_total.1).abs());
    let length_ratio = if native_length > 0.0 {
        emitted_length / native_length
    } else {
        0.0
    };
    let endpoint_error = match (outputs.last(), native.last()) {
        (Some(output), Some(record)) => (output.values[0] - record.values[0])
            .abs()
            .max((output.values[1] - record.values[1]).abs()),
        _ => f64::INFINITY,
    };
    let cursor_error = match (cursor_after, outputs.last()) {
        (Some((x, y)), Some(output)) => (x as f64 - output.values[0])
            .abs()
            .max(y as f64 - output.values[1]),
        _ => f64::INFINITY,
    };
    let pass = native.len() >= moves / 2
        && displacement_error <= 3.0
        && (0.98..=1.02).contains(&length_ratio)
        && endpoint_error <= 2.0
        && cursor_error <= 2.0;

    println!("source-probe (synthetic relative input, diagnostics build)");
    println!("compared_moves: {}", native.len());
    println!(
        "native_displacement_px: ({:.1}, {:.1})",
        native_total.0, native_total.1
    );
    println!(
        "emitted_displacement_px: ({:.1}, {:.1})",
        emitted_total.0, emitted_total.1
    );
    println!("displacement_error_px: {displacement_error:.3}");
    println!("path_length_ratio_manhattan: {length_ratio:.5}");
    println!("output_endpoint_vs_native_px: {endpoint_error:.3}");
    println!("cursor_vs_output_px: {cursor_error:.3}");
    println!("mapping: relative_to_committed");
    println!(
        "mapping_gate: {}",
        if pass {
            "pass"
        } else {
            "FAIL (do not claim a working low-CPU Windows release)"
        }
    );
    println!("physical MX observation: required separately (run without --allow-cursor-move)");
    if pass { 0 } else { 1 }
}

#[cfg(windows)]
fn observe_physical(seconds: u64) -> i32 {
    let session = match win::Session::start(win::SessionConfig {
        scenario: win::Scenario::Observe,
        duration: None,
        report: None,
        mapping: cursor_smooth::runtime::Mapping::RelativeToCommitted,
    }) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("source-probe: cannot start session: {error}");
            return 1;
        }
    };
    println!(
        "observe-only: move the physical mouse for {seconds} s; no pointer motion is generated"
    );
    std::thread::sleep(Duration::from_secs(seconds));
    let summary = session.finish();
    let moves: Vec<win::TraceRecord> = summary
        .input_trace
        .iter()
        .filter(|record| record.kind == 1)
        .copied()
        .collect();
    let mut length = 0.0;
    for pair in moves.windows(2) {
        let dx = pair[1].values[0] - pair[0].values[0];
        let dy = pair[1].values[1] - pair[0].values[1];
        length += (dx * dx + dy * dy).sqrt();
    }
    let adequate = moves.len() >= 32 && length >= 64.0;
    println!("physical_moves: {}", moves.len());
    println!("physical_path_px: {length:.1}");
    println!(
        "physical_evidence: {}",
        if adequate {
            "collected"
        } else {
            "insufficient (no physical motion observed)"
        }
    );
    if adequate { 0 } else { 1 }
}

#[cfg(windows)]
fn ensure_active(session: &win::Session, nonce: u64) -> bool {
    session.toggle();
    std::thread::sleep(Duration::from_millis(20));
    for _ in 0..3 {
        win::inject_relative(4, 0, nonce);
        std::thread::sleep(Duration::from_millis(8));
    }
    session.wait_for_state(State::Active, Duration::from_secs(2))
}

#[cfg(windows)]
fn suspend_latency(state_trace: &[win::TraceRecord], requested_ns: u64) -> Option<u64> {
    state_trace
        .iter()
        .find(|record| {
            record.at_ns >= requested_ns && record.values[0] as u32 == State::Suspended as u32
        })
        .map(|record| record.at_ns.saturating_sub(requested_ns))
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum Drill {
    EmitterStall,
    InjectionFailed,
    QueueOverflow,
    ButtonRace,
    HookRemoved,
}

#[cfg(windows)]
impl Drill {
    fn label(self) -> &'static str {
        match self {
            Drill::EmitterStall => "emitter_stall",
            Drill::InjectionFailed => "injection_failed",
            Drill::QueueOverflow => "queue_overflow",
            Drill::ButtonRace => "button_race",
            Drill::HookRemoved => "hook_removed",
        }
    }

    fn expected(self) -> Option<FaultCode> {
        match self {
            Drill::EmitterStall => Some(FaultCode::EmitterStall),
            Drill::InjectionFailed => Some(FaultCode::InjectionFailed),
            Drill::QueueOverflow => Some(FaultCode::QueueOverflow),
            Drill::ButtonRace => None,
            Drill::HookRemoved => Some(FaultCode::HookMissing),
        }
    }
}

#[cfg(windows)]
fn fault_check(args: &[String]) -> i32 {
    use std::sync::atomic::Ordering;
    if !flag(args, "--allow-cursor-move") {
        eprintln!("fault-check: --allow-cursor-move is required (the drill moves the pointer)");
        return 2;
    }
    let saved = win::cursor_position();
    let mut failures = 0;
    let only = value(args, "--drill");
    for drill in [
        Drill::EmitterStall,
        Drill::InjectionFailed,
        Drill::QueueOverflow,
        Drill::ButtonRace,
        Drill::HookRemoved,
    ] {
        if let Some(only) = only
            && drill.label() != only
        {
            continue;
        }
        let session = match win::Session::start(win::SessionConfig {
            scenario: win::Scenario::FaultCheck,
            duration: None,
            report: None,
            mapping: cursor_smooth::runtime::Mapping::RelativeToCommitted,
        }) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("fault-check: cannot start session: {error}");
                return 1;
            }
        };
        let nonce = session.diagnostics.nonce;
        if !ensure_active(&session, nonce) {
            println!("{}: FAIL (never active)", drill.label());
            failures += 1;
            let _ = session.finish();
            continue;
        }

        if matches!(drill, Drill::ButtonRace) {
            win::inject_button(true, nonce);
            std::thread::sleep(Duration::from_millis(5));
            let during = session.shared.state();
            win::inject_button(false, nonce);
            std::thread::sleep(Duration::from_millis(20));
            if during != State::ButtonsPassthrough {
                println!("button_race: FAIL (state was {during:?})");
                failures += 1;
            } else {
                println!("button_race: pass (buttons-passthrough barrier)");
            }
            let _ = session.finish();
            continue;
        }

        let requested = session.now_ns();
        match drill {
            Drill::EmitterStall => {
                win::inject_relative(6, 0, nonce);
                std::thread::sleep(Duration::from_millis(2));
                session
                    .diagnostics
                    .pause_consumer
                    .store(true, Ordering::Release);
            }
            Drill::InjectionFailed => {
                session
                    .diagnostics
                    .inject_failure
                    .store(true, Ordering::Release);
                win::inject_relative(6, 0, nonce);
            }
            Drill::QueueOverflow => {
                session
                    .diagnostics
                    .pause_consumer
                    .store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut fill = 0;
                while Instant::now() < deadline
                    && session.shared.state() != State::Suspended
                    && fill < 2048
                {
                    win::inject_relative(1, 0, nonce);
                    fill += 1;
                }
            }
            Drill::ButtonRace => unreachable!("handled before the timed drills"),
            Drill::HookRemoved => {
                session
                    .diagnostics
                    .remove_hook
                    .store(true, Ordering::Release);
                for _ in 0..40 {
                    win::inject_relative(7, 0, nonce);
                    std::thread::sleep(Duration::from_millis(3));
                }
            }
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && session.shared.state() != State::Suspended {
            std::thread::sleep(Duration::from_millis(2));
        }
        session
            .diagnostics
            .pause_consumer
            .store(false, Ordering::Release);
        session
            .diagnostics
            .inject_failure
            .store(false, Ordering::Release);

        let suspended = session.shared.state() == State::Suspended;
        let cause = session.shared.fault();
        let summary = session.finish();

        if !suspended {
            println!("{}: FAIL (no suspension)", drill.label());
            failures += 1;
            continue;
        }
        if let Some(expected) = drill.expected()
            && cause != expected
        {
            println!(
                "{}: FAIL (cause {}, expected {})",
                drill.label(),
                cause.as_str(),
                expected.as_str()
            );
            failures += 1;
        }
        match suspend_latency(&summary.state_trace, requested) {
            Some(latency) => {
                let suspended_at = requested + latency + 2_000_000;
                let reenabled_at = summary
                    .state_trace
                    .iter()
                    .filter(|record| record.at_ns > suspended_at)
                    .map(|record| record.at_ns)
                    .min()
                    .unwrap_or(u64::MAX);
                let after = summary
                    .output_trace
                    .iter()
                    .filter(|record| record.at_ns > suspended_at && record.at_ns < reenabled_at)
                    .count();
                let mut ok = true;
                if latency > 250_000_000 {
                    ok = false;
                }
                if after > 0 {
                    ok = false;
                }
                println!(
                    "{}: suspended_in_us={} outputs_after_suspension={after} -> {}",
                    drill.label(),
                    latency / 1_000,
                    if ok { "pass" } else { "FAIL" }
                );
                if !ok {
                    failures += 1;
                }
            }
            None => {
                println!("{}: FAIL (no suspended state recorded)", drill.label());
                failures += 1;
            }
        }
    }
    if let Some((x, y)) = saved {
        win::set_cursor(x, y);
    }
    println!(
        "fault-check: {}",
        if failures == 0 { "pass" } else { "fail" }
    );
    if failures == 0 { 0 } else { 1 }
}

#[cfg(not(windows))]
fn timer_bench(_args: &[String]) -> i32 {
    eprintln!("cursor-smooth-probe: timer-bench requires Windows");
    1
}

#[cfg(windows)]
fn timer_bench(args: &[String]) -> i32 {
    let seconds = parse_u64(args, "--seconds").unwrap_or(2).clamp(1, 10);
    let wakes = win::measure_timer_cadence(seconds);
    println!(
        "timer-bench: seconds={seconds} wakes={wakes} cadence_per_s={:.1}",
        wakes as f64 / seconds as f64
    );
    0
}

#[cfg(not(windows))]
fn inject_cost(_args: &[String]) -> i32 {
    eprintln!("cursor-smooth-probe: inject-cost requires Windows");
    1
}

#[cfg(windows)]
fn inject_cost(args: &[String]) -> i32 {
    if !flag(args, "--allow-cursor-move") {
        eprintln!("inject-cost: --allow-cursor-move is required");
        return 2;
    }
    let count = parse_u64(args, "--count")
        .unwrap_or(2000)
        .clamp(100, 20_000) as usize;
    let saved = win::cursor_position();
    if flag(args, "--bare") {
        // Baseline: SendInput cost with no hook, queue, or emitter installed.
        let start = win::process_cpu();
        let wall = Instant::now();
        for _ in 0..count {
            win::inject_relative(1, 0, 0);
        }
        let end = win::process_cpu();
        let cpu_ms = (end.0 + end.1).saturating_sub(start.0 + start.1);
        println!(
            "inject-cost(bare): count={count} cpu_ms={cpu_ms} wall_ms={} us_per_injection={:.2}",
            wall.elapsed().as_millis(),
            cpu_ms as f64 * 1000.0 / count as f64
        );
        if let Some((x, y)) = saved {
            win::set_cursor(x, y);
        }
        return 0;
    }
    let session = match win::Session::start(win::SessionConfig {
        scenario: win::Scenario::ReplayOutput,
        duration: None,
        report: None,
        mapping: cursor_smooth::runtime::Mapping::RelativeToCommitted,
    }) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("inject-cost: cannot start session: {error}");
            return 1;
        }
    };
    let nonce = session.diagnostics.nonce;
    if !ensure_active(&session, nonce) {
        eprintln!("inject-cost: smoothing never active");
        let _ = session.finish();
        return 1;
    }
    let start = win::process_cpu();
    let wall = Instant::now();
    for _ in 0..count {
        win::inject_relative(1, 0, nonce);
    }
    let end = win::process_cpu();
    let elapsed = wall.elapsed();
    let summary = session.finish();
    if let Some((x, y)) = saved {
        win::set_cursor(x, y);
    }
    let cpu_ms = (end.0 + end.1).saturating_sub(start.0 + start.1);
    let per_injection_us = cpu_ms as f64 * 1000.0 / count as f64;
    println!(
        "inject-cost: count={count} cpu_ms={cpu_ms} wall_ms={} us_per_injection={per_injection_us:.2} ticks={} emitted={}",
        elapsed.as_millis(),
        summary.metrics.output_ticks,
        summary.metrics.emitted
    );
    println!(
        "projected_1khz_ms_per_s: {:.1}",
        per_injection_us * 1000.0 / 1000.0
    );
    0
}

#[cfg(windows)]
#[derive(Debug)]
struct TargetReport {
    wall_ms: f64,
    user_ms: f64,
    kernel_ms: f64,
    input_events: f64,
    output_ticks: f64,
    emitted: f64,
    idle_wakes: f64,
    p99_us: f64,
    max_gap_us: f64,
    endpoint_px: f64,
}

#[cfg(windows)]
fn read_report(path: &std::path::Path) -> Option<TargetReport> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next()?.split('\t').collect();
    let row: Vec<&str> = lines.next()?.split('\t').collect();
    let index = |name: &str| header.iter().position(|value| *value == name);
    let get = |name: &str| -> Option<f64> {
        let position = index(name)?;
        row.get(position)?.parse().ok()
    };
    Some(TargetReport {
        wall_ms: get("wall_ms")?,
        user_ms: get("user_ms")?,
        kernel_ms: get("kernel_ms")?,
        input_events: get("input_events")?,
        output_ticks: get("output_ticks")?,
        emitted: get("emitted")?,
        idle_wakes: get("idle_wakes")?,
        p99_us: get("lateness_p99_us")?,
        max_gap_us: get("max_pending_gap_us")?,
        endpoint_px: get("endpoint_error_px")?,
    })
}

#[cfg(windows)]
fn benchmark(args: &[String]) -> i32 {
    let mode = value(args, "--mode").unwrap_or("");
    let seconds = parse_u64(args, "--seconds").unwrap_or(60).clamp(5, 600);
    let trials = parse_u64(args, "--trials").unwrap_or(3).clamp(1, 10);
    match mode {
        "idle" => benchmark_target(args, seconds, trials, false),
        "physical" => benchmark_target(args, seconds, trials, true),
        "replay-output" => benchmark_replay(args, seconds, trials),
        _ => {
            eprintln!("benchmark: --mode must be idle, replay-output or physical");
            2
        }
    }
}

#[cfg(windows)]
fn default_target(args: &[String]) -> std::path::PathBuf {
    value(args, "--target")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/release/cursor-smooth.exe"))
}

#[cfg(windows)]
fn benchmark_target(args: &[String], seconds: u64, trials: u64, physical: bool) -> i32 {
    if physical && !flag(args, "--allow-cursor-move") {
        eprintln!(
            "benchmark: physical mode requires --allow-cursor-move and a human moving the mouse"
        );
        return 2;
    }
    let target = default_target(args);
    if !target.exists() {
        eprintln!("benchmark: target not found: {}", target.display());
        return 1;
    }
    let mut worst_ok = true;
    for trial in 0..trials {
        let report_path = std::env::temp_dir().join(format!(
            "cursor-smooth-bench-{}-{}-{trial}.tsv",
            std::process::id(),
            win::monotonic_ns()
        ));
        let observer_start = win::process_cpu();
        let started = Instant::now();
        let status = std::process::Command::new(&target)
            .arg("run")
            .arg("--duration")
            .arg(seconds.to_string())
            .arg("--report")
            .arg(&report_path)
            .status();
        let observer_end = win::process_cpu();
        let wall = started.elapsed();
        let observer_ms =
            (observer_end.0 + observer_end.1).saturating_sub(observer_start.0 + observer_start.1);
        match status {
            Ok(status) if status.success() => {}
            other => {
                eprintln!("benchmark: target exited unexpectedly: {other:?}");
                worst_ok = false;
                continue;
            }
        }
        let Some(report) = read_report(&report_path) else {
            eprintln!(
                "benchmark: cannot read target report {}",
                report_path.display()
            );
            worst_ok = false;
            continue;
        };
        let _ = std::fs::remove_file(&report_path);
        let cpu_per_sec = (report.user_ms + report.kernel_ms) / (report.wall_ms / 1000.0);
        let observer_per_sec = observer_ms as f64 / wall.as_secs_f64();
        let mut ok = true;
        if physical && report.input_events < 1.0 {
            println!("trial {trial}: physical input unavailable (no native events observed)");
            worst_ok = false;
            continue;
        }
        if !physical {
            if report.idle_wakes > 0.0 {
                println!(
                    "trial {trial}: FAIL idle timer wakes = {}",
                    report.idle_wakes
                );
                ok = false;
            }
            // CPU time is reported in scheduler-tick quanta (15.625 ms); a
            // single tick over the run is below the resolution of the measure.
            let resolution = 15.625 / (report.wall_ms / 1000.0).max(0.001);
            if cpu_per_sec > 0.1 + resolution {
                println!(
                    "trial {trial}: FAIL idle CPU {cpu_per_sec:.4} ms/s (resolution {resolution:.3})"
                );
                ok = false;
            }
        } else {
            if cpu_per_sec > 10.0 {
                println!("trial {trial}: FAIL active CPU {cpu_per_sec:.4} ms/s");
                ok = false;
            }
            let cadence = report.output_ticks / (report.wall_ms / 1000.0);
            if cadence < 950.0 {
                println!("trial {trial}: FAIL cadence {cadence:.0}/s");
                ok = false;
            }
            if report.p99_us > 1000.0 || report.max_gap_us > 4000.0 {
                println!(
                    "trial {trial}: FAIL lateness p99 {:.0} us, gap {:.0} us",
                    report.p99_us, report.max_gap_us
                );
                ok = false;
            }
            if report.endpoint_px > 1.0 {
                println!(
                    "trial {trial}: FAIL endpoint error {:.1} px",
                    report.endpoint_px
                );
                ok = false;
            }
        }
        println!(
            "trial {trial}: wall_ms={:.0} user_ms={:.0} kernel_ms={:.0} cpu_ms_s={cpu_per_sec:.4} observer_ms_s={observer_per_sec:.3} idle_wakes={} emitted={:.0} ticks={:.0} p99_us={:.0} gap_us={:.0} endpoint_px={:.1} -> {}",
            report.wall_ms,
            report.user_ms,
            report.kernel_ms,
            report.idle_wakes,
            report.emitted,
            report.output_ticks,
            report.p99_us,
            report.max_gap_us,
            report.endpoint_px,
            if ok { "pass" } else { "FAIL" }
        );
        worst_ok &= ok;
    }
    if worst_ok { 0 } else { 1 }
}

#[cfg(windows)]
fn benchmark_replay(args: &[String], seconds: u64, trials: u64) -> i32 {
    if !flag(args, "--allow-cursor-move") {
        eprintln!("benchmark: replay-output requires --allow-cursor-move");
        return 2;
    }
    let saved = win::cursor_position();
    let mut worst_ok = true;
    for trial in 0..trials {
        let session = match win::Session::start(win::SessionConfig {
            scenario: win::Scenario::ReplayOutput,
            duration: None,
            report: None,
            mapping: cursor_smooth::runtime::Mapping::RelativeToCommitted,
        }) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("benchmark: cannot start session: {error}");
                return 1;
            }
        };
        if let Some((x, y)) = saved {
            win::set_cursor(x, y);
        }
        let nonce = session.diagnostics.nonce;
        if !ensure_active(&session, nonce) {
            eprintln!("benchmark: smoothing never active");
            let _ = session.finish();
            worst_ok = false;
            continue;
        }
        let cpu_start = win::process_cpu();
        let generator_start = win::thread_cpu_ms();
        let started = Instant::now();
        let count = (seconds * 125) as usize;
        inject_closed_loop(count, Duration::from_millis(8), 6, nonce);
        let generator_end = win::thread_cpu_ms();
        let cpu_end = win::process_cpu();
        let wall = started.elapsed();
        let summary = session.finish();
        let target_user = cpu_end
            .0
            .saturating_sub(cpu_start.0 + generator_end.0 - generator_start.0);
        let target_kernel = cpu_end
            .1
            .saturating_sub(cpu_start.1 + generator_end.1 - generator_start.1);
        let wall_s = wall.as_secs_f64().max(0.001);
        let cpu_per_sec = (target_user + target_kernel) as f64 / wall_s;
        let observer_per_sec = ((generator_end.0 + generator_end.1)
            .saturating_sub(generator_start.0 + generator_start.1))
            as f64
            / wall_s;
        let cadence = summary.metrics.output_ticks as f64 / wall_s;
        let mut ok = true;
        if cadence < 950.0 {
            println!("trial {trial}: FAIL cadence {cadence:.0}/s");
            ok = false;
        }
        if summary.metrics.emitted < 100 {
            println!(
                "trial {trial}: FAIL too few changed outputs {}",
                summary.metrics.emitted
            );
            ok = false;
        }
        if summary.metrics.lateness_p99_ns > 1_000_000 {
            println!(
                "trial {trial}: FAIL lateness p99 {} ns",
                summary.metrics.lateness_p99_ns
            );
            ok = false;
        }
        if summary.metrics.max_pending_gap_ns > 4_000_000 {
            println!(
                "trial {trial}: FAIL output gap {} ns",
                summary.metrics.max_pending_gap_ns
            );
            ok = false;
        }
        if cpu_per_sec > 10.0 {
            println!("trial {trial}: FAIL CPU {cpu_per_sec:.4} ms/s");
            ok = false;
        }
        println!(
            "trial {trial}: wall_ms={:.0} target_ms_s={cpu_per_sec:.4} observer_ms_s={observer_per_sec:.3} ticks={:.0} emitted={} p99_us={:.1} gap_us={:.1} skipped={} -> {}",
            wall.as_millis(),
            summary.metrics.output_ticks,
            summary.metrics.emitted,
            summary.metrics.lateness_p99_ns as f64 / 1000.0,
            summary.metrics.max_pending_gap_ns as f64 / 1000.0,
            summary.metrics.skipped_identical,
            if ok { "pass" } else { "FAIL" }
        );
        worst_ok &= ok;
    }
    if let Some((x, y)) = saved {
        win::set_cursor(x, y);
    }
    if worst_ok { 0 } else { 1 }
}
