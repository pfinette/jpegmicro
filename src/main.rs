use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Align, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Layout, Rect, Role, Sense, Stroke,
    StrokeKind, Theme, WidgetInfo, pos2, vec2,
};
use jpegmicro::{Outcome, Settings, Strictness, collect_jpegs, format_size, process_file, worker_pool};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JPEGmicro")
            .with_inner_size([780.0, 540.0])
            .with_min_inner_size([680.0, 380.0])
            .with_drag_and_drop(true)
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        ..Default::default()
    };
    eframe::run_native("JPEGmicro", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// The one signal color: bytes saved.
const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x6A, 0x1F);

/// A faceplate with a recessed screen, after hardware audio gear.
struct Palette {
    face: Color32,
    groove: Color32,
    ink: Color32,
    dim: Color32,
    screen: Color32,
    screen_edge: Color32,
    screen_text: Color32,
    screen_dim: Color32,
    seg_off: Color32,
    seg_kept: Color32,
    error: Color32,
}

const LIGHT: Palette = Palette {
    face: Color32::from_rgb(0xE7, 0xE5, 0xDF),
    groove: Color32::from_rgb(0xC9, 0xC5, 0xBC),
    ink: Color32::from_rgb(0x1B, 0x1B, 0x1A),
    dim: Color32::from_rgb(0x6F, 0x6C, 0x66),
    screen: Color32::from_rgb(0x1A, 0x1B, 0x1D),
    screen_edge: Color32::from_rgb(0x9C, 0x98, 0x90),
    screen_text: Color32::from_rgb(0xE6, 0xE4, 0xDE),
    screen_dim: Color32::from_rgb(0x82, 0x84, 0x88),
    seg_off: Color32::from_rgb(0x2C, 0x2E, 0x32),
    seg_kept: Color32::from_rgb(0x9A, 0x9D, 0xA3),
    error: Color32::from_rgb(0xFF, 0x6B, 0x5B),
};

const DARK: Palette = Palette {
    face: Color32::from_rgb(0x2B, 0x2B, 0x2A),
    groove: Color32::from_rgb(0x44, 0x44, 0x42),
    ink: Color32::from_rgb(0xEC, 0xEA, 0xE4),
    dim: Color32::from_rgb(0x9A, 0x97, 0x8F),
    screen: Color32::from_rgb(0x17, 0x18, 0x1A),
    screen_edge: Color32::from_rgb(0x0E, 0x0E, 0x0F),
    ..LIGHT
};

fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// DIN for hardware labels, SF Mono for screens, SF Pro for menus and tooltips.
fn din(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("din".into()))
}

fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// Text with extra letter spacing, as printed on a faceplate.
fn tracked(text: impl Into<String>, font: FontId, spacing: f32, color: Color32) -> LayoutJob {
    LayoutJob::single_section(text.into(), TextFormat { font_id: font, extra_letter_spacing: spacing, color, ..Default::default() })
}

fn install_style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Each family falls back to egui's own fonts when a system font is missing.
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    fonts.families.insert(FontFamily::Name("din".into()), fallback);
    for (name, path, family) in [
        ("SF Pro", "/System/Library/Fonts/SFNS.ttf", FontFamily::Proportional),
        ("SF Mono", "/System/Library/Fonts/SFNSMono.ttf", FontFamily::Monospace),
        ("DIN", "/System/Library/Fonts/Supplemental/DIN Alternate Bold.ttf", FontFamily::Name("din".into())),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            fonts.families.entry(family).or_default().insert(0, name.into());
        }
    }
    ctx.set_fonts(fonts);

    for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            style.spacing.scroll = egui::style::ScrollStyle::floating();
            style.interaction.tooltip_delay = 0.6;
            style.interaction.selectable_labels = false;
            let v = &mut style.visuals;
            v.panel_fill = p.face;
            v.window_fill = p.face;
            v.selection.bg_fill = ACCENT;
            v.selection.stroke.color = p.ink;
            v.widgets.noninteractive.bg_stroke.color = p.groove;
            v.widgets.noninteractive.fg_stroke.color = p.ink;
            v.widgets.inactive.fg_stroke.color = p.ink;
        });
    }
}

