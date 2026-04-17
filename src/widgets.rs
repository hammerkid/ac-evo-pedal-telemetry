//! HUD widgets: pedal graph, gear indicator, speed readout, steering wheel, resize grip.
//!
//! All draw functions are free functions that take data parameters — no dependency
//! on [`OverlayApp`]. Every size, font, and stroke is proportional to
//! `scale = height / 56.0` so the overlay looks consistent at any size.

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::time::Instant;

use eframe::egui::{self, Color32, Frame, Rounding, Stroke, Ui, Vec2};
use egui::viewport::ResizeDirection;
use egui_plot::{Line, Plot, PlotPoints};

use crate::Sample;

// ─────────────────────────────────────────────────────────────────────────────
// Pedal graph
// ─────────────────────────────────────────────────────────────────────────────

/// Scrolling pedal trace graph.
/// Green = throttle (yellow on TC), Red = brake (orange + glow on ABS), Blue = clutch.
pub fn draw_graph(
    ui: &mut Ui,
    history: &VecDeque<Sample>,
    history_seconds: f64,
    start_time: Instant,
    width: f32,
    height: f32,
) {
    let now = start_time.elapsed().as_secs_f64();
    let c_pts: PlotPoints = history.iter().map(|s| [s.t - now, s.clutch * 100.0]).collect();
    let scale = height / 56.0;
    let line_w = (2.0 * scale).clamp(1.0, 5.0);

    // Throttle segments split by TC state
    let throttle_normal_color = Color32::from_rgb(100, 220, 70);
    let throttle_tc_color = Color32::from_rgb(255, 200, 40);
    let mut throttle_segments: Vec<(Vec<[f64; 2]>, bool)> = Vec::new();
    for s in history {
        let pt = [s.t - now, s.throttle * 100.0];
        let tc_on = s.tc_active;
        match throttle_segments.last_mut() {
            Some((pts, prev)) if *prev == tc_on => pts.push(pt),
            Some((pts, _)) => {
                let bridge = *pts.last().unwrap();
                throttle_segments.push((vec![bridge, pt], tc_on));
            }
            None => throttle_segments.push((vec![pt], tc_on)),
        }
    }

    // Brake segments split by ABS state — color changes, same line width
    let brake_normal_color = Color32::from_rgb(240, 55, 50);
    let brake_abs_color = Color32::from_rgb(255, 160, 20);
    let abs_threshold = 0.05;
    let mut brake_segments: Vec<(Vec<[f64; 2]>, bool)> = Vec::new();
    for s in history {
        let pt = [s.t - now, s.brake * 100.0];
        let abs_on = s.abs_vibration > abs_threshold;
        match brake_segments.last_mut() {
            Some((pts, prev)) if *prev == abs_on => pts.push(pt),
            Some((pts, _)) => {
                let bridge = *pts.last().unwrap();
                brake_segments.push((vec![bridge, pt], abs_on));
            }
            None => brake_segments.push((vec![pt], abs_on)),
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
                .show_x(false)
                .show_y(false)
                .clamp_grid(true)
                .set_margin_fraction(Vec2::new(0.0, 0.02))
                .include_x(-history_seconds)
                .include_x(0.0)
                .include_y(0.0)
                .include_y(100.0)
                .height(height)
                .width(width)
                .show(ui, |plot_ui| {
                    // TC glow behind throttle (wide semi-transparent line)
                    for (pts, tc_on) in &throttle_segments {
                        if *tc_on {
                            plot_ui.line(
                                Line::new(PlotPoints::new(pts.clone()))
                                    .color(Color32::from_rgba_unmultiplied(255, 200, 40, 50))
                                    .width(line_w * 4.0),
                            );
                        }
                    }
                    // Throttle line
                    for (pts, tc_on) in &throttle_segments {
                        let color = if *tc_on { throttle_tc_color } else { throttle_normal_color };
                        plot_ui.line(Line::new(PlotPoints::new(pts.clone())).color(color).width(line_w));
                    }
                    // TC cut magnitude (thin purple)
                    let has_any_tc = history.iter().any(|s| s.tc_active);
                    if has_any_tc {
                        let tc_cut_pts: PlotPoints = history.iter()
                            .map(|s| [s.t - now, s.throttle_game * 100.0])
                            .collect();
                        plot_ui.line(
                            Line::new(tc_cut_pts)
                                .color(Color32::from_rgb(180, 100, 255))
                                .width(line_w * 0.6)
                        );
                    }
                    // Brake line — color changes for ABS segments
                    for (pts, abs_on) in &brake_segments {
                        let color = if *abs_on { brake_abs_color } else { brake_normal_color };
                        plot_ui.line(Line::new(PlotPoints::new(pts.clone())).color(color).width(line_w));
                    }
                    // Clutch line
                    plot_ui.line(Line::new(c_pts).color(Color32::from_rgb(60, 130, 255)).width(line_w * 0.75));
                });
        });
}

