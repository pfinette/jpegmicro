use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Align, Align2, Color32, FontData, FontFamily, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind,
    TextStyle, Theme, pos2, vec2,
};
use eframe::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use jpegmicro::{Outcome, Settings, Strictness, collect_jpegs, format_size, process_file, worker_pool};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JPEGmicro")
            .with_inner_size([760.0, 540.0])
            .with_min_inner_size([560.0, 360.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("JPEGmicro", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// Colors for one theme. Kept bytes are graphite; removed bytes are cyanotype blue.
struct Palette {
    bg: Color32,
    chrome: Color32,
    text: Color32,
    muted: Color32,
    line: Color32,
    track: Color32,
    control: Color32,
    kept: Color32,
    saved: Color32,
    error: Color32,
}

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1C, 0x20, 0x26),
    chrome: Color32::from_rgb(0x22, 0x27, 0x2E),
    text: Color32::from_rgb(0xE4, 0xE7, 0xEB),
    muted: Color32::from_rgb(0x8B, 0x94, 0x9E),
    line: Color32::from_rgb(0x30, 0x36, 0x3E),
    track: Color32::from_rgb(0x2B, 0x31, 0x39),
    control: Color32::from_rgb(0x3A, 0x41, 0x4A),
    kept: Color32::from_rgb(0x5E, 0x68, 0x73),
    saved: Color32::from_rgb(0x5B, 0x9B, 0xE6),
    error: Color32::from_rgb(0xE5, 0x73, 0x5F),
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF3, 0xF5, 0xF7),
    chrome: Color32::from_rgb(0xE8, 0xEC, 0xF0),
    text: Color32::from_rgb(0x1D, 0x22, 0x28),
    muted: Color32::from_rgb(0x64, 0x6D, 0x77),
    line: Color32::from_rgb(0xD3, 0xD9, 0xDF),
    track: Color32::from_rgb(0xDF, 0xE4, 0xE9),
    control: Color32::from_rgb(0xC6, 0xCD, 0xD5),
    kept: Color32::from_rgb(0x98, 0xA3, 0xAF),
    saved: Color32::from_rgb(0x1F, 0x5F, 0xB0),
    error: Color32::from_rgb(0xC2, 0x41, 0x2D),
};

fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// SF Pro from the system, sizes, and both palettes.
fn install_style(ctx: &egui::Context) {
    if let Ok(sf) = std::fs::read("/System/Library/Fonts/SFNS.ttf") {
        ctx.add_font(FontInsert {
            name: "SF Pro".into(),
            data: FontData::from_owned(sf),
            families: vec![InsertFontFamily { family: FontFamily::Proportional, priority: FontPriority::Highest }],
        });
    }
    for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            for (text_style, size) in [
                (TextStyle::Heading, 22.0),
                (TextStyle::Body, 13.5),
                (TextStyle::Button, 13.5),
                (TextStyle::Small, 11.5),
            ] {
                style.text_styles.insert(text_style, FontId::proportional(size));
            }
            let v = &mut style.visuals;
            v.panel_fill = p.bg;
            v.window_fill = p.chrome;
            v.extreme_bg_color = p.track;
            v.selection.bg_fill = p.saved;
            v.selection.stroke.color = p.saved;
            v.widgets.noninteractive.bg_stroke.color = p.line;
            v.widgets.noninteractive.fg_stroke.color = p.text;
            v.widgets.inactive.fg_stroke.color = p.text;
            v.widgets.inactive.bg_fill = p.control;
        });
    }
}

#[derive(Serialize, Deserialize)]
struct Prefs {
    strictness: Strictness,
    min_saving_percent: f64,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { strictness: Strictness::Aggressive, min_saving_percent: 5.0 }
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