#[derive(Serialize, Deserialize)]
struct Prefs {
    strictness: Strictness,
    min_saving_percent: f64,
}

const DEFAULT_MIN_SAVING: f64 = 5.0;

impl Default for Prefs {
    fn default() -> Self {
        Self { strictness: Strictness::Aggressive, min_saving_percent: DEFAULT_MIN_SAVING }
    }
}

enum Status {
    Queued,
    Working,
    Done(Outcome),
    Failed(String),
}

struct Row {
    path: PathBuf,

    /// Original size, read when the file is queued.
    size: u64,
    status: Status,
}

struct App {
    prefs: Prefs,
    rows: Vec<Row>,
    tx: Sender<(usize, Status)>,
    rx: Receiver<(usize, Status)>,
    pool: rayon::ThreadPool,

    /// Paths given on the command line, processed on the first frame.
    launch_paths: Vec<PathBuf>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_style(&cc.egui_ctx);
        let prefs = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let (tx, rx) = channel();
        let launch_paths = std::env::args_os().skip(1).map(PathBuf::from).collect();
        Self { prefs, rows: Vec::new(), tx, rx, pool: worker_pool(), launch_paths }
    }

    fn busy(&self) -> bool {
        self.rows.iter().any(|r| matches!(r.status, Status::Queued | Status::Working))
    }

    fn enqueue(&mut self, dropped: Vec<PathBuf>, ctx: &egui::Context) {
        // Skip files already in the list that are queued or running.
        let mut files = collect_jpegs(&dropped);
        files.retain(|f| !self.rows.iter().any(|r| &r.path == f && matches!(r.status, Status::Queued | Status::Working)));
        if files.is_empty() {
            return;
        }

        let start = self.rows.len();
        self.rows.extend(files.iter().map(|p| Row {
            path: p.clone(),
            size: std::fs::metadata(p).map_or(0, |m| m.len()),
            status: Status::Queued,
        }));

        let settings = Settings::new(self.prefs.strictness, self.prefs.min_saving_percent);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.pool.spawn(move || {
            files.par_iter().enumerate().for_each_with(tx, |tx, (i, path)| {
                let _ = tx.send((start + i, Status::Working));
                ctx.request_repaint();
                let status = match process_file(path, &settings) {
                    Ok(outcome) => Status::Done(outcome),
                    Err(e) => Status::Failed(e),
                };
                let _ = tx.send((start + i, status));
                ctx.request_repaint();
            });
        });
    }

    /// Wordmark and status display, in the title bar beside the traffic lights.
    fn title_strip(&self, ui: &mut egui::Ui) {
        let p = palette(ui);

        // The strip is the title bar: drag to move, double-click to zoom.
        let bar = Rect::from_min_max(pos2(0.0, 0.0), pos2(ui.ctx().content_rect().right(), ui.max_rect().bottom()));
        let background = ui.interact(bar, ui.scope_id().with("titlebar"), Sense::click_and_drag());
        if background.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if background.double_clicked() {
            let zoomed = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!zoomed));
        }

        let done = self.rows.iter().filter(|r| matches!(r.status, Status::Done(_) | Status::Failed(_))).count();
        let status = match (self.rows.len(), self.busy()) {
            (0, _) => "READY".to_owned(),
            (n, true) => format!("WORKING {done}/{n}"),
            (n, false) => format!("DONE {n}/{n}"),
        };

        ui.horizontal_centered(|ui| {
            ui.label(tracked("JPEGMICRO", din(15.0), 3.0, p.ink));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                lcd_text(ui, &status, p);
                if ui.available_width() > 220.0 {
                    ui.add_space(10.0);
                    ui.label(tracked("ORIGINALS GO TO THE TRASH", din(10.0), 1.2, p.dim));
                }
            });
        });
    }

    /// Hardware controls along the bottom of the faceplate.
    fn control_row(&mut self, ui: &mut egui::Ui) {
        let (mut before, mut saved) = (0u64, 0u64);
        for r in &self.rows {
            if let Status::Done(o) = &r.status {
                before += o.size_before();
                saved += o.bytes_saved();
            }
        }
        let pct = if before > 0 { saved as f64 * 100.0 / before as f64 } else { 0.0 };
        let can_clear = !self.rows.is_empty() && !self.busy();
        let mut clear = false;

        ui.horizontal(|ui| {
            control(ui, 3.0 * SWITCH_SEGMENT, "STRICTNESS", |ui| {
                ui.add_space(15.0);
                strictness_switch(ui, &mut self.prefs.strictness);
            });
            ui.add_space(28.0);
            control(ui, 64.0, "MIN SAVING", |ui| {
                knob(ui, &mut self.prefs.min_saving_percent, 0.0..=30.0, DEFAULT_MIN_SAVING);
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                control(ui, 64.0, "", |ui| {
                    ui.add_space(15.0);
                    clear = hardware_button(ui, "CLEAR", can_clear);
                });
                ui.add_space(20.0);
                control(ui, 176.0, "SAVED", |ui| {
                    ui.add_space(7.0);
                    saved_display(ui, saved, pct);
                });
            });
        });
        if clear {
            self.rows.clear();
        }
    }

    /// The recessed screen: one track per file, or what to do when there are none.
    fn screen(&self, ui: &mut egui::Ui, hovering: bool) {
        let p = palette(ui);
        let edge = if hovering { ACCENT } else { p.screen_edge };
        egui::Frame::new()
            .fill(p.screen)
            .corner_radius(6)
            .stroke(Stroke::new(if hovering { 2.0 } else { 1.0 }, edge))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                let r = ui.max_rect();
                if self.rows.is_empty() {
                    let title = if hovering { "RELEASE TO SHRINK" } else { "DROP PHOTOS OR FOLDERS" };
                    let color = if hovering { ACCENT } else { p.screen_text };
                    let g = ui.painter().layout_job(tracked(title, mono(15.0), 2.0, color));
                    ui.painter().galley(r.center() - vec2(g.size().x / 2.0, 22.0), g, color);
                    ui.painter().text(
                        r.center() + vec2(0.0, 14.0),
                        Align2::CENTER_CENTER,
                        "Each JPEG gets as small as it can without looking different.",
                        mono(11.0),
                        p.screen_dim,
                    );
                    return;
                }

                let largest = self.rows.iter().map(|r| r.size).max().unwrap_or(1).max(1);
                egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show_rows(
                    ui,
                    ROW_HEIGHT,
                    self.rows.len(),
                    |ui, range| {
                        for row in &self.rows[range] {
                            track(ui, row, largest);
                        }
                    },
                );
                if hovering {
                    let veil = Color32::from_rgba_unmultiplied(p.screen.r(), p.screen.g(), p.screen.b(), 220);
                    ui.painter().rect_filled(r, 4.0, veil);
                    let g = ui.painter().layout_job(tracked("RELEASE TO ADD", mono(15.0), 2.0, ACCENT));
                    ui.painter().galley(r.center() - g.size() / 2.0, g, ACCENT);
                }
            });
    }
}

