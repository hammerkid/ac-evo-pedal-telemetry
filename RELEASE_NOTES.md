# Release Notes

## v0.3.0 — AC Evo 0.6 Fix + Module Decomposition

### Breaking Changes

- **Removed `simetry`, `tokio`, and `uom` dependencies** — telemetry is now read directly from AC Evo shared memory via Win32 FFI in a plain `std::thread`. This simplifies the build and removes ~30 transitive dependencies.

### New

- **AC Evo 0.6+ support** — SHM mapping renamed from `acpmf_physics` to `acevo_pmf_physics`. The overlay tries the new name first and falls back to the old one.
- **Fixed gear display** — corrected offset (16, i32) and encoding (AC: 0=R, 1=N, 2=1st → subtract 1).
- **Fixed speed readout** — corrected offset (28, f32). Was previously reading the fuel field at offset 12.
- **RPM reading** — added engine RPM from offset 20 (i32) for gear color shift.
- **ABS detection** — brake line changes color from red to orange during ABS activation (clean segment splitting by `abs_vibration > 0.05`).
- **TC detection** — throttle line turns yellow during traction control, with a thin purple line showing game-applied throttle.
- **Configurable graph time window** — press `[` / `]` to shrink/grow the scrolling window (2–30 seconds, default 8s).
- **SHM probe subsystem** — scans all known AC mapping names, diffs against baseline, logs to `probe_log.txt`. Visible in debug overlay.
- **Widget visibility toggles** — show/hide individual widgets (Graph, Gear, Speed, Wheel) from the debug panel.
- **Debug overlay gated behind `cfg(debug_assertions)`** — excluded from release builds and git-tracked source.

### Refactored

- **Module decomposition** — split `src/main.rs` (~1440 lines) into 5 files:
  - `src/main.rs` (~310 lines) — app state, entry point, render loop
  - `src/telemetry.rs` (~290 lines) — SHM FFI, telemetry thread, probe infra
  - `src/widgets.rs` (~190 lines) — graph, gear, speed, wheel, resize grip
  - `src/input.rs` (~95 lines) — gamepad input, keyboard fallback
  - `src/debug.rs` (~190 lines) — debug overlay panel (local-only, not in git)

### Fixed

- ABS detection broken after AC Evo Update 0.6 (SHM name change)
- Gear showing wrong number (was reading wrong offset, wrong encoding)
- Speed showing wrong value (was reading fuel field instead of speedKmh)
- ABS rendering artifacts (barcode pattern from fill-to-zero glow) replaced with clean segment color change

## v0.2.0 — Pedal Axis Inversion

### New

- **Per-pedal axis inversion toggles** — press `D` to open the debug overlay, then click `T inv` / `B inv` / `C inv` to flip the direction of throttle, brake, or clutch individually. Fixes pedals that read 100% when released and 0% when pressed on non-Moza hardware.

### Defaults

| Pedal    | Default    | Reason                            |
|----------|------------|-----------------------------------|
| Throttle | Direct     | Moza SRP Lite: pressed = +1.0     |
| Brake    | Inverted   | Moza SRP Lite: pressed = −1.0     |
| Clutch   | Direct     | Moza SRP Lite: pressed = +1.0     |

### Notes

- Inversion settings are **runtime-only** and reset when the overlay restarts.
- Toggle buttons highlight orange when active.
- No code changes needed for most non-Moza pedals — just toggle the inversion in the debug panel.