    fn settings_bar(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        ui.horizontal(|ui| {
            ui.label("Strictness");
            egui::ComboBox::from_id_salt("strictness")
                .selected_text(self.prefs.strictness.label())
                .show_ui(ui, |ui| {
                    for s in Strictness::ALL {
                        ui.selectable_value(&mut self.prefs.strictness, s, s.label());
                    }
                });
            ui.add_space(18.0);
            ui.label("Replace if at least");
            ui.add(egui::Slider::new(&mut self.prefs.min_saving_percent, 0.0..=30.0).suffix("%").integer());
            ui.label("smaller");
            if ui.available_width() > 170.0 {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("Originals go to the Trash").color(p.muted));
                });
            }
        });
    }

    /// The whole window is the drop target; with no files it says so.
    fn empty_state(&self, ui: &mut egui::Ui, hovering: bool) {
        let p = palette(ui);
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let painter = ui.painter();
        if hovering {
            painter.rect(rect.shrink(6.0), 12.0, p.saved.gamma_multiply(0.12), Stroke::new(2.0, p.saved), StrokeKind::Inside);
        }
        let c = rect.center();
        let title = if hovering { "Release to shrink" } else { "Drop photos or folders" };
        painter.text(c - vec2(0.0, 14.0), Align2::CENTER_CENTER, title, FontId::proportional(24.0), p.text);
        painter.text(
            c + vec2(0.0, 18.0),
            Align2::CENTER_CENTER,
            "Each JPEG gets as small as it can without looking different.",
            FontId::proportional(13.5),
            p.muted,
        );
    }

    fn file_list(&self, ui: &mut egui::Ui) {
        let largest = self.rows.iter().map(|r| r.size).max().unwrap_or(1).max(1);
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show_rows(
            ui,
            ROW_HEIGHT,
            self.rows.len(),
            |ui, range| {
                for row in &self.rows[range] {
                    file_row(ui, row, largest);
                }
            },
        );
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let done = self.rows.iter().filter(|r| matches!(r.status, Status::Done(_) | Status::Failed(_))).count();
        let (mut before, mut saved) = (0u64, 0u64);
        for r in &self.rows {
            if let Status::Done(o) = &r.status {
                before += o.size_before();
                saved += o.bytes_saved();
            }
        }
        let pct = if before > 0 { saved as f64 * 100.0 / before as f64 } else { 0.0 };

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{done} of {} done", self.rows.len())).color(p.muted));
            ui.add_space(12.0);
            if saved > 0 {
                ui.label(format!("Saved {} of {} ({pct:.0}%)", format_size(saved), format_size(before)));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("Clear list")).clicked() {
                    self.rows.clear();
                }
            });
        });
    }
}

const ROW_HEIGHT: f32 = 30.0;
const NAME_WIDTH: f32 = 170.0;
const SIZES_WIDTH: f32 = 130.0;
const PCT_WIDTH: f32 = 52.0;
const GAP: f32 = 16.0;
const BAR_HEIGHT: f32 = 9.0;

/// Removed bytes are drawn as small squares, after JPEG's 8×8 blocks.
const BLOCK: f32 = 4.0;
const BLOCK_GAP: f32 = 1.0;