const CONTROL_HEIGHT: f32 = 76.0;
const LABEL_HEIGHT: f32 = 14.0;
const SWITCH_SEGMENT: f32 = 86.0;

/// A control with its label printed under it; all labels share one baseline.
fn control(ui: &mut egui::Ui, width: f32, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = palette(ui);
    ui.allocate_ui_with_layout(vec2(width, CONTROL_HEIGHT), Layout::top_down(Align::Center), |ui| {
        ui.set_min_size(vec2(width, CONTROL_HEIGHT));
        add(ui);
        if !label.is_empty() {
            let r = ui.max_rect();
            let g = ui.painter().layout_job(tracked(label, din(10.0), 1.4, p.dim));
            ui.painter().galley(pos2(r.center().x - g.size().x / 2.0, r.bottom() - LABEL_HEIGHT), g, p.dim);
        }
    });
}

/// A three-position switch; the selected position lights up.
fn strictness_switch(ui: &mut egui::Ui, value: &mut Strictness) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(3.0 * SWITCH_SEGMENT, 24.0), Sense::hover());
    for (i, s) in Strictness::ALL.into_iter().enumerate() {
        let r = Rect::from_min_size(rect.min + vec2(SWITCH_SEGMENT * i as f32, 0.0), vec2(SWITCH_SEGMENT, 24.0));
        let response = ui.interact(r, ui.scope_id().with(("strictness", i)), Sense::click());
        if response.clicked() {
            *value = s;
        }
        let on = *value == s;
        response.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, on, s.label()));

        let painter = ui.painter();
        if on {
            painter.rect_filled(r.shrink(2.0), 2.0, ACCENT);
        } else if response.hovered() {
            painter.rect_filled(r.shrink(2.0), 2.0, p.groove);
        }
        if i > 0 {
            painter.vline(r.left(), r.y_range(), Stroke::new(1.0, p.ink));
        }
        let text_color = if on { LIGHT.ink } else { p.ink };
        let g = painter.layout_job(tracked(s.label().to_uppercase(), din(11.0), 1.0, text_color));
        painter.galley(r.center() - g.size() / 2.0, g, text_color);
        if response.has_focus() {
            painter.rect_stroke(r.shrink(1.0), 3.0, Stroke::new(2.0, ACCENT), StrokeKind::Inside);
        }
    }
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, p.ink), StrokeKind::Inside);
}

