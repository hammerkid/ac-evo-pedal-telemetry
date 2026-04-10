# Release Notes

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
