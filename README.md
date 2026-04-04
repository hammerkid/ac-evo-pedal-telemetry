# Racing Pedal Overlay

A transparent, borderless, always-on-top HUD for sim racing built with Rust.  
Displays real-time pedal inputs (throttle / brake / clutch), gear, speed, and steering position.

![screenshot](screenshot.png)

## Features

- **Scrolling pedal graph** — 8-second rolling trace: green (throttle), red (brake), blue (clutch)
- **ABS detection** — brake line turns orange when ABS is active (Assetto Corsa / AC Evo via shared memory; other sims via pedal heuristic)
- **Gear indicator** — color shifts green → yellow → red based on engine RPM
- **Speed readout** — km/h from sim telemetry
- **Steering wheel** — miniature rotating wheel icon from hardware axis
- **Sim telemetry** — auto-connects to iRacing, ACC, Assetto Corsa, rFactor 2, Dirt Rally 2 via [`simetry`](https://crates.io/crates/simetry)
- **Hardware input** — reads pedals and wheel via [`gilrs`](https://crates.io/crates/gilrs) (HID/DirectInput)
- **Fully resizable** — all widgets scale proportionally; resize grip appears on hover (bottom-right corner)
- **Borderless & transparent** — rounded-corner overlay with drag-to-move
- **Debug mode** — press `D` to see raw axis codes (useful for mapping new hardware)

## Hardware

Developed and tested with:

- **Moza R3** direct-drive wheel
- **Moza SRP Lite** pedals (2-pedal set)

Other HID gamepads / wheels should work — use the debug overlay (`D`) to identify axis codes.

## Controls

| Input             | Action                                |
|-------------------|---------------------------------------|
| `D`               | Toggle debug overlay                  |
| `Arrow Up / Down` | Simulate throttle / brake (keyboard)  |
| `Space`           | Simulate clutch (keyboard)            |
| `Arrow L / R`     | Simulate steering (keyboard)          |
| Drag anywhere     | Move the overlay window               |
| Hover bottom-right corner | Resize grip (activates after ~500ms) |

## Building

Requires the Rust toolchain. The project pins `stable-x86_64-pc-windows-msvc` via `rust-toolchain.toml`.

```sh
cargo build --release
```

The binary lands in `target/release/racing_pedal_overlay.exe` (or `target/x86_64-pc-windows-msvc/release/` depending on your toolchain).

## Running

```sh
cargo run --release
```

Launch your sim, then start the overlay. It will auto-detect the running sim and begin reading telemetry. Without a sim, the gear shows "N" and speed shows "0" — pedals and steering still work from hardware or keyboard.

## Axis Mapping

Moza devices report as `RawGameController` via Windows Gaming Input, so axes may show as `Unknown`. To map your hardware:

1. Run the overlay and press `D` to open the debug overlay
2. Press each pedal one at a time and note the axis + code
3. Update the `match axis` block in `read_inputs()` in `src/main.rs`

## Architecture

```
┌────────────────────────────────────────────────────────────┐
│  main thread (eframe / egui)                               │
│  ┌──────┐ ┌──────┐ ┌───────┐ ┌───────┐                   │
│  │Graph │ │ Gear │ │ Speed │ │ Wheel │  ← custom widgets  │
│  └──────┘ └──────┘ └───────┘ └───────┘                   │
│       ▲ gilrs events          ▲ mpsc::Receiver            │
└───────┼───────────────────────┼───────────────────────────┘
        │                       │
   USB / HID axis       ┌──────┴──────┐
   (pedals, wheel)      │  telemetry  │ ← background thread
                        │   thread    │   (tokio + simetry)
                        └─────────────┘
```

- **Main thread** — immediate-mode GUI via `eframe`/`egui`. Polls `gilrs` events and draws custom widgets each frame.
- **Telemetry thread** — owns a `tokio` runtime, loops `simetry::connect().await` → `next_moment()`, sends snapshots over `std::sync::mpsc`.
- **AC shared memory** — for Assetto Corsa / AC Evo, reads `Local\acpmf_physics` directly via Win32 FFI to get ABS/TC status and pedal values (not exposed through simetry's `Moment` trait).
- **Scaling** — every font size, stroke width, and margin is proportional to `scale = available_height / 56.0`, so the overlay looks consistent at any size.

## Dependencies

| Crate       | Purpose                              |
|-------------|--------------------------------------|
| `eframe`    | Native GUI framework (egui backend)  |
| `egui_plot` | Scrolling line chart                 |
| `gilrs`     | Cross-platform gamepad/wheel input   |
| `simetry`   | Sim racing telemetry (multi-sim)     |
| `tokio`     | Async runtime for telemetry thread   |
| `uom`       | Unit-of-measure conversions          |

## License

MIT
