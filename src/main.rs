//! # Racing Pedal Overlay
//!
//! A transparent, always-on-top HUD for sim racing that displays real-time
//! pedal inputs, gear, speed, and steering position. Reads hardware via
//! [`gilrs`] and sim telemetry via [`simetry`]. Detects ABS/TC activation
//! and colors the brake line orange on the scrolling graph.
//!
//! ## Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────┐
//! │  main thread (eframe / egui)                               │
//! │  ┌──────┐ ┌──────┐ ┌───────┐ ┌───────┐                   │
//! │  │Graph │ │ Gear │ │ Speed │ │ Wheel │  <- custom widgets │
//! │  └──────┘ └──────┘ └───────┘ └───────┘                   │
//! │       ▲ gilrs events          ▲ mpsc::Receiver            │
//! └───────┼───────────────────────┼───────────────────────────┘
//!         │                       │
//!    USB / HID axis       ┌──────┴──────┐
//!    (pedals, wheel)      │ telemetry   │ <- background thread
//!                         │ thread      │    (tokio + simetry)
//!                         └─────────────┘
//!              + direct AC shared memory (Win32 FFI)
//! ```
//!
//! ## Controls
//!
//! | Key              | Action                          |
//! |------------------|---------------------------------|
//! | `D`              | Toggle debug overlay            |
//! | `Arrow Up/Down`  | Simulate throttle / brake       |
//! | `Space`          | Simulate clutch                 |
//! | `Arrow L/R`      | Simulate steering               |
//! | Drag window      | Move overlay                    |
//! | Hover BR corner  | Resize grip (activates ~500ms)  |

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::mpsc;
use std::time::Instant;

use eframe::egui::{
    self, Align, Color32, Context, Frame, Key, Layout, RichText, Rounding, Stroke, Ui, Vec2,
};
use eframe::{App, NativeOptions};
use egui::viewport::ResizeDirection;
use egui_plot::{Line, Plot, PlotPoints};
use gilrs::{Axis, EventType, Gilrs};

/// How many seconds of pedal history the graph displays (scrolling window).
const HISTORY_SECONDS: f64 = 8.0;

/// Target sample rate for recording pedal values into the history buffer.
const SAMPLE_RATE_HZ: f64 = 60.0;

// ─────────────────────────────────────────────────────────────────────────────
// Sim Telemetry — background thread
// ─────────────────────────────────────────────────────────────────────────────

/// Data packet sent from the telemetry thread to the UI via [`mpsc`] channel.
struct TelemetryData {
    sim_name: String,
    gear: Option<i8>,
    speed_kmh: Option<f64>,
    rpm: Option<f64>,
    max_rpm: Option<f64>,
    abs_active: bool,
    tc_active: bool,
    /// Pedal values for debug logging.
    pedals_game: Option<(f64, f64, f64)>,
    pedals_raw: Option<(f64, f64, f64)>,
    /// Which method was used to detect ABS (for debug display).
    abs_source: &'static str,
}

// ─────────────────────────────────────────────────────────────────────────────
// AC shared memory reader — direct Win32 FFI
//
// simetry's Moment trait for Assetto Corsa doesn't expose pedals() or
// abs_in_action. But the data IS in AC's shared memory (acpmf_physics).
// We open the same mapping ourselves and read the fields we need.
// ─────────────────────────────────────────────────────────────────────────────

/// Snapshot of fields we read directly from AC's physics shared memory.
struct AcPhysicsSnapshot {
    gas: f32,
    brake: f32,
    clutch: f32,
    abs_active: bool,
    tc_active: bool,
}

