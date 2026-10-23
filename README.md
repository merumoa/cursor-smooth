# cursor-smooth

[![ci](https://github.com/merumoa/cursor-smooth/actions/workflows/ci.yml/badge.svg)](https://github.com/merumoa/cursor-smooth/actions/workflows/ci.yml)

Software cursor smoothing for a low-polling-rate mouse on a high-refresh display.
It does **not** change the mouse radio polling rate, DPI, acceleration or firmware; it
replaces the visible cursor path with a 1000 Hz output opportunity stream fed by the
native desktop displacement Windows already produced.

Status (2026-10-23):

| Area | State |
|---|---|
| Windows engine, hook, guard, CLI | implemented, local mapping gate and fault drills pass |
| Windows performance gates | **not met on the author's host** - see [docs/measurements.md](docs/measurements.md) |
| macOS adapter | experimental, builds for arm64 and x86_64, never run on physical hardware |
| Release assets | none until the performance gates pass |

## What it does

- Buffers deliberately: linear interpolation of the post-acceleration desktop path with
  `lag = clamp(1.5 * median(input interval), 4 ms, 40 ms)` and never extrapolates past the
  newest real sample. A 125 Hz mouse therefore gets about 12 ms of deliberate buffering,
  not prediction.
- Emits absolute virtual-desktop positions at up to 1000 scheduling opportunities per
  second, skipping identical quantised pixels.
- Goes fully idle (no armed timer, zero wakeups) when no motion is pending.
- Fails open: hook/tap install failure, queue overflow, emitter stall, injection failure,
  lost hook callbacks, cursor read failure and cursor teleport all disable suppression and
  latch a fault that only an explicit toggle clears.

## Usage

```
cursor-smooth run                      # start smoothing (Windows)
cursor-smooth run --duration 30 --report out.tsv
cursor-smooth doctor                   # permissions, display and pointer facts, read-only
cursor-smooth --help
cursor-smooth --version
```

- `Ctrl+Alt+M` toggles smoothing, `Ctrl+Alt+Esc` is the emergency off.
- Hold any mouse button and motion passes through natively until every button is released,
  so click targets and drag ordering stay native. Drag motion is therefore unsmoothed.
- Unknown commands and options (including the retired `--hz` and `--simulate`) exit 2
  before any hook is installed.
- `--report` writes a UTF-8 TSV (`schema_version=1`) with CPU time, counts, lateness and
  endpoint error; the file must not exist yet.

macOS:

```
cursor-smooth run --experimental-macos   # requires Accessibility + Input Monitoring
cursor-smooth doctor
```

Without `--experimental-macos` macOS `run` exits 2. Missing permissions exit 3 with setup
guidance; the tool never requests root, never resets TCC and never prompts from CI.

## Limits

- Public low-level hooks cannot identify the physical device: while active, **all** mouse
  movement in the session is smoothed, not only the MX Master.
- Elevated (UIPI) applications, secure desktop, remote desktop and games using raw or
  relative input are not smoothed; the tool suspends or passes through instead of trying to
  bypass protections.
- Fractional rendering of the cursor is not exposed by Windows; output is quantised to
  physical pixels and identical positions are deduplicated.
- macOS Quartz delta semantics (accelerated or unaccelerated, suppression behaviour) are
  unverified on hardware; that build is labelled experimental.

## Building

```
cargo build --locked --release --bins
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Diagnostics (Windows only, non-default feature):

```
cargo run --locked --release --features diagnostics --bin cursor-smooth-probe -- source-probe --allow-cursor-move --cycles 8
cargo run --locked --release --features diagnostics --bin cursor-smooth-probe -- fault-check --allow-cursor-move
cargo run --locked --release --bin cursor-smooth-probe -- benchmark --mode idle --seconds 15 --trials 2
cargo run --locked --release --bin cursor-smooth-probe -- benchmark --mode replay-output --seconds 15 --trials 2 --allow-cursor-move
cargo run --locked --release --bin cursor-smooth-probe -- timer-bench --seconds 3
cargo run --locked --release --bin cursor-smooth-probe -- inject-cost --count 4000 --allow-cursor-move [--bare]
```

`source-probe`, `fault-check` and `benchmark --mode replay-output` move the pointer and
require `--allow-cursor-move`; the pointer is restored afterwards.

## Licence

MIT, (c) 2026 merumoa. See [LICENSE](LICENSE).