/// One file: name, a bar as long as its original size, then sizes or status.
fn file_row(ui: &mut egui::Ui, row: &Row, largest: u64) {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let painter = ui.painter_at(rect);
    let font = FontId::proportional(13.5);
    let cy = rect.center().y;

    let name = row.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut job = LayoutJob::single_section(name, TextFormat::simple(font.clone(), p.text));
    job.wrap = TextWrapping { max_width: NAME_WIDTH - 8.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = painter.layout_job(job);
    painter.galley(pos2(rect.left(), cy - galley.size().y / 2.0), galley, p.text);

    let info_right = rect.right() - 4.0;
    let bar_left = rect.left() + NAME_WIDTH;
    let bar_space = (info_right - SIZES_WIDTH - PCT_WIDTH - GAP - bar_left).max(40.0);
    let full = (bar_space * row.size as f32 / largest as f32).max(6.0).round();
    let bar = Rect::from_min_size(pos2(bar_left, (cy - BAR_HEIGHT / 2.0).round()), vec2(full, BAR_HEIGHT));

    let right_text = |text: String, x: f32, color: Color32| {
        painter.text(pos2(x, cy), Align2::RIGHT_CENTER, text, font.clone(), color);
    };
    let status_text = |text: &str, color: Color32| right_text(text.into(), info_right, color);

    match &row.status {
        Status::Done(Outcome::Compressed { before, after, quality, dssim }) => {
            let kept_w = (full * *after as f32 / *before as f32).round();
            painter.rect_filled(Rect::from_min_size(bar.min, vec2(kept_w, BAR_HEIGHT)), 2.0, p.kept);
            paint_blocks(&painter, Rect::from_min_max(pos2(bar.left() + kept_w + BLOCK_GAP, bar.top()), bar.max), p.saved);
            right_text(before_after(*before, *after), info_right - PCT_WIDTH, p.text);
            let pct = (before - after) as f64 * 100.0 / *before as f64;
            right_text(format!("−{pct:.0}%"), info_right, p.saved);
            response.on_hover_text(format!("{}\nQuality {quality}, DSSIM {dssim:.5}", row.path.display()));
        }
        status => {
            painter.rect_filled(bar, 2.0, p.track);
            let tip = match status {
                Status::Queued => {
                    status_text("Queued", p.muted);
                    None
                }
                Status::Working => {
                    painter.rect_filled(bar, 2.0, p.kept.gamma_multiply(0.5));
                    status_text("Working", p.text);
                    None
                }
                Status::Failed(e) => {
                    status_text("Couldn't shrink", p.error);
                    Some(e.clone())
                }
                Status::Done(Outcome::AlreadyProcessed { .. }) => {
                    status_text("Already shrunk", p.muted);
                    None
                }
                Status::Done(Outcome::NoGain { best, .. }) => {
                    status_text("Kept original", p.muted);
                    Some(match best {
                        Some(b) => format!("Best result was {}, below the minimum saving", format_size(*b)),
                        None => "No quality setting stayed visually identical".into(),
                    })
                }
                Status::Done(Outcome::Skipped { reason, .. }) => {
                    status_text(&format!("Skipped: {reason}"), p.muted);
                    None
                }
                Status::Done(Outcome::Compressed { .. }) => unreachable!(),
            };
            let path = row.path.display().to_string();
            response.on_hover_text(match tip {
                Some(t) => format!("{path}\n{t}"),
                None => path,
            });
        }
    }
}

/// Fill `area` with a grid of small squares.
fn paint_blocks(painter: &egui::Painter, area: Rect, color: Color32) {
    let step = BLOCK + BLOCK_GAP;
    let mut y = area.top();
    while y + BLOCK <= area.bottom() + 0.5 {
        let mut x = area.left();
        while x + BLOCK <= area.right() + 0.5 {
            painter.rect_filled(Rect::from_min_size(pos2(x, y), vec2(BLOCK, BLOCK)), 0.0, color);
            x += step;
        }
        y += step;
    }
}

/// "4.5 → 2.1 MB", or "1.2 MB → 640.0 KB" when the units differ.
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

        let chrome = egui::Frame::side_top_panel(ui.style()).fill(p.chrome).inner_margin(egui::Margin::symmetric(14, 10));
        egui::Panel::top("settings").frame(chrome).show(ui, |ui| self.settings_bar(ui));
        if !self.rows.is_empty() {
            egui::Panel::bottom("footer").frame(chrome).show(ui, |ui| self.footer(ui));
        }
        let body = egui::Frame::central_panel(ui.style()).fill(p.bg).inner_margin(egui::Margin::symmetric(14, 10));
        egui::CentralPanel::default().frame(body).show(ui, |ui| {
            if self.rows.is_empty() {
                self.empty_state(ui, hovering);
            } else {
                self.file_list(ui);
                if hovering {
                    let r = ui.max_rect();
                    let veil = Color32::from_rgba_unmultiplied(p.bg.r(), p.bg.g(), p.bg.b(), 225);
                    ui.painter().rect(r, 12.0, veil, Stroke::new(2.0, p.saved), StrokeKind::Inside);
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, "Release to add", FontId::proportional(24.0), p.text);
                }
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.prefs);
    }
}