/// Read ABS status and pedal values from AC's shared memory page.
/// Works for AC classic and AC Evo (same shared memory layout).
/// Returns None if the shared memory isn't available (game not running).
#[cfg(windows)]
fn read_ac_shared_physics() -> Option<AcPhysicsSnapshot> {
    // Win32 FFI — OpenFileMappingA / MapViewOfFile / UnmapViewOfFile / CloseHandle
    extern "system" {
        fn OpenFileMappingA(access: u32, inherit: i32, name: *const u8) -> isize;
        fn MapViewOfFile(h: isize, access: u32, off_hi: u32, off_lo: u32, bytes: usize) -> *mut u8;
        fn UnmapViewOfFile(base: *const u8) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const FILE_MAP_READ: u32 = 4;

    unsafe {
        let handle = OpenFileMappingA(
            FILE_MAP_READ,
            0,
            b"Local\\acpmf_physics\0".as_ptr(),
        );
        if handle == 0 {
            return None;
        }
        let ptr = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0);
        if ptr.is_null() {
            CloseHandle(handle);
            return None;
        }

        // Field offsets (bytes) from PageFilePhysics #[repr(C, packed(4))].
        // All fields are i32/f32/[f32;N] — no padding in packed(4).
        let gas            = *(ptr.add(4)   as *const f32);   // offset 4
        let brake          = *(ptr.add(8)   as *const f32);   // offset 8
        let clutch         = *(ptr.add(364) as *const f32);   // offset 364

        // AC Evo: the i32 fields tc_in_action (672) / abs_in_action (676) are
        // always 0. Instead the f32 at offset 252 ("abs") and 204 ("tc")
        // oscillate between 0.0 and 1.0 when the assist is intervening.
        let abs_field      = *(ptr.add(252) as *const f32);   // offset 252
        let tc_field       = *(ptr.add(204) as *const f32);   // offset 204

        UnmapViewOfFile(ptr);
        CloseHandle(handle);

        Some(AcPhysicsSnapshot {
            gas,
            brake,
            clutch,
            abs_active: abs_field > 0.5,
            tc_active: tc_field > 0.5,
        })
    }
}

#[cfg(not(windows))]
fn read_ac_shared_physics() -> Option<AcPhysicsSnapshot> {
    None // AC shared memory is Windows-only
}