/// An arc knob: drag up or down, arrow keys when focused, double-click to reset.
fn knob(ui: &mut egui::Ui, value: &mut f64, range: RangeInclusive<f64>, default: f64) {
    let p = palette(ui);
    let size = 42.0;
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click_and_drag());
    if response.dragged() {
        *value -= response.drag_delta().y as f64 * 0.15;
    }
    if response.drag_stopped() {
        *value = value.round();
    }
    if response.double_clicked() {
        *value = default;
    }
    if response.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight) {
                *value += 1.0;
            }
            if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft) {
                *value -= 1.0;
            }
        });
    }
    *value = value.clamp(*range.start(), *range.end());
    let t = ((*value - range.start()) / (range.end() - range.start())) as f32;
    let shown = format!("{:.0}%", value.round());
    let response = response
        .on_hover_text(format!("Replace a file only if it gets at least {shown} smaller. Drag up or down; double-click resets."));
    response.widget_info(|| WidgetInfo::slider(true, value.round(), "Minimum saving"));

    // 270° sweep with the gap at the bottom, as on a hardware pot.
    let c = rect.center();
    let radius = size / 2.0 - 2.0;
    let start = 0.75 * std::f32::consts::PI;
    let sweep = 1.5 * std::f32::consts::PI;
    let arc = |from: f32, to: f32| -> Vec<egui::Pos2> {
        (0..=32).map(|k| from + (to - from) * k as f32 / 32.0).map(|a| c + radius * vec2(a.cos(), a.sin())).collect()
    };
    let painter = ui.painter();
    painter.add(egui::Shape::line(arc(start, start + sweep), Stroke::new(3.0, p.groove)));
    if t > 0.0 {
        painter.add(egui::Shape::line(arc(start, start + sweep * t), Stroke::new(3.0, ACCENT)));
    }
    let a = start + sweep * t;
    let dir = vec2(a.cos(), a.sin());
    let body = radius - 6.0;
    painter.circle_filled(c, body, p.ink);
    painter.line_segment([c + 0.3 * body * dir, c + (body - 2.5) * dir], Stroke::new(2.5, p.face));
    if response.has_focus() {
        painter.circle_stroke(c, radius + 3.0, Stroke::new(1.5, ACCENT));
    }
    ui.add_space(2.0);
    ui.label(egui::RichText::new(shown).font(mono(12.0)).color(p.ink));
}

