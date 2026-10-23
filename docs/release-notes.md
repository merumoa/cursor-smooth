# cursor-smooth 0.1.0 - source release, no performance claim yet

This release publishes the source and CI of the Rust rewrite. It deliberately carries no
"works smoothly on every machine" claim:

- Windows: engine, low-level hook, guard and CLI are implemented. The native mapping gate
  (`hook.pt - committed_pointer` reproduces every native delta within 0-2 px over 128
  moves) and all five fault drills pass locally on Windows 11 build 26200.
- Windows performance gates **fail on the author's host**: 741-745 output opportunities/s
  instead of >= 950/s, p99 lateness ~2.3 ms instead of <= 1 ms, worst output gap ~32 ms
  instead of <= 4 ms, and 50-57 CPU ms/s instead of <= 10 ms/s. A bare 1 ms high-resolution
  waitable timer already wakes at only 736-748 Hz there, and `SendInput` plus a low-level
  hook costs tens of microseconds per event. The numbers and commands are in
  `docs/measurements.md`; there is no filtered or averaged pass.
- macOS: builds for arm64 and x86_64, CI compiles/runs the permission-gate and tag-codec
  tests, but the event tap has never been exercised on physical hardware. Retina grids,
  multi-display seams, delta acceleration semantics, wake handling and CPU are unverified.
  `run` requires `--experimental-macos` and exits 3 without permissions.

Idle behaviour is measured and passing: no armed timer and zero wakeups without input.

Nothing here changes DPI, acceleration, firmware or the radio polling rate, and no binary
is signed or notarised.
