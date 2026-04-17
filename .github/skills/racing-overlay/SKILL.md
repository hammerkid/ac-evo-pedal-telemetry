---
name: racing-overlay
description: "Use when modifying the Racing Pedal Overlay — a transparent sim racing HUD built with Rust, eframe/egui, and gilrs. Covers: pedal graph rendering, ABS/TC detection, AC Evo shared memory FFI, telemetry threading, widget scaling, overlay window management, axis mapping, and egui_plot usage."
---

 - when build check if app running, if yes, close it before build, and after build, start it again

# Racing Pedal Overlay — Development Skill

## Project Overview

Single-binary transparent always-on-top HUD for sim racing. Displays a scrolling pedal graph, gear indicator, speed readout, and steering wheel icon. Split into 5 source files under `src/`.

**Target platform:** Windows (uses Win32 FFI for AC shared memory + monitor size).  
**Toolchain:** `stable-x86_64-pc-windows-msvc` (pinned in `rust-toolchain.toml`).  
**Build:** `cargo build --release` — binary at `target/release/racing_pedal_overlay.exe`.

## Architecture

```
Main thread (eframe/egui)          Background thread (std::thread)
┌──────────────────────┐           ┌──────────────────────┐
│ OverlayApp::update() │◄─mpsc────│ spawn_telemetry_thread│
│  input::read_inputs()│          │  read_ac_shared_physics│
│  update_history()    │          │  probe_all_shm()      │
│  poll_telemetry()    │          └──────────────────────┘
│  widgets::draw_*()   │
│  debug::draw_debug() │
└──────────────────────┘
```

## Source File Map

| File | Lines | Purpose |
|------|-------|---------|
| `src/main.rs` | ~310 | App state, entry point, `OverlayApp` struct, `update_history()`, `poll_telemetry()`, `App::update()` |
| `src/telemetry.rs` | ~290 | `TelemetryData`, AC SHM FFI (`read_ac_shared_physics()`), SHM probe infra, `spawn_telemetry_thread()` |
| `src/widgets.rs` | ~230 | `draw_graph()`, `draw_gear()`, `draw_speed()`, `draw_wheel()`, `draw_resize_grip()` — all free fns |
| `src/input.rs` | ~95 | `axis_to_pedal()`, `axis_to_pedal_inv()`, `read_inputs()` — gilrs + keyboard |
| `src/debug.rs` | ~190 | `draw_debug_overlay()` — floating debug panel with axis log, SHM probe, toggles. **Local-only, gated behind `cfg(debug_assertions)`, excluded from git.** |

## Key Structs & Functions

| Item | File | Purpose |
|------|------|---------|
| `TelemetryData` | telemetry.rs | Data packet from telemetry thread → UI via mpsc |
| `AcPhysicsSnapshot` | telemetry.rs | Fields read from AC shared memory (FFI) |
| `read_ac_shared_physics()` | telemetry.rs | Win32 FFI: OpenFileMappingA → reads `Local\acevo_pmf_physics` |
| `spawn_telemetry_thread()` | telemetry.rs | Background thread: 60 Hz SHM poll loop, sends TelemetryData |
| `probe_all_shm()` | telemetry.rs | Scans all known SHM names, logs non-zero fields |
| `Sample` | main.rs | Single point in pedal history ring buffer |
| `OverlayApp` | main.rs | Root app state: hardware inputs, history, telemetry, UI |
| `OverlayApp::new()` | main.rs | Constructor: transparent visuals, spawns telemetry thread |
| `read_inputs()` | input.rs | Polls gilrs axis events + keyboard fallback |
| `update_history()` | main.rs | Records pedal samples at SAMPLE_RATE_HZ (60 Hz) |
| `poll_telemetry()` | main.rs | Drains mpsc, updates gear/speed/ABS state, clears stale |
| `draw_graph()` | widgets.rs | Scrolling pedal trace with ABS/TC colored segments |
| `draw_gear()` | widgets.rs | Gear indicator with RPM color shift |
| `draw_speed()` | widgets.rs | Speed readout in km/h |
| `draw_wheel()` | widgets.rs | Miniature rotating steering wheel |
| `draw_resize_grip()` | widgets.rs | Bottom-right resize with 500ms hover delay |
| `draw_debug_overlay()` | debug.rs | Debug panel: axis log, SHM probe, inversion/visibility toggles |

## Critical Design Patterns

### Scaling
All widget sizes, fonts, strokes, and margins are proportional to:
```rust
let scale = available_height / 56.0;  // 56 = reference height in px
```
Never use absolute pixel values for widget internals. Always multiply by `scale`.

### ABS Detection
- **AC shared memory** (AC Evo 0.6+):
  - SHM name: `Local\acevo_pmf_physics` (falls back to `Local\acpmf_physics`)
  - Field at offset 252 (`abs_field`, f32, 0.0–1.0) — oscillates during ABS intervention
  - Threshold: `> 0.01` triggers ABS active; raw float stored as `abs_vibration` for graph coloring
  - TC field at offset 204 (f32) — `> 0.5` triggers TC active
  - `abs_source = "shm"`