/// A square hardware button with its legend printed on it.
fn hardware_button(ui: &mut egui::Ui, legend: &str, enabled: bool) -> bool {
    let p = palette(ui);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(64.0, 24.0), sense);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, legend));
    let color = if enabled { p.ink } else { p.groove };
    let painter = ui.painter();
    if response.is_pointer_button_down_on() {
        painter.rect_filled(rect, 3.0, p.groove);
    }
    painter.rect_stroke(rect, 3.0, Stroke::new(1.0, color), StrokeKind::Inside);
    let g = painter.layout_job(tracked(legend, din(11.0), 1.2, color));
    painter.galley(rect.center() - g.size() / 2.0, g, color);
    if response.has_focus() {
        painter.rect_stroke(rect.expand(2.0), 4.0, Stroke::new(1.5, ACCENT), StrokeKind::Outside);
    }
    enabled && response.clicked()
}

/// A small screen in the title strip.
fn lcd_text(ui: &mut egui::Ui, text: &str, p: &Palette) {
    let g = ui.painter().layout_job(tracked(text, mono(11.0), 1.0, p.screen_text));
    let (rect, _) = ui.allocate_exact_size(g.size() + vec2(18.0, 8.0), Sense::hover());
    ui.painter().rect(rect, 3.0, p.screen, Stroke::new(1.0, p.screen_edge), StrokeKind::Inside);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, p.screen_text);
}

/// The total saved, in orange digits.
fn saved_display(ui: &mut egui::Ui, saved: u64, pct: f64) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(176.0, 40.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, 4.0, p.screen, Stroke::new(1.0, p.screen_edge), StrokeKind::Inside);
    let (amount, color) = if saved > 0 { (format_size(saved), ACCENT) } else { ("0 B".to_owned(), p.screen_dim) };
    painter.text(rect.left_center() + vec2(12.0, 0.0), Align2::LEFT_CENTER, amount, mono(20.0), color);
    if saved > 0 {
        painter.text(rect.right_center() - vec2(12.0, 0.0), Align2::RIGHT_CENTER, format!("{pct:.0}%"), mono(12.0), p.screen_dim);
    }
}

const ROW_HEIGHT: f32 = 26.0;
const NAME_WIDTH: f32 = 190.0;
const SIZES_WIDTH: f32 = 140.0;
const PCT_WIDTH: f32 = 50.0;
const GAP: f32 = 16.0;

/// LED segments: lit grey for kept bytes, orange for removed bytes.
const SEGMENT: f32 = 3.0;
const SEGMENT_GAP: f32 = 2.0;
const SEGMENT_HEIGHT: f32 = 10.0;

