//! # Racing Pedal Overlay
//!
//! A transparent, always-on-top HUD for sim racing that displays real-time
//! pedal inputs, gear, speed, and steering position. Reads hardware via
//! [`gilrs`] and sim telemetry from AC Evo shared memory (Win32 FFI).
//! Detects ABS/TC activation and colors the brake/throttle lines on the
//! scrolling graph.
//!
//! ## Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────┐
//! │  main thread (eframe / egui)                               │
//! │  ┌──────┐ ┌──────┐ ┌───────┐ ┌───────┐                     │
//! │  │Graph │ │ Gear │ │ Speed │ │ Wheel │  <- widgets.rs      │
//! │  └──────┘ └──────┘ └───────┘ └───────┘                     │
//! │       ▲ gilrs (input.rs)      ▲ mpsc::Receiver             │
//! └───────┼───────────────────────┼────────────────────────────┘
//!         │                       │
//!    USB / HID axis       ┌──────┴──────┐
//!    (pedals, wheel)      │ telemetry   │ <- telemetry.rs
//!                         │ thread      │    (std::thread + Win32 FFI)
//!                         └─────────────┘
//! ```
//!
//! ## Controls
//!
//! | Key              | Action                          |
//! |------------------|---------------------------------|
//! | `D`              | Toggle debug overlay            |
//! | `[` / `]`        | Shrink / grow graph time window |
//! | `Arrow Up/Down`  | Simulate throttle / brake       |
//! | `Space`          | Simulate clutch                 |
//! | `Arrow L/R`      | Simulate steering               |
//! | Drag window      | Move overlay                    |
//! | Hover BR corner  | Resize grip (activates ~500ms)  |

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(debug_assertions)]
mod debug;
mod input;
mod telemetry;
mod widgets;

use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::Instant;

use eframe::egui::{self, Align, Color32, Context, Frame, Layout, Rounding, Stroke};
use eframe::{App, NativeOptions};
use gilrs::Gilrs;

use telemetry::TelemetryData;

/// Default seconds of pedal history the graph displays (scrolling window).
const DEFAULT_HISTORY_SECONDS: f64 = 8.0;

/// Minimum / maximum allowed graph time window.
const MIN_HISTORY_SECONDS: f64 = 2.0;
const MAX_HISTORY_SECONDS: f64 = 30.0;

/// Target sample rate for recording pedal values into the history buffer.
const SAMPLE_RATE_HZ: f64 = 60.0;

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Query the primary monitor resolution via Win32 `GetSystemMetrics`.
#[cfg(windows)]
fn primary_monitor_size() -> (u32, u32) {
    extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN).max(800) as u32;
        let h = GetSystemMetrics(SM_CYSCREEN).max(600) as u32;
        (w, h)
    }
}

#[cfg(not(windows))]
fn primary_monitor_size() -> (u32, u32) {
    (1920, 1080)
}

