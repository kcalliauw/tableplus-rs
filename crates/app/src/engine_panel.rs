//! Installing a helper engine such as DuckDB: the panel the connection form shows while the engine
//! is missing, and the row Preferences lists it in. Nothing downloads until the user clicks Install.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, RichText, Ui};
use tableplus_core::engines::{self, Availability, HelperEngine};

use crate::task::{Spawner, Task};
use crate::theme::palette;
use crate::widgets;

/// Bytes received so far and the size the server announced.
type Received = Arc<Mutex<(u64, Option<u64>)>>;

pub struct Installer {
    pub engine: &'static HelperEngine,
    availability: Availability,
    /// What the install folder holds, read with the availability rather than every frame,
    /// because checking a helper hashes all of it.
    installed: Vec<engines::Installed>,
    download: Option<(Task<PathBuf>, Received)>,
    error: Option<String>,
}

impl Installer {
    pub fn new(engine: &'static HelperEngine) -> Self {
        Self {
            engine,
            availability: engines::availability(engine),
            installed: engines::installed(engine),
            download: None,
            error: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.availability.is_ready()
    }

    /// What the engine card shows while the engine isn't installed: its size on disk.
    pub fn size_label(&self) -> String {
        match &self.availability {
            Availability::Installable(release) => format!("{:.0} MB", release.size as f64 / 1_000_000.0),
            _ => self.engine.size_label.to_string(),
        }
    }

    pub fn is_installing(&self) -> bool {
        self.download.is_some()
    }

    /// Checks the install folder again, after an install or a removal.
    pub fn refresh(&mut self) {
        self.availability = engines::availability(self.engine);
        self.installed = engines::installed(self.engine);
    }

    /// Picks up a finished download. Returns true when the engine just became ready.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let Some((task, _)) = &mut self.download else { return false };
        let Some(result) = task.poll() else {
            ctx.request_repaint_after(Duration::from_millis(100));
            return false;
        };
        self.download = None;
        self.finish(result.map(|_| ()))
    }

    fn finish(&mut self, result: anyhow::Result<()>) -> bool {
        self.error = result.err().map(|e| format!("{e:#}"));
        self.refresh();
        self.error.is_none() && self.is_ready()
    }

    pub fn start(&mut self, spawner: &Spawner) {
        let received: Received = Arc::default();
        let progress = received.clone();
        let engine = self.engine;
        self.error = None;
        let task = spawner.spawn(async move {
            engines::install(engine, &mut |done, total| *progress.lock().unwrap() = (done, total)).await
        });
        self.download = Some((task, received));
    }

    fn install_from_file(&mut self) {
        let Some(file) = rfd::FileDialog::new()
            .set_title(format!("Install {} from a release file", self.engine.label))
            .add_filter("Helper release", &["gz"])
            .pick_file()
        else {
            return;
        };
        let result = engines::install_from_file(self.engine, &file).map(|_| ());
        self.finish(result);
    }

    /// The panel above the connection form: what the install costs, its progress, its error.
    pub fn show_panel(&mut self, ui: &mut Ui, spawner: &Spawner) {
        let p = palette(ui);
        let label = self.engine.label;
        egui::Frame::new()
            .fill(p.accent.gamma_multiply(0.08))
            .stroke(egui::Stroke::new(1.0, p.accent.gamma_multiply(0.5)))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match self.availability.clone() {
                    Availability::Ready(_) => {
                        ui.label(RichText::new(format!("{label} support is installed.")).color(p.success));
                    }
                    Availability::Unreleased => {
                        ui.label(format!(
                            "{label} support isn't installed, and there is no release of it for {} yet.",
                            engines::platform()
                        ));
                    }
                    Availability::Installable(release) => {
                        ui.label(RichText::new(format!("{label} support isn't installed.")).strong());
                        ui.label(widgets::muted(
                            ui,
                            format!(
                                "{label} runs in a separate program: a {} download that takes {} on disk.",
                                engines::megabytes(release.download_size),
                                engines::megabytes(release.size)
                            ),
                        ));
                        ui.add_space(6.0);
                        if let Some((_, received)) = &self.download {
                            let (done, total) = *received.lock().unwrap();
                            let total = total.unwrap_or(release.download_size).max(1);
                            ui.add(egui::ProgressBar::new(done as f32 / total as f32).show_percentage().text(format!(
                                "Downloading {} of {}",
                                engines::megabytes(done),
                                engines::megabytes(total)
                            )));
                        } else {
                            ui.horizontal(|ui| {
                                if widgets::primary_button(ui, "Install").clicked() {
                                    self.start(spawner);
                                }
                                if widgets::button(ui, "Install from file…").clicked() {
                                    self.install_from_file();
                                }
                            });
                        }
                    }
                }
                if let Some(e) = &self.error {
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(RichText::new(e.as_str()).color(p.danger)).wrap());
                }
            });
    }

    /// One row in Preferences: the installed version and its size, with Install, Update or Remove.
    pub fn show_row(&mut self, ui: &mut Ui, spawner: &Spawner) {
        let installed = self.installed.clone();
        let current = installed.iter().find(|i| i.current);
        // This app's version is there, but its hash doesn't match the release.
        let damaged = current.is_none() && installed.iter().any(|i| i.version == self.engine.version);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} {}", self.engine.label, self.engine.version)).strong());
                match (&self.availability, current) {
                    (Availability::Ready(path), None) => {
                        ui.label(widgets::muted(ui, format!("development build at {}", path.display())));
                    }
                    (_, Some(c)) => {
                        ui.label(widgets::muted(ui, format!("installed · {}", engines::megabytes(c.size))));
                    }
                    (Availability::Installable(_), None) if damaged => {
                        ui.label(RichText::new("doesn't match this app's release").color(palette(ui).danger));
                    }
                    (Availability::Installable(_), None) if !installed.is_empty() => {
                        ui.label(widgets::muted(ui, "an older version is installed"));
                    }
                    _ => {
                        ui.label(widgets::muted(ui, "not installed"));
                    }
                }
            });
            ui.horizontal(|ui| {
                if self.is_installing() {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(widgets::muted(ui, "Downloading…"));
                    return;
                }
                let releasable = matches!(self.availability, Availability::Installable(_));
                if releasable {
                    let verb = match () {
                        _ if installed.is_empty() => "Install",
                        _ if damaged => "Reinstall",
                        _ => "Update",
                    };
                    if widgets::button(ui, verb).clicked() {
                        self.start(spawner);
                    }
                }
                if !installed.is_empty() && widgets::button(ui, "Remove").clicked() {
                    let result = engines::remove(self.engine);
                    self.finish(result);
                }
            });
            if let Some(e) = &self.error {
                ui.add(egui::Label::new(RichText::new(e.as_str()).color(palette(ui).danger)).wrap());
            }
        });
    }
}