/// One file as a track: name, an LED meter as long as the original, then sizes or status.
fn track(ui: &mut egui::Ui, row: &Row, largest: u64) {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let painter = ui.painter_at(rect);
    let font = mono(12.0);
    let cy = rect.center().y;

    let name = row.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut job = LayoutJob::single_section(name, TextFormat::simple(font.clone(), p.screen_text));
    job.wrap = TextWrapping { max_width: NAME_WIDTH - 12.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = painter.layout_job(job);
    painter.galley(pos2(rect.left(), cy - galley.size().y / 2.0), galley, p.screen_text);

    let info_right = rect.right() - 4.0;
    let meter_left = rect.left() + NAME_WIDTH;
    let meter_space = (info_right - SIZES_WIDTH - PCT_WIDTH - GAP - meter_left).max(40.0);
    let step = SEGMENT + SEGMENT_GAP;
    let segments = ((meter_space * row.size as f32 / largest as f32) / step).round().max(2.0) as usize;
    let lit = |n_kept: usize, kept: Color32, rest: Color32| {
        for k in 0..segments {
            let x = meter_left + k as f32 * step;
            let r = Rect::from_min_size(pos2(x, (cy - SEGMENT_HEIGHT / 2.0).round()), vec2(SEGMENT, SEGMENT_HEIGHT));
            painter.rect_filled(r, 0.0, if k < n_kept { kept } else { rest });
        }
    };

    let right_text = |text: String, x: f32, color: Color32| {
        painter.text(pos2(x, cy), Align2::RIGHT_CENTER, text, font.clone(), color);
    };
    let status_text = |text: &str, color: Color32| right_text(text.into(), info_right, color);

    let detail = match &row.status {
        Status::Done(Outcome::Compressed { before, after, quality, dssim }) => {
            let kept = (segments as f64 * *after as f64 / *before as f64).round() as usize;
            lit(kept, p.seg_kept, ACCENT);
            right_text(before_after(*before, *after), info_right - PCT_WIDTH, p.screen_text);
            let pct = (before - after) as f64 * 100.0 / *before as f64;
            right_text(format!("−{pct:.0}%"), info_right, ACCENT);
            Some(format!("Quality {quality}, DSSIM {dssim:.5}"))
        }
        Status::Working => {
            lit(segments, p.seg_kept.gamma_multiply(0.55), p.seg_off);
            status_text("WORKING", p.screen_text);
            None
        }
        status => {
            lit(0, p.seg_off, p.seg_off);
            match status {
                Status::Queued => {
                    status_text("QUEUED", p.screen_dim);
                    None
                }
                Status::Failed(e) => {
                    status_text("COULDN'T SHRINK", p.error);
                    Some(e.clone())
                }
                Status::Done(Outcome::AlreadyProcessed { .. }) => {
                    status_text("ALREADY SHRUNK", p.screen_dim);
                    None
                }
                Status::Done(Outcome::NoGain { best, .. }) => {
                    status_text("KEPT ORIGINAL", p.screen_dim);
                    Some(match best {
                        Some(b) => format!("Best result was {}, below the minimum saving", format_size(*b)),
                        None => "No quality setting stayed visually identical".into(),
                    })
                }
                Status::Done(Outcome::Skipped { reason, .. }) => {
                    status_text(&format!("SKIPPED: {}", reason.to_uppercase()), p.screen_dim);
                    None
                }
                Status::Working | Status::Done(Outcome::Compressed { .. }) => unreachable!(),
            }
        }
    };

    let path = row.path.display().to_string();
    let tip = match detail {
        Some(d) => format!("{path}\n{d}"),
        None => path,
    };
    response.on_hover_text(tip).context_menu(|ui| {
        if ui.button("Show in Finder").clicked() {
            let _ = std::process::Command::new("open").arg("-R").arg(&row.path).spawn();
            ui.close();
        }
    });
}

/// "4.5 → 2.1 MB", or "1.2 MB → 640 KB" when the units differ.
fn before_after(before: u64, after: u64) -> String {
    let (b, a) = (format_size(before), format_size(after));
    match (b.rsplit_once(' '), a.rsplit_once(' ')) {
        (Some((b_num, b_unit)), Some((_, a_unit))) if b_unit == a_unit => format!("{b_num} → {a}"),
        _ => format!("{b} → {a}"),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = palette(ui);

        while let Ok((i, status)) = self.rx.try_recv() {
            if let Some(row) = self.rows.get_mut(i) {
                row.status = status;
            }
        }

        if !self.launch_paths.is_empty() {
            let paths = std::mem::take(&mut self.launch_paths);
            self.enqueue(paths, &ctx);
        }

        let (hovering, dropped) = ctx.input(|i| {
            let dropped: Vec<PathBuf> = i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect();
            (!i.raw.hovered_files.is_empty(), dropped)
        });
        if !dropped.is_empty() {
            self.enqueue(dropped, &ctx);
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Left margin clears the traffic lights; the 32 pt strip centers on them.
        let strip = egui::Frame::new().fill(p.face).inner_margin(egui::Margin { left: 80, right: 14, top: 4, bottom: 4 });
        egui::Panel::top("title").frame(strip).exact_size(32.0).show(ui, |ui| self.title_strip(ui));

        let controls = egui::Frame::new().fill(p.face).inner_margin(egui::Margin { left: 16, right: 16, top: 12, bottom: 10 });
        egui::Panel::bottom("controls").frame(controls).show(ui, |ui| self.control_row(ui));

        let body = egui::Frame::new().fill(p.face).inner_margin(egui::Margin { left: 14, right: 14, top: 10, bottom: 4 });
        egui::CentralPanel::default().frame(body).show(ui, |ui| self.screen(ui, hovering));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.prefs);
    }
}