### ABS in the Graph
- `abs_vibration` (f32, 0.0–1.0) is stored per `Sample` — raw per-tick signal, NOT smoothed
- The `abs_held` bool (300ms hold timer) exists only for the debug overlay text — NOT used for graph coloring
- Brake line: segment-split by `abs_vibration > 0.05` threshold — red (normal) vs orange (ABS active), same line width
- Throttle line: segment-split by `tc_active` bool — green (normal) vs yellow (TC active), same line width; wide semi-transparent glow behind TC segments
- Segments use bridge points so there are no gaps between color transitions
- **No fill-to-zero, no per-sample gradient** — these cause barcode/artifact patterns

### Telemetry Thread
- Pure `std::thread::spawn` — no tokio, no async
- Polls `read_ac_shared_physics()` at ~60 Hz via 16ms sleep loop
- Also runs SHM probe subsystem that scans all known mapping names and diffs against baseline
- Data sent via `std::sync::mpsc::channel` — UI drains to latest snapshot each frame

### AC Shared Memory Layout (acevo_pmf_physics)
All fields packed(4), offsets in bytes:
- 4: gas (f32)
- 8: brake (f32)
- 12: fuel (f32)
- 16: gear (i32) — AC uses 0=R, 1=N, 2=1st, 3=2nd...
- 20: rpms (i32)
- 24: steerAngle (f32)
- 28: speedKmh (f32)
- 204: tc field (f32) — 0.0/1.0 oscillation
- 252: abs field (f32) — 0.0–1.0 vibration intensity
- 364: clutch (f32)

### Gear Encoding
AC reports: 0=Reverse, 1=Neutral, 2=1st gear, etc. The telemetry thread subtracts 1 before sending: `(snap.gear - 1) as i8` → -1=R, 0=N, 1=1st...

### Window Setup
- Borderless, transparent, always-on-top via `egui::ViewportBuilder`
- Sized to ~26% of monitor width, 5:1 aspect ratio
- Positioned at bottom-center, 4% above screen edge
- Draggable anywhere (entire panel is a drag zone)
- Resize via bottom-right grip (visible after 500ms hover)

### Graph Time Window
- Default 8 seconds (`DEFAULT_HISTORY_SECONDS`)
- User-adjustable via `[` / `]` keys (2–30 seconds range)
- History buffer trimmed on each `update_history()` call

## Common Modification Scenarios

### Adding a new data field from the sim
1. Add field to `TelemetryData` in `telemetry.rs`
2. Populate it in `spawn_telemetry_thread()` — read from AC shared memory at the correct offset
3. Add corresponding field to `OverlayApp` in `main.rs`
4. Read it in `poll_telemetry()` (and clear in the stale-data branch)
5. Display it in `widgets.rs` or create a new `draw_*()` function

### Adding a new widget
1. Create `pub fn draw_widget(ui: &mut Ui, ..., h: f32, w: f32)` in `widgets.rs`
2. Use `scale = h / 56.0` for all sizing
3. Allocate space with `ui.allocate_exact_size()`
4. Paint with `ui.painter()` — use `p.rect()`, `p.galley()`, `p.circle_stroke()`, etc.
5. Add to the horizontal layout in `App::update()` in `main.rs` — account for width in `fixed` calculation
6. Add visibility toggle in `debug.rs` (`vis_btn()`)

### Changing axis mapping
- The `match axis` block in `input.rs` maps gilrs axes to throttle/brake/clutch/steering
- Use debug mode (`D`) to identify axis codes for different hardware
- `axis_to_pedal()` = direct (pressed = +1.0), `axis_to_pedal_inv()` = inverted (pressed = -1.0)

### Modifying the scrolling graph
- History buffer: `VecDeque<Sample>`, capped to `history_seconds` (default 8s, adjustable 2–30s)
- Sample rate: `SAMPLE_RATE_HZ` (60 Hz) — controlled in `update_history()`
- Rendering: `egui_plot::Plot` with `Line` segments, X axis = relative time, Y axis = 0–100%
- Brake/throttle segments split by ABS/TC state for color changes — modify the segment-building loops in `draw_graph()` in `widgets.rs`

## Dependencies & Versions

| Crate | Version | Notes |
|-------|---------|-------|
| eframe | 0.30 | egui native backend |
| egui_plot | 0.30 | Must match eframe major version |
| gilrs | 0.11 | Gamepad/wheel HID input |

## Gotchas

- **AC Evo 0.6+ SHM name change**: Mapping renamed from `acpmf_physics` to `acevo_pmf_physics`. The code tries the new name first, falls back to old.
- **AC Evo integer ABS fields broken**: The i32 fields `abs_in_action` (offset 676) / `tc_in_action` (offset 672) are always 0 in AC Evo — use the float fields at offsets 252 (abs) and 204 (tc) instead.
- **Gear off-by-one**: AC uses 0=R, 1=N, 2=1st. Must subtract 1 for correct display.
- **gilrs axis variance**: Different wheels report on different axes; Moza devices show as `RawGameController` with `Unknown` axis types
- **Transparent window**: `clear_color` must return `[0,0,0,0]` and panel_fill must be `TRANSPARENT` — otherwise rounded corners show opaque background
- **No simetry/tokio**: These dependencies were removed in v0.3.0. All telemetry is via direct Win32 SHM FFI in a plain `std::thread`.
