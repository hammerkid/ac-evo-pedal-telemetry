//! Gamepad/wheel/pedal hardware input and keyboard fallback.

use eframe::egui::{Context, Key};
use gilrs::{Axis, EventType};

use crate::OverlayApp;
use crate::{MIN_HISTORY_SECONDS, MAX_HISTORY_SECONDS};

/// Convert raw axis value (−1..+1) to pedal 0..1.
/// **Inverted**: unpressed = +1.0, fully pressed = −1.0 (some HID pedals).
pub fn axis_to_pedal_inv(value: f32) -> f32 {
    ((1.0 - value) * 0.5).clamp(0.0, 1.0)
}

/// **Direct**: unpressed = −1.0, fully pressed = +1.0 (Moza SRP Lite, etc.).
pub fn axis_to_pedal(value: f32) -> f32 {
    ((value + 1.0) * 0.5).clamp(0.0, 1.0)
}

/// Polls gilrs for gamepad/wheel/pedal axis events and reads keyboard
/// fallback inputs.  The axis→pedal mapping is tuned for the Moza R3 +
/// SRP Lite combo; press **D** to see raw axis codes for other hardware.
pub fn read_inputs(app: &mut OverlayApp, ctx: &Context) {
    if ctx.input(|i| i.key_pressed(Key::D)) {
        app.debug_mode = !app.debug_mode;
    }
    // [ / ] keys adjust graph time window
    if ctx.input(|i| i.key_pressed(Key::OpenBracket)) {
        app.history_seconds = (app.history_seconds - 1.0).max(MIN_HISTORY_SECONDS);
    }
    if ctx.input(|i| i.key_pressed(Key::CloseBracket)) {
        app.history_seconds = (app.history_seconds + 1.0).min(MAX_HISTORY_SECONDS);
    }

    // --- gilrs hardware input ---
    if let Some(gilrs) = &mut app.gilrs {
        while let Some(event) = gilrs.next_event() {
            if let EventType::AxisChanged(axis, value, code) = event.event {
                if app.debug_mode {
                    app.debug_log.push_back(format!(
                        "{:?}  code:{:?}  val:{:+.3}",
                        axis, code, value
                    ));
                    while app.debug_log.len() > 12 {
                        app.debug_log.pop_front();
                    }
                }

                match axis {
                    Axis::LeftZ | Axis::LeftStickY => {
                        app.throttle = if app.invert_throttle {
                            axis_to_pedal_inv(value)
                        } else {
                            axis_to_pedal(value)
                        };
                    }
                    Axis::RightZ | Axis::RightStickY => {
                        app.brake = if app.invert_brake {
                            axis_to_pedal_inv(value)
                        } else {
                            axis_to_pedal(value)
                        };
                    }
                    Axis::RightStickX => {
                        app.clutch = if app.invert_clutch {
                            axis_to_pedal_inv(value)
                        } else {
                            axis_to_pedal(value)
                        };
                    }
                    _ => app.steering = value,
                }
            }
        }
    }

    // --- Keyboard fallback (for testing without hardware) ---
    let kb_t = if ctx.input(|i| i.key_down(Key::ArrowUp)) { 1.0_f32 } else { 0.0 };
    let kb_b = if ctx.input(|i| i.key_down(Key::ArrowDown)) { 1.0_f32 } else { 0.0 };
    let kb_c = if ctx.input(|i| i.key_down(Key::Space)) { 1.0_f32 } else { 0.0 };
    app.throttle = app.throttle.max(kb_t);
    app.brake = app.brake.max(kb_b);
    app.clutch = app.clutch.max(kb_c);

    if ctx.input(|i| i.key_down(Key::ArrowLeft)) {
        app.steering = -1.0;
    } else if ctx.input(|i| i.key_down(Key::ArrowRight)) {
        app.steering = 1.0;
    }
}
