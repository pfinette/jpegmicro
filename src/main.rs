use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, StrokeKind};
use jpegmicro::{Outcome, Settings, Strictness, collect_jpegs, format_size, process_file, worker_pool};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JPEGmicro")
            .with_inner_size([760.0, 540.0])
            .with_min_inner_size([520.0, 360.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("JPEGmicro", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
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
        self.rows.extend(files.iter().map(|p| Row { path: p.clone(), status: Status::Queued }));

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
        ui.horizontal(|ui| {
            ui.label("Strictness");
            egui::ComboBox::from_id_salt("strictness")
                .selected_text(self.prefs.strictness.label())
                .show_ui(ui, |ui| {
                    for s in Strictness::ALL {
                        ui.selectable_value(&mut self.prefs.strictness, s, s.label());
                    }
                });
            ui.add_space(16.0);
            ui.label("Min. saving");
            ui.add(egui::Slider::new(&mut self.prefs.min_saving_percent, 0.0..=30.0).suffix("%").integer());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("Originals go to the Trash").weak());
            });
        });
    }

    fn drop_zone(&self, ui: &mut egui::Ui, hovering: bool) {
        let height = if self.rows.is_empty() { ui.available_height() } else { 90.0 };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
        let visuals = ui.visuals();
        let accent = visuals.selection.bg_fill;
        let (fill, stroke) = if hovering {
            (accent.gamma_multiply(0.25), Stroke::new(2.0, accent))
        } else {
            (visuals.extreme_bg_color, Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color))
        };
        let painter = ui.painter();
        painter.rect(rect, 10.0, fill, stroke, StrokeKind::Inside);
        let text = if hovering { "Release to compress" } else { "Drop JPEGs or folders here" };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(if self.rows.is_empty() { 22.0 } else { 16.0 }),
            visuals.text_color(),
        );
    }

    fn file_table(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            egui::Grid::new("files").striped(true).num_columns(5).spacing([16.0, 6.0]).show(ui, |ui| {
                for h in ["File", "Before", "After", "Saved", "Status"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();

                for row in &self.rows {
                    let name = row.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    ui.label(name).on_hover_text(row.path.display().to_string());
                    match &row.status {
                        Status::Queued => {
                            blank(ui, 3);
                            ui.label(RichText::new("Queued").weak());
                        }
                        Status::Working => {
                            blank(ui, 3);
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Working");
                            });
                        }
                        Status::Done(o) => outcome_cells(ui, o),
                        Status::Failed(e) => {
                            blank(ui, 3);
                            ui.label(RichText::new("Error").color(Color32::from_rgb(220, 80, 70))).on_hover_text(e);
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
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
            ui.label(format!("{done} of {} files", self.rows.len()));
            ui.separator();
            ui.label(RichText::new(format!("Saved {} ({pct:.0}%)", format_size(saved))).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!self.busy() && !self.rows.is_empty(), egui::Button::new("Clear")).clicked() {
                    self.rows.clear();
                }
            });
        });
    }
}

/// Empty grid cells.
fn blank(ui: &mut egui::Ui, n: usize) {
    for _ in 0..n {
        ui.label("");
    }
}

fn outcome_cells(ui: &mut egui::Ui, o: &Outcome) {
    match o {
        Outcome::Compressed { before, after, quality, dssim } => {
            ui.label(format_size(*before));
            ui.label(format_size(*after));
            ui.label(format!("{:.0}%", (before - after) as f64 * 100.0 / *before as f64));
            ui.label(RichText::new("Done").color(Color32::from_rgb(70, 170, 90)))
                .on_hover_text(format!("Quality {quality}, DSSIM {dssim:.5}"));
        }
        Outcome::AlreadyProcessed { size } => {
            ui.label(format_size(*size));
            blank(ui, 2);
            ui.label(RichText::new("Already done").weak());
        }
        Outcome::NoGain { size, best } => {
            ui.label(format_size(*size));
            blank(ui, 2);
            let tip = match best {
                Some(b) => format!("Best result was {}, not enough smaller", format_size(*b)),
                None => "No quality setting stayed visually identical".into(),
            };
            ui.label(RichText::new("No gain").weak()).on_hover_text(tip);
        }
        Outcome::Skipped { size, reason } => {
            ui.label(format_size(*size));
            blank(ui, 2);
            ui.label(RichText::new(format!("Skipped: {reason}")).weak());
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

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

        egui::Panel::top("settings").show(ui, |ui| {
            ui.add_space(6.0);
            self.settings_bar(ui);
            ui.add_space(6.0);
        });
        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(6.0);
            self.footer(ui);
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.drop_zone(ui, hovering);
            if !self.rows.is_empty() {
                ui.add_space(10.0);
                self.file_table(ui);
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.prefs);
    }
}
