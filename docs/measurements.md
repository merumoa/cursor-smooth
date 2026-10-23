# Measurements

Host: AMD Ryzen 7 9800X3D (8 cores / 16 logical CPUs), Windows 11 build 26200,
Rust 1.99.0, release build without the `diagnostics` feature except where noted.
All numbers below are from this host only. A percentage of one logical core is
`100 * (user_ms + kernel_ms) / wall_ms`; on this host 1% of one core is 0.0625% of the
whole machine.

## Release gates

| Gate | Target | Measured | Result |
|---|---|---|---|
| Idle timer wakeups | 0 | 0 | pass |
| Idle CPU | <= 0.1 ms/s | 0.00 ms/s (15 s trials, 15.625 ms accounting quantum) | pass |
| Active output opportunities | >= 950/s | 741-753/s | **fail** |
| Active p99 lateness | <= 1 ms | 2.28-2.42 ms | **fail** |
| Active output gap | <= 4 ms | 30.9-32.9 ms | **fail** |
| Active CPU | <= 10 ms/s | 48.0-57.3 ms/s (observer 1.1-4.1 ms/s excluded) | **fail** |

The engine itself is not the limiter of the cadence on this host: a bare 1 ms
`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer, with no work in the loop, wakes at
736-748 times per second here (periodic and one-shot re-arm both measured). The 950/s
gate would require a spin or a higher-rate source, both of which the design forbids.

Per-event injection cost, measured around a real session with the hook installed and
without one (`inject-cost`), is roughly 8-39 us bare and 43 us hooked on this host, so a
1000 Hz emitter has a floor of tens of milliseconds of CPU per second with `SendInput`
plus a low-level hook. That is the measured physical cost of the approach, not a
regression in the engine loop.

## Mapping gate (`source-probe --allow-cursor-move --cycles 8`)

Comparative displacement over 128 native moves in a closed loop, three consecutive runs:

```
native_displacement_px: (2.0, -3.0)   emitted_displacement_px: (4.0, -3.0)   displacement_error_px: 2.000
path_length_ratio_manhattan: 1.00504  cursor_vs_output_px: 0.000            mapping_gate: pass
native_displacement_px: (2.0, 0.0)    emitted_displacement_px: (4.0, 0.0)    displacement_error_px: 2.000
path_length_ratio_manhattan: 1.00532  cursor_vs_output_px: 0.000            mapping_gate: pass
native_displacement_px: (3.0, -1.0)   emitted_displacement_px: (3.0, -1.0)   displacement_error_px: 0.000
path_length_ratio_manhattan: 1.00000  cursor_vs_output_px: 0.000            mapping_gate: pass
```

Mapping selected: `RelativeToCommitted` (`hook.pt - committed_pointer`), validated on
Windows 11 build 26200 at the default pointer speed. The residual 0-2 px is integer
quantisation accumulated over 128 moves on a closed loop whose native deltas do not sum
to exactly zero.

## Fault drills (`fault-check --allow-cursor-move`)

Each drill runs in its own session; the supervisor measures from the fault request to the
first latched `Suspended` state and counts outputs after that latch (bounded by the next
state transition). The latch is asynchronous, so one in-flight emission within 2 ms is
tolerated.

```
emitter_stall: suspended_in_us=83374 outputs_after_suspension=0 -> pass
injection_failed: suspended_in_us=16092 outputs_after_suspension=0 -> pass
queue_overflow: suspended_in_us=66780 outputs_after_suspension=0 -> pass
button_race: pass (buttons-passthrough barrier)
hook_removed: suspended_in_us=83408 outputs_after_suspension=0 -> pass
fault-check: pass
```

`hook_removed` is detected as unseen cursor motion (cursor moved more than 8 px from the
displayed position with no recent emission), because Windows raw-input counting is not
delivered for message-only windows; that is the physically grounded signal the guard uses.

## Idle (`benchmark --mode idle --seconds 15 --trials 2`)

```
wall_ms=15007 user_ms=0 kernel_ms=0 idle_wakes=0 emitted=0 ticks=0 -> pass
wall_ms=15014 user_ms=0 kernel_ms=0 idle_wakes=0 emitted=0 ticks=0 -> pass
```

Tests: `cargo test --locked --all-targets` runs 28 tests on Windows. `cargo check
--locked --target aarch64-apple-darwin --all-targets` and the `x86_64-apple-darwin`
equivalent type-check the macOS adapter and its unit tests locally; CI runs them natively
on macOS runners (this host cannot link Apple binaries).

Physical-input measurement (`benchmark --mode physical`) is unavailable: it needs a human
moving the mouse for the whole trial.