// ─────────────────────────────────────────────────────────────────────────────
// Gear indicator
// ─────────────────────────────────────────────────────────────────────────────

/// Gear display. Color shifts green → yellow → red based on RPM percentage.
pub fn draw_gear(ui: &mut Ui, gear: Option<i8>, rpm_pct: f32, h: f32, w: f32) {
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

// ─────────────────────────────────────────────────────────────────────────────
// Speed readout
// ─────────────────────────────────────────────────────────────────────────────

/// Speed in km/h with a small unit label below.
pub fn draw_speed(ui: &mut Ui, speed: Option<f64>, h: f32, w: f32) {
    let (num, num_col) = match speed {
        Some(s) => (format!("{:.0}", s), Color32::WHITE),
        None => ("0".into(), Color32::from_gray(55)),
    };
    let scale = h / 56.0;
    let size = Vec2::new(w, h);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();

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

// ─────────────────────────────────────────────────────────────────────────────
// Steering wheel
// ─────────────────────────────────────────────────────────────────────────────

/// Miniature steering wheel icon that rotates ±90° based on the steering axis.
pub fn draw_wheel(ui: &mut Ui, steer: f32, h: f32) {
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

    p.circle_stroke(center, r, Stroke::new(3.5 * scale, Color32::from_gray(60)));
    p.circle_stroke(center, r, Stroke::new(2.0 * scale, rim_col));

    let hub_r = (4.0 * scale).max(2.0);
    p.circle_filled(center, hub_r, Color32::from_gray(50));
    p.circle_stroke(center, hub_r, Stroke::new(1.5 * scale, hub_col));

    for i in 0..3 {
        let base = (i as f32) * 2.0 * PI / 3.0 - PI / 2.0;
        let a = base + rot;
        let from = center + egui::vec2(a.cos() * hub_r, a.sin() * hub_r);
        let to = center + egui::vec2(a.cos() * (r - 1.0), a.sin() * (r - 1.0));
        p.line_segment([from, to], Stroke::new(2.5 * scale, Color32::from_gray(55)));
        p.line_segment([from, to], Stroke::new(1.5 * scale, spoke_col));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resize grip
// ─────────────────────────────────────────────────────────────────────────────

/// Bottom-right resize grip. Invisible by default; two diagonal lines appear
/// after hovering for 500 ms. Dragging triggers native OS resize.
pub fn draw_resize_grip(ui: &mut Ui, resize_hover_start: &mut Option<Instant>) {
    let panel_rect = ui.max_rect();
    let grip_size = 14.0;
    let grip_rect = egui::Rect::from_min_size(
        egui::pos2(panel_rect.right() - grip_size, panel_rect.bottom() - grip_size),
        Vec2::splat(grip_size),
    );

    let resp = ui.interact(grip_rect, ui.id().with("resize_grip"), egui::Sense::click_and_drag());

    if resp.hovered() {
        if resize_hover_start.is_none() {
            *resize_hover_start = Some(Instant::now());
        }
    } else {
        *resize_hover_start = None;
    }

    let active = resize_hover_start
        .map(|t| t.elapsed().as_millis() >= 500)
        .unwrap_or(false);

    if active {
        let p = ui.painter();
        let br = grip_rect.right_bottom();
        let col = Color32::from_rgba_unmultiplied(180, 180, 200, 160);
        p.line_segment(
            [br - egui::vec2(grip_size, 0.0), br - egui::vec2(0.0, grip_size)],
            Stroke::new(1.5, col),
        );
        p.line_segment(
            [br - egui::vec2(grip_size * 0.55, 0.0), br - egui::vec2(0.0, grip_size * 0.55)],
            Stroke::new(1.5, col),
        );
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
        if resp.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(ResizeDirection::SouthEast));
        }
    }

    if resp.hovered() {
        ui.ctx().request_repaint();
    }
}
