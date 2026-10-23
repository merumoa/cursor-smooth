# User guide

Practical walkthrough for `cursor-smooth` 0.1.0. Read the status table in
[README](../README.md) first: on the author's host the performance gates **fail**, so this
is source you build and measure yourself, not a tuned drop-in.

## 1. Requirements

- Windows 10 1803+ / Windows 11 x64, or macOS 11+ (experimental).
- Rust 1.99.0 (pinned by `rust-toolchain.toml`); `cargo` and `rustup` on `PATH`.
- Windows: no administrator rights. macOS: Accessibility + Input Monitoring grants for the
  terminal or the built binary.

## 2. Build

```sh
git clone https://github.com/merumoa/cursor-smooth
cd cursor-smooth
cargo build --locked --release --bins
```

Binaries: `target/release/cursor-smooth` (product) and `target/release/cursor-smooth-probe`
(diagnostics). The diagnostic probe is built without the `diagnostics` Cargo feature by
default; fault injection needs `--features diagnostics`.

## 3. First run

```sh
cursor-smooth doctor        # read-only: DPI, display, pointer speed, permissions, mutex
cursor-smooth run           # start smoothing; Ctrl+Alt+M toggles, Ctrl+Alt+Esc disables
```

`doctor` never installs a hook. On macOS `run` additionally requires
`--experimental-macos`; without permissions it exits 3 and prints which grant is missing.

A second instance is refused (session mutex). Ctrl+C stops and restores native input.
`run --duration <seconds>` stops by itself; `run --report out.tsv` writes the measurement
report after cleanup and refuses to overwrite an existing file.

## 3.1 Tray companion (Windows)

```pwsh
pwsh -File tray\build.ps1
tray\bin\CursorSmoothTray.exe --settings
```

The tray is optional: the CLI works without it. It shows the engine state, starts and stops
the engine, opens `doctor`, and stores its three settings in
`%APPDATA%\cursor-smooth\tray.json`. Closing the window hides it; **Выход** in the tray menu
stops the engine and closes the icon. Stopping is graceful: the tray owns a named kernel event
and passes `--stop-event <name>` to the engine, so the engine unhooks and restores input before
exiting (if it ignores the signal for three seconds the tray kills it as a last resort).

A desktop shortcut (`CursorSmooth.lnk`, argument `--settings`) is created by
`tray\make-shortcut.ps1`; running it a second time only reopens the settings window.

## 4. What you should observe

- Cursor motion follows the same path the OS produced, delayed by roughly
  `clamp(1.5 * input interval, 4 ms, 40 ms)` — about 12 ms for a 125 Hz mouse.
- No motion means no work: no armed timer, zero wakeups, no CPU burn.
- Holding any mouse button passes motion through untouched until every button is released,
  so clicking and dragging behave natively (drag motion is unsmoothed).
- Any fault (hook loss, overflow, failed injection, cursor teleport) disables smoothing and
  stays latched until you toggle it back on.

## 5. Measure it

```sh
# idle: expects zero wakes and ~0 CPU
cursor-smooth-probe benchmark --mode idle --seconds 60 --trials 3

# replay at 125 Hz into real output (moves the pointer, restored afterwards)
cursor-smooth-probe benchmark --mode replay-output --seconds 60 --trials 3 --allow-cursor-move

# physical: needs you to keep moving the mouse for the whole trial
cursor-smooth-probe benchmark --mode physical --seconds 60 --trials 3 --allow-cursor-move
```

Replay is synthetic input and is labelled as such; only `--mode physical` measures a real
hand. Compare the same trial before/after a code change instead of comparing hardware.

## 6. Diagnostics

```sh
# mapping gate: proves hook.pt - committed reproduces each native delta
cursor-smooth-probe source-probe --allow-cursor-move --cycles 8

# fail-open drills: stall, refused injection, queue overflow, button race, removed hook
cursor-smooth-probe fault-check --allow-cursor-move

# platform limits: bare 1 ms high-resolution timer cadence, per-injection cost
cursor-smooth-probe timer-bench --seconds 3
cursor-smooth-probe inject-cost --count 4000 --allow-cursor-move [--bare]

# pure engine replay, no OS interaction
cursor-smooth-probe replay --self-check
```

Pointer-moving diagnostics need `--allow-cursor-move` and restore the saved position. They
run in a bounded window; do not run two interceptors (the old C# prototype and this binary)
at the same time.

## 7. Troubleshooting

| Symptom | Cause / action |
|---|---|
| `run` exits immediately | Read the fault line; the hook or hotkeys failed to install. Run `doctor`. |
| Cursor jumps or stops smoothing over elevated apps | UIPI: elevation crosses a boundary hooks cannot smooth. Expected; pass-through. |
| Smoothing stops by itself | A guard fault was latched (see the summary line). Toggle with Ctrl+Alt+M after fixing the cause. |
| No smoothing at all in games | Games use raw/relative input; the tool does not bypass that by design. |
| macOS exits 3 | Grant Accessibility and Input Monitoring to the binary/terminal, then rerun. No prompt is ever shown by the tool. |
| Report file rejected | The path already exists; `--report` never overwrites. |

## 8. Limits worth remembering

- Public hooks cannot identify the physical device: while active, all session mouse motion
  is smoothed, not just one model.
- Output is quantised to physical pixels; identical positions are dropped.
- The 1000 Hz figure is software output opportunities with deliberate buffering, not a
  hardware polling-rate change and not a guarantee of 1000 changed pixels per second.
- macOS is experimental: builds and CI tests pass, physical devices, Retina grids,
  multi-display seams and CPU are unverified.