/// Spawns a background thread that continuously connects to any running sim
/// (iRacing, ACC, Assetto Corsa, rFactor 2, Dirt Rally 2, etc.) via the
/// [`simetry`] crate. Telemetry snapshots are sent over an [`mpsc`] channel.
///
/// The thread owns its own [`tokio`] runtime because `simetry` is async, but
/// the rest of the app is synchronous (egui's immediate-mode loop).
fn spawn_telemetry_thread() -> mpsc::Receiver<TelemetryData> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("telemetry: failed to create runtime: {e}");
                return;
            }
        };
        rt.block_on(async {
            loop {
                // simetry::connect() blocks until a sim is detected
                let mut client = simetry::connect().await;
                let name = client.name().to_owned();
                while let Some(moment) = client.next_moment().await {
                    use uom::si::angular_velocity::revolution_per_minute;
                    use uom::si::velocity::kilometer_per_hour;

                    // Try AC shared memory first (has abs_in_action directly)
                    let ac_snap = if name == "AssettoCorsa" || name == "AssettoCorsaCompetizione" {
                        read_ac_shared_physics()
                    } else {
                        None
                    };

                    // ABS detection: prefer AC shared memory, fall back to pedals heuristic
                    let (abs_active, tc_active, abs_source);
                    if let Some(ref snap) = ac_snap {
                        abs_active = snap.abs_active;
                        tc_active = snap.tc_active;
                        abs_source = "shm";
                    } else {
                        let pg = moment.pedals().map(|p| (p.throttle, p.brake, p.clutch));
                        let pr = moment.pedals_raw().map(|p| (p.throttle, p.brake, p.clutch));
                        abs_active = match (pg, pr) {
                            (Some(game), Some(raw)) if raw.1 > 0.01 => game.1 < raw.1 * 0.95,
                            _ => false,
                        };
                        tc_active = false;
                        abs_source = if pg.is_some() && pr.is_some() { "pedals" } else { "none" };
                    }

                    // Pedal values: prefer AC shm, fall back to Moment::pedals()
                    let pedals_game = ac_snap
                        .as_ref()
                        .map(|s| (s.gas as f64, s.brake as f64, s.clutch as f64))
                        .or_else(|| moment.pedals().map(|p| (p.throttle, p.brake, p.clutch)));
                    let pedals_raw = moment.pedals_raw().map(|p| (p.throttle, p.brake, p.clutch));

                    let data = TelemetryData {
                        sim_name: name.clone(),
                        gear: moment.vehicle_gear(),
                        speed_kmh: moment
                            .vehicle_velocity()
                            .map(|v| v.get::<kilometer_per_hour>()),
                        rpm: moment
                            .vehicle_engine_rotation_speed()
                            .map(|v| v.get::<revolution_per_minute>()),
                        max_rpm: moment
                            .vehicle_max_engine_rotation_speed()
                            .map(|v| v.get::<revolution_per_minute>()),
                        abs_active,
                        tc_active,
                        pedals_game,
                        pedals_raw,
                        abs_source,
                    };
                    if tx.send(data).is_err() {
                        return; // receiver dropped, app is closing
                    }
                }
            }
        });
    });
    rx
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Racing Pedal Overlay")
            .with_inner_size([500.0, 100.0])
            .with_decorations(false)   // borderless — we draw our own frame
            .with_resizable(true)
            .with_transparent(true)    // lets rounded corners show through
            .with_always_on_top(),     // stays above the sim window
        ..Default::default()
    };

    eframe::run_native(
        "Racing Pedal Overlay",
        options,
        Box::new(|cc| Ok(Box::new(OverlayApp::new(cc)))),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Application state
// ─────────────────────────────────────────────────────────────────────────────

/// A single point in the pedal history timeline.
#[derive(Clone, Copy)]
struct Sample {
    t: f64,
    throttle: f64,
    brake: f64,
    clutch: f64,
    abs_active: bool,
}

/// Root application state. Holds hardware input values, the scrolling history
/// buffer, sim telemetry channel, and transient UI state.
struct OverlayApp {
    // Hardware input (gilrs)
    gilrs: Option<Gilrs>,
    throttle: f32,
    brake: f32,
    clutch: f32,
    steering: f32, // −1.0 full left … 0.0 center … +1.0 full right

    // Scrolling graph history (ring buffer, capped to HISTORY_SECONDS)
    history: VecDeque<Sample>,
    start_time: Instant,
    last_sample_time: f64,

    // Sim telemetry (received from background thread)
    telemetry_rx: mpsc::Receiver<TelemetryData>,
    gear: Option<i8>,
    speed_kmh: Option<f64>,
    rpm_pct: f32, // 0.0 … 1.0 (engine RPM / max RPM)
    abs_active: bool,
    /// ABS hold: stays true for 300ms after last ABS pulse to avoid flicker
    abs_held: bool,
    abs_hold_until: Option<Instant>,
    tc_active: bool,
    abs_source: &'static str,
    /// Debug: last pedal values from telemetry (game-modified)
    pedals_game_dbg: Option<(f64, f64, f64)>,
    /// Debug: last raw pedal values from telemetry
    pedals_raw_dbg: Option<(f64, f64, f64)>,
    sim_name: String,
    last_telemetry: Option<Instant>,

    // UI state
    debug_mode: bool,
    debug_log: VecDeque<String>,
    resize_hover_start: Option<Instant>,
}

impl OverlayApp {
    /// Constructs the app, configures transparent visuals, and kicks off the
    /// telemetry background thread.
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Override egui's default dark theme to be fully transparent so the
        // GPU clear color shows through the rounded corners.
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::TRANSPARENT;
        visuals.window_fill = Color32::TRANSPARENT;
        visuals.window_stroke = Stroke::NONE;
        visuals.override_text_color = Some(Color32::from_rgb(220, 225, 232));
        cc.egui_ctx.set_visuals(visuals);

        let telemetry_rx = spawn_telemetry_thread();

        Self {
            gilrs: Gilrs::new().ok(),
            throttle: 0.0,
            brake: 0.0,
            clutch: 0.0,
            steering: 0.0,
            history: VecDeque::new(),
            start_time: Instant::now(),
            last_sample_time: 0.0,
            telemetry_rx,
            gear: None,
            speed_kmh: None,
            rpm_pct: 0.0,
            abs_active: false,
            abs_held: false,
            abs_hold_until: None,
            tc_active: false,
            abs_source: "none",
            pedals_game_dbg: None,
            pedals_raw_dbg: None,
            sim_name: String::new(),
            last_telemetry: None,
            debug_mode: false,
            debug_log: VecDeque::new(),
            resize_hover_start: None,
        }
    }

    /// Convert raw axis value (-1.0 to 1.0) to pedal % (0.0 to 1.0).
    /// Inverted: unpressed = +1.0, fully pressed = -1.0 (some HID pedals).
    fn axis_to_pedal_inv(value: f32) -> f32 {
        ((1.0 - value) * 0.5).clamp(0.0, 1.0)
    }

    /// Direct: unpressed = -1.0, fully pressed = +1.0 (Moza SRP Lite, etc.).
    fn axis_to_pedal(value: f32) -> f32 {
        ((value + 1.0) * 0.5).clamp(0.0, 1.0)
    }

    // ─────────────────────────────────────────────────────────────────────
    // Input handling
    // ─────────────────────────────────────────────────────────────────────

    /// Polls gilrs for gamepad/wheel/pedal axis events and reads keyboard
    /// fallback inputs. The axis→pedal mapping is tuned for the Moza R3 +
    /// SRP Lite combo; press D to see raw axis codes for other hardware.
    fn read_inputs(&mut self, ctx: &Context) {
        if ctx.input(|i| i.key_pressed(Key::D)) {
            self.debug_mode = !self.debug_mode;
        }

        // --- gilrs hardware input ---
        if let Some(gilrs) = &mut self.gilrs {
            while let Some(event) = gilrs.next_event() {
                if let EventType::AxisChanged(axis, value, code) = event.event {
                    if self.debug_mode {
                        self.debug_log.push_back(format!(
                            "{:?}  code:{:?}  val:{:+.3}",
                            axis, code, value
                        ));
                        while self.debug_log.len() > 12 {
                            self.debug_log.pop_front();
                        }
                    }

                    match axis {
                        // Moza SRP Lite: throttle on LeftZ/LeftStickY,
                        // brake on RightZ/RightStickY. Uses direct conversion
                        // (pressed = +1.0). Press D to verify your mapping.
                        Axis::LeftZ | Axis::LeftStickY => {
                            self.throttle = Self::axis_to_pedal(value);
                        }
                        Axis::RightZ | Axis::RightStickY => {
                            self.brake = Self::axis_to_pedal_inv(value);
                        }
                        Axis::RightStickX => {
                            self.clutch = Self::axis_to_pedal(value);
                        }
                        // Steering: Moza R3 may report on LeftStickX, DPadX,
                        // or Unknown. Catch all remaining axes as steering.
                        _ => self.steering = value,
                    }
                }
            }
        }

        // --- Keyboard fallback (for testing without hardware) ---
        // Pedals: keyboard overrides via max (gilrs keeps its value)
        let kb_t = if ctx.input(|i| i.key_down(Key::ArrowUp)) { 1.0_f32 } else { 0.0 };
        let kb_b = if ctx.input(|i| i.key_down(Key::ArrowDown)) { 1.0_f32 } else { 0.0 };
        let kb_c = if ctx.input(|i| i.key_down(Key::Space)) { 1.0_f32 } else { 0.0 };
        self.throttle = self.throttle.max(kb_t);
        self.brake = self.brake.max(kb_b);
        self.clutch = self.clutch.max(kb_c);

        // Steering: arrow keys override
        if ctx.input(|i| i.key_down(Key::ArrowLeft)) {
            self.steering = -1.0;
        } else if ctx.input(|i| i.key_down(Key::ArrowRight)) {
            self.steering = 1.0;
        }
    }

    /// Records the current pedal values into the scrolling history buffer
    /// at [`SAMPLE_RATE_HZ`]. Old samples beyond [`HISTORY_SECONDS`] are
    /// discarded from the front.
    fn update_history(&mut self) {
        let now = self.start_time.elapsed().as_secs_f64();
        if now - self.last_sample_time < 1.0 / SAMPLE_RATE_HZ {
            return;
        }
        self.last_sample_time = now;
        self.history.push_back(Sample {
            t: now,
            throttle: self.throttle as f64,
            brake: self.brake as f64,
            clutch: self.clutch as f64,
            abs_active: self.abs_held,
        });
        while let Some(front) = self.history.front() {
            if now - front.t > HISTORY_SECONDS {
                self.history.pop_front();
            } else {
                break;
            }
        }
    }

    /// Drains the [`mpsc`] channel from the telemetry thread, keeps only the
    /// latest snapshot, and clears stale data if no update arrives for 2 s.
    fn poll_telemetry(&mut self) {
        let mut latest = None;
        while let Ok(data) = self.telemetry_rx.try_recv() {
            latest = Some(data);
        }
        if let Some(data) = latest {
            self.gear = data.gear;
            self.speed_kmh = data.speed_kmh;
            self.rpm_pct = match (data.rpm, data.max_rpm) {
                (Some(rpm), Some(max)) if max > 0.0 => (rpm / max).clamp(0.0, 1.0) as f32,
                _ => 0.0,
            };
            self.abs_active = data.abs_active;
            // Hold ABS for 300ms after last pulse to smooth rapid on/off
            if data.abs_active {
                self.abs_hold_until = Some(Instant::now() + std::time::Duration::from_millis(300));
                self.abs_held = true;
            } else if self.abs_hold_until.map(|t| Instant::now() >= t).unwrap_or(true) {
                self.abs_held = false;
            }
            self.tc_active = data.tc_active;
            self.abs_source = data.abs_source;
            self.pedals_game_dbg = data.pedals_game;
            self.pedals_raw_dbg = data.pedals_raw;
            self.sim_name = data.sim_name;
            self.last_telemetry = Some(Instant::now());
        }
        // Clear stale data (no updates in 2 seconds)
        if let Some(last) = self.last_telemetry {
            if last.elapsed().as_secs_f32() > 2.0 {
                self.gear = None;
                self.speed_kmh = None;
                self.rpm_pct = 0.0;
                self.abs_active = false;
                self.abs_held = false;
                self.abs_hold_until = None;
                self.tc_active = false;
                self.abs_source = "none";
                self.sim_name.clear();
                self.last_telemetry = None;
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Custom widgets — all sizes are proportional to `h` (available height)
    // so the entire overlay scales when resized.
    //
    // Reference height = 56 px  →  scale = h / 56.0
    // ─────────────────────────────────────────────────────────────────────

    /// Scrolling pedal trace graph.
    /// Green = throttle, Red/Orange = brake (orange when ABS active), Blue = clutch.
    fn draw_graph(&self, ui: &mut Ui, width: f32, height: f32) {
        let now = self.start_time.elapsed().as_secs_f64();
        let t_pts: PlotPoints = self.history.iter().map(|s| [s.t - now, s.throttle * 100.0]).collect();
        let c_pts: PlotPoints = self.history.iter().map(|s| [s.t - now, s.clutch * 100.0]).collect();
        let scale = height / 56.0;
        let line_w = (2.0 * scale).clamp(1.0, 5.0);

        // Split brake history into segments by ABS state
        let brake_normal_color = Color32::from_rgb(240, 55, 50);
        let brake_abs_color = Color32::from_rgb(255, 160, 20);
        let mut brake_segments: Vec<(Vec<[f64; 2]>, bool)> = Vec::new();
        for s in &self.history {
            let pt = [s.t - now, s.brake * 100.0];
            match brake_segments.last_mut() {
                Some((pts, abs)) if *abs == s.abs_active => pts.push(pt),
                Some((pts, _prev_abs)) => {
                    // Bridge: repeat last point so segments connect visually
                    let bridge = *pts.last().unwrap();
                    brake_segments.push((vec![bridge, pt], s.abs_active));
                }
                None => brake_segments.push((vec![pt], s.abs_active)),
            }
        }

        Frame::default()
            .fill(Color32::from_rgba_unmultiplied(12, 14, 20, 240))
            .stroke(Stroke::new(0.5, Color32::from_rgba_unmultiplied(60, 70, 100, 80)))
            .rounding(Rounding::same((6.0 * scale).max(2.0)))
            .inner_margin(egui::Margin::symmetric(1.0, 0.0))
            .show(ui, |ui| {
                Plot::new("pedal_plot")
                    .allow_zoom(false)
                    .allow_drag(false)
                    .allow_boxed_zoom(false)
                    .allow_scroll(false)
                    .allow_double_click_reset(false)
                    .show_axes([false, false])
                    .show_grid([false, false])
                    .show_background(false)
                    .set_margin_fraction(Vec2::new(0.0, 0.02))
                    .include_x(-HISTORY_SECONDS)
                    .include_x(0.0)
                    .include_y(0.0)
                    .include_y(100.0)
                    .height(height)
                    .width(width)
                    .show(ui, |plot_ui| {
                        plot_ui.line(Line::new(t_pts).color(Color32::from_rgb(100, 220, 70)).width(line_w));
                        for (pts, abs) in &brake_segments {
                            let color = if *abs { brake_abs_color } else { brake_normal_color };
                            plot_ui.line(Line::new(PlotPoints::new(pts.clone())).color(color).width(line_w));
                        }
                        plot_ui.line(Line::new(c_pts).color(Color32::from_rgb(60, 130, 255)).width(line_w * 0.75));
                    });
            });
    }

    /// Current gear indicator. Color shifts green → yellow → red based on RPM.
    /// Shows "N" when no telemetry is connected.
    fn draw_gear(ui: &mut Ui, gear: Option<i8>, rpm_pct: f32, h: f32, w: f32) {
        let text: String = match gear {
            Some(g) if g < 0 => "R".into(),
            Some(0) => "N".into(),
            Some(g) => format!("{}", g),
            None => "N".into(),
        };
        let color = if gear.is_none() {
            Color32::from_gray(70)
        } else if rpm_pct > 0.9 {
            Color32::from_rgb(255, 40, 40)
        } else if rpm_pct > 0.7 {
            Color32::from_rgb(255, 210, 30)
        } else {
            Color32::from_rgb(50, 215, 50)
        };
        let scale = h / 56.0;

        let size = Vec2::new(w, h);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let p = ui.painter();

        p.rect(
            rect,
            Rounding::same((6.0 * scale).max(2.0)),
            Color32::from_rgba_unmultiplied(18, 22, 30, 230),
            Stroke::new(0.5, Color32::from_rgba_unmultiplied(80, 90, 110, 80)),
        );

        let font_size = (36.0 * scale).clamp(12.0, 96.0);
        let galley = p.layout_no_wrap(
            text,
            egui::FontId::new(font_size, egui::FontFamily::Monospace),
            color,
        );
        let pos = rect.center() - galley.size() / 2.0;
        p.galley(pos, galley, color);
    }

    /// Speed readout in km/h with a small unit label below.
    fn draw_speed(ui: &mut Ui, speed: Option<f64>, h: f32, w: f32) {
        let (num, num_col) = match speed {
            Some(s) => (format!("{:.0}", s), Color32::WHITE),
            None => ("0".into(), Color32::from_gray(55)),
        };
        let scale = h / 56.0;
        let size = Vec2::new(w, h);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let p = ui.painter();

        // Number
        let num_font = (28.0 * scale).clamp(10.0, 72.0);
        let num_galley = p.layout_no_wrap(
            num,
            egui::FontId::new(num_font, egui::FontFamily::Monospace),
            num_col,
        );
        let num_pos = egui::pos2(
            rect.center().x - num_galley.size().x / 2.0,
            rect.center().y - num_galley.size().y / 2.0 - 4.0 * scale,
        );
        p.galley(num_pos, num_galley, num_col);

        // Unit label
        let unit_font = (10.0 * scale).clamp(6.0, 24.0);
        let unit_galley = p.layout_no_wrap(
            "km/h".into(),
            egui::FontId::proportional(unit_font),
            Color32::from_gray(110),
        );
        let unit_pos = egui::pos2(
            rect.center().x - unit_galley.size().x / 2.0,
            rect.bottom() - unit_galley.size().y - 2.0 * scale,
        );
        p.galley(unit_pos, unit_galley, Color32::from_gray(110));
    }

    /// Miniature steering wheel icon. Rotates ±90° based on the steering axis.
    fn draw_wheel(ui: &mut Ui, steer: f32, h: f32) {
        let size = Vec2::splat(h);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let center = rect.center();
        let scale = h / 56.0;
        let r = h / 2.0 - 5.0 * scale;
        let p = ui.painter();

        let rim_col = Color32::from_gray(180);
        let spoke_col = Color32::from_gray(140);
        let hub_col = Color32::from_gray(160);
        let rot = steer * PI * 0.5;

        // Outer rim
        p.circle_stroke(center, r, Stroke::new(3.5 * scale, Color32::from_gray(60)));
        p.circle_stroke(center, r, Stroke::new(2.0 * scale, rim_col));

        // Hub
        let hub_r = (4.0 * scale).max(2.0);
        p.circle_filled(center, hub_r, Color32::from_gray(50));
        p.circle_stroke(center, hub_r, Stroke::new(1.5 * scale, hub_col));

        // 3 spokes
        for i in 0..3 {
            let base = (i as f32) * 2.0 * PI / 3.0 - PI / 2.0;
            let a = base + rot;
            let from = center + egui::vec2(a.cos() * hub_r, a.sin() * hub_r);
            let to = center + egui::vec2(a.cos() * (r - 1.0), a.sin() * (r - 1.0));
            p.line_segment([from, to], Stroke::new(2.5 * scale, Color32::from_gray(55)));
            p.line_segment([from, to], Stroke::new(1.5 * scale, spoke_col));
        }
    }

    /// Bottom-right resize grip. Invisible by default; two diagonal lines
    /// appear after hovering for 500 ms. Dragging triggers native
    /// `BeginResize(SouthEast)` so the OS handles the resize smoothly.
    fn draw_resize_grip(&mut self, ui: &mut Ui) {
        let panel_rect = ui.max_rect();
        let grip_size = 14.0;
        let grip_rect = egui::Rect::from_min_size(
            egui::pos2(panel_rect.right() - grip_size, panel_rect.bottom() - grip_size),
            Vec2::splat(grip_size),
        );

        let resp = ui.interact(grip_rect, ui.id().with("resize_grip"), egui::Sense::click_and_drag());

        // Track hover duration
        if resp.hovered() {
            if self.resize_hover_start.is_none() {
                self.resize_hover_start = Some(Instant::now());
            }
        } else {
            self.resize_hover_start = None;
        }

        let active = self.resize_hover_start
            .map(|t| t.elapsed().as_millis() >= 500)
            .unwrap_or(false);

        if active {
            // Draw the grip triangle
            let p = ui.painter();
            let br = grip_rect.right_bottom();
            let alpha = 160u8;
            let col = Color32::from_rgba_unmultiplied(180, 180, 200, alpha);
            p.line_segment(
                [br - egui::vec2(grip_size, 0.0), br - egui::vec2(0.0, grip_size)],
                Stroke::new(1.5, col),
            );
            p.line_segment(
                [br - egui::vec2(grip_size * 0.55, 0.0), br - egui::vec2(0.0, grip_size * 0.55)],
                Stroke::new(1.5, col),
            );

            // Change cursor
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);

            // Begin resize on drag
            if resp.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(ResizeDirection::SouthEast));
            }
        }

        // Keep repainting while hovering so we can detect the 500ms threshold
        if resp.hovered() {
            ui.ctx().request_repaint();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// eframe::App implementation — main render loop
// ─────────────────────────────────────────────────────────────────────────────

impl App for OverlayApp {
    /// Return fully-transparent clear color so the GPU doesn't fill the
    /// rounded-corner gaps with an opaque background.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.read_inputs(ctx);
        self.update_history();
        self.poll_telemetry();

        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(Color32::from_rgba_unmultiplied(22, 26, 35, 230))
                    .rounding(Rounding::same(14.0))
                    .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                    .outer_margin(egui::Margin::same(4.0))
            )
            .show(ctx, |ui| {
                // Make borderless window draggable
                let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::drag());
                if drag.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let h = ui.available_height();
                    let scale = h / 56.0; // 56 = reference height
                    ui.spacing_mut().item_spacing.x = (12.0 * scale).max(4.0);
                    let gap = ui.spacing().item_spacing.x;

                    // Widget widths scale with height
                    let gear_w = (48.0 * scale).max(24.0);
                    let speed_w = (74.0 * scale).max(36.0);
                    let wheel_w = h;
                    let fixed = gear_w + speed_w + wheel_w + gap * 3.0 + 3.0;
                    let graph_w = (ui.available_width() - fixed).max(60.0 * scale);

                    self.draw_graph(ui, graph_w, h);
                    Self::draw_gear(ui, self.gear, self.rpm_pct, h, gear_w);
                    Self::draw_speed(ui, self.speed_kmh, h, speed_w);
                    Self::draw_wheel(ui, self.steering, h);
                });

                // Resize grip — bottom-right corner, visible after hovering 500ms
                self.draw_resize_grip(ui);
            });

        // Debug overlay (floating, press D to toggle)
        if self.debug_mode {
            egui::Area::new(egui::Id::new("debug"))
                .fixed_pos(egui::pos2(8.0, 8.0))
                .show(ctx, |ui| {
                    Frame::default()
                        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 210))
                        .rounding(Rounding::same(4.0))
                        .inner_margin(egui::Margin::same(6.0))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("[D] Debug — axis events")
                                    .size(10.0)
                                    .color(Color32::YELLOW),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "T:{:.0}% B:{:.0}% C:{:.0}% S:{:+.2}",
                                    self.throttle * 100.0,
                                    self.brake * 100.0,
                                    self.clutch * 100.0,
                                    self.steering,
                                ))
                                .size(10.0)
                                .color(Color32::WHITE)
                                .monospace(),
                            );
                            let telem_status = if self.last_telemetry.is_some() {
                                format!("Sim: {} (connected)", self.sim_name)
                            } else {
                                "Sim: waiting...".into()
                            };
                            ui.label(
                                RichText::new(telem_status)
                                    .size(10.0)
                                    .color(if self.last_telemetry.is_some() {
                                        Color32::from_rgb(80, 220, 80)
                                    } else {
                                        Color32::from_gray(120)
                                    })
                                    .monospace(),
                            );
                            // ABS / TC detection debug info
                            let abs_label = format!(
                                "ABS: {}  TC: {}  [src: {}]",
                                if self.abs_held { "ACTIVE" } else { "off" },
                                if self.tc_active { "ACTIVE" } else { "off" },
                                self.abs_source,
                            );
                            ui.label(
                                RichText::new(abs_label)
                                    .size(10.0)
                                    .color(if self.abs_held || self.tc_active {
                                        Color32::from_rgb(255, 160, 20)
                                    } else {
                                        Color32::from_gray(100)
                                    })
                                    .monospace(),
                            );
                            // Show raw pedal debug values
                            if let Some((gt, gb, gc)) = self.pedals_game_dbg {
                                ui.label(
                                    RichText::new(format!(
                                        "Game  T:{:.3} B:{:.3} C:{:.3}",
                                        gt, gb, gc
                                    ))
                                    .size(9.0)
                                    .color(Color32::from_gray(160))
                                    .monospace(),
                                );
                            } else {
                                ui.label(
                                    RichText::new("Game  pedals: N/A")
                                        .size(9.0)
                                        .color(Color32::from_rgb(200, 80, 80))
                                        .monospace(),
                                );
                            }
                            if let Some((rt, rb, rc)) = self.pedals_raw_dbg {
                                ui.label(
                                    RichText::new(format!(
                                        "Raw   T:{:.3} B:{:.3} C:{:.3}",
                                        rt, rb, rc
                                    ))
                                    .size(9.0)
                                    .color(Color32::from_gray(160))
                                    .monospace(),
                                );
                            } else {
                                ui.label(
                                    RichText::new("Raw   pedals: N/A")
                                        .size(9.0)
                                        .color(Color32::from_rgb(200, 80, 80))
                                        .monospace(),
                                );
                            }
                            if !self.debug_log.is_empty() {
                                ui.separator();
                                for line in &self.debug_log {
                                    ui.label(
                                        RichText::new(line)
                                            .size(9.0)
                                            .color(Color32::from_gray(190))
                                            .monospace(),
                                    );
                                }
                            }
                        });
                });
        }

        ctx.request_repaint();
    }
}