fn main() -> eframe::Result<()> {
    let (mon_w, mon_h) = primary_monitor_size();

    let win_w = (mon_w as f32 * 0.26).clamp(400.0, 1000.0);
    let win_h = (win_w / 5.0).clamp(80.0, 200.0);

    let pos_x = (mon_w as f32 - win_w) / 2.0;
    let pos_y = mon_h as f32 - win_h - (mon_h as f32 * 0.04);

    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Racing Pedal Overlay")
            .with_inner_size([win_w, win_h])
            .with_position(egui::pos2(pos_x, pos_y))
            .with_decorations(false)
            .with_resizable(true)
            .with_transparent(true)
            .with_always_on_top(),
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
pub(crate) struct Sample {
    pub t: f64,
    pub throttle: f64,
    pub brake: f64,
    pub clutch: f64,
    pub abs_vibration: f32,
    pub tc_active: bool,
    pub throttle_game: f64,
}

/// Root application state.
pub(crate) struct OverlayApp {
    // Hardware input (gilrs)
    pub gilrs: Option<Gilrs>,
    pub throttle: f32,
    pub brake: f32,
    pub clutch: f32,
    pub steering: f32,
    pub steering_game: Option<f32>,

    // Scrolling graph history
    pub history: VecDeque<Sample>,
    pub start_time: Instant,
    pub last_sample_time: f64,

    // Sim telemetry
    pub telemetry_rx: mpsc::Receiver<TelemetryData>,
    pub gear: Option<i8>,
    pub speed_kmh: Option<f64>,
    pub rpm_pct: f32,
    pub abs_active: bool,
    pub abs_held: bool,
    pub abs_hold_until: Option<Instant>,
    pub tc_active: bool,
    pub abs_vibration: f32,
    pub throttle_game: f32,
    pub abs_source: &'static str,
    pub pedals_game_dbg: Option<(f64, f64, f64)>,
    pub pedals_raw_dbg: Option<(f64, f64, f64)>,
    pub sim_name: String,
    pub last_telemetry: Option<Instant>,

    // UI state
    pub debug_mode: bool,
    pub debug_log: VecDeque<String>,
    pub resize_hover_start: Option<Instant>,

    // Pedal axis inversion
    pub invert_throttle: bool,
    pub invert_brake: bool,
    pub invert_clutch: bool,

    // Widget visibility
    pub show_graph: bool,
    pub show_gear: bool,
    pub show_speed: bool,
    pub show_wheel: bool,

    pub probe_values: Vec<(usize, f32)>,
    pub history_seconds: f64,
}

impl OverlayApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::TRANSPARENT;
        visuals.window_fill = Color32::TRANSPARENT;
        visuals.window_stroke = Stroke::NONE;
        visuals.override_text_color = Some(Color32::from_rgb(220, 225, 232));
        cc.egui_ctx.set_visuals(visuals);

        let telemetry_rx = telemetry::spawn_telemetry_thread();

        Self {
            gilrs: Gilrs::new().ok(),
            throttle: 0.0,
            brake: 0.0,
            clutch: 0.0,
            steering: 0.0,
            steering_game: None,
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
            abs_vibration: 0.0,
            throttle_game: 0.0,
            abs_source: "none",
            pedals_game_dbg: None,
            pedals_raw_dbg: None,
            sim_name: String::new(),
            last_telemetry: None,
            debug_mode: false,
            debug_log: VecDeque::new(),
            resize_hover_start: None,
            invert_throttle: false,
            invert_brake: true,
            invert_clutch: false,
            show_graph: true,
            show_gear: true,
            show_speed: true,
            show_wheel: true,
            probe_values: Vec::new(),
            history_seconds: DEFAULT_HISTORY_SECONDS,
        }
    }

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
            abs_vibration: self.abs_vibration,
            tc_active: self.tc_active,
            throttle_game: if self.last_telemetry.is_some() {
                self.throttle_game as f64
            } else {
                self.throttle as f64
            },
        });
        while let Some(front) = self.history.front() {
            if now - front.t > self.history_seconds {
                self.history.pop_front();
            } else {
                break;
            }
        }
    }

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
            if data.abs_active {
                self.abs_hold_until = Some(Instant::now() + std::time::Duration::from_millis(300));
                self.abs_held = true;
            } else if self.abs_hold_until.map(|t| Instant::now() >= t).unwrap_or(true) {
                self.abs_held = false;
            }
            self.tc_active = data.tc_active;
            self.abs_vibration = data.abs_vibration;
            self.throttle_game = data.pedals_game.map(|p| p.0 as f32).unwrap_or(self.throttle);
            self.steering_game = data.steer_angle;
            self.abs_source = data.abs_source;
            self.pedals_game_dbg = data.pedals_game;
            self.pedals_raw_dbg = data.pedals_raw;
            self.sim_name = data.sim_name;
            self.last_telemetry = Some(Instant::now());
            if !data.probe_values.is_empty() {
                self.probe_values = data.probe_values;
            }
        }
        if let Some(last) = self.last_telemetry {
            if last.elapsed().as_secs_f32() > 2.0 {
                self.gear = None;
                self.speed_kmh = None;
                self.rpm_pct = 0.0;
                self.abs_active = false;
                self.abs_held = false;
                self.abs_hold_until = None;
                self.tc_active = false;
                self.abs_vibration = 0.0;
                self.throttle_game = 0.0;
                self.steering_game = None;
                self.abs_source = "none";
                self.sim_name.clear();
                self.last_telemetry = None;
                self.probe_values.clear();
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// eframe::App — main render loop
// ─────────────────────────────────────────────────────────────────────────────

impl App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        input::read_inputs(self, ctx);
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
                let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::drag());
                if drag.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let h = ui.available_height();
                    let scale = h / 56.0;
                    ui.spacing_mut().item_spacing.x = (12.0 * scale).max(4.0);
                    let gap = ui.spacing().item_spacing.x;

                    let gear_w = (48.0 * scale).max(24.0);
                    let speed_w = (74.0 * scale).max(36.0);
                    let wheel_w = h;

                    let mut fixed = 0.0_f32;
                    let mut extra_gaps = 0u32;
                    if self.show_gear  { fixed += gear_w;  extra_gaps += 1; }
                    if self.show_speed { fixed += speed_w; extra_gaps += 1; }
                    if self.show_wheel { fixed += wheel_w; extra_gaps += 1; }
                    fixed += gap * extra_gaps as f32 + 3.0;
                    let graph_w = (ui.available_width() - fixed).max(60.0 * scale);

                    if self.show_graph {
                        widgets::draw_graph(ui, &self.history, self.history_seconds, self.start_time, graph_w, h);
                    }
                    if self.show_gear {
                        widgets::draw_gear(ui, self.gear, self.rpm_pct, h, gear_w);
                    }
                    if self.show_speed {
                        widgets::draw_speed(ui, self.speed_kmh, h, speed_w);
                    }
                    if self.show_wheel {
                        let effective_steer = self.steering_game.unwrap_or(self.steering);
                        widgets::draw_wheel(ui, effective_steer, h);
                    }
                });

                widgets::draw_resize_grip(ui, &mut self.resize_hover_start);
            });

        #[cfg(debug_assertions)]
        if self.debug_mode {
            debug::draw_debug_overlay(self, ctx);
        }

        ctx.request_repaint();
    }
}
