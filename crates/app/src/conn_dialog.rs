//! New/edit connection dialog with test connection, TLS and SSH settings.

use eframe::egui::{self, Color32, CornerRadius, Key, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, vec2};
use tableplus_core::config::{ColorTag, ConnectionConfig, Credentials, DriverKind, EnvTag, SshAuth, SslMode};
use tableplus_core::driver;
use tableplus_core::duckdb::{self, DuckDbTarget};
use tableplus_core::engines;
use tableplus_core::secrets::{SecretKind, SecretStore};

use crate::task::{Spawner, Task, poll_slot};
use crate::theme::{self, palette};
use crate::welcome::driver_badge;
use crate::widgets;

pub enum DialogOutcome {
    Open,
    Cancel,
    Save { connect: bool },
}

/// New Connection first asks for the engine, then shows the form; Edit opens the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Engine,
    Form,
}

pub struct ConnDialog {
    pub config: ConnectionConfig,
    is_new: bool,
    step: Step,
    /// Engines whose helper program isn't installed, checked when the dialog opens.
    missing_helpers: Vec<DriverKind>,
    password: String,
    ssh_password: String,
    ssh_passphrase: String,
    port: String,
    ssh_port: String,
    url: String,
    url_error: Option<String>,
    test: Option<Task<String>>,
    test_result: Option<Result<String, String>>,
}

impl ConnDialog {
    pub fn new(config: ConnectionConfig, is_new: bool, secrets: &SecretStore) -> Self {
        let file_based = config.driver.is_file_based();
        let get = |k| if file_based { String::new() } else { secrets.get_for(config.id, k).unwrap_or_default() };
        Self {
            password: get(SecretKind::Password),
            ssh_password: get(SecretKind::SshPassword),
            ssh_passphrase: get(SecretKind::SshPassphrase),
            port: config.port.to_string(),
            ssh_port: config.ssh.port.to_string(),
            config,
            is_new,
            step: if is_new { Step::Engine } else { Step::Form },
            missing_helpers: DriverKind::ALL
                .into_iter()
                .filter(|d| d.helper().is_some_and(|e| !engines::is_available(e)))
                .collect(),
            url: String::new(),
            url_error: None,
            test: None,
            test_result: None,
        }
    }

    pub fn credentials(&self) -> Credentials {
        let opt = |s: &String| (!s.is_empty()).then(|| s.clone());
        Credentials {
            password: opt(&self.password),
            ssh_password: opt(&self.ssh_password),
            ssh_passphrase: opt(&self.ssh_passphrase),
        }
    }

    pub fn save_secrets(&self, secrets: &SecretStore) -> anyhow::Result<()> {
        // New file connections never had secrets; an edited one keeps its engine.
        if self.config.driver.is_file_based() {
            return Ok(());
        }
        let id = self.config.id;
        if self.config.save_password {
            secrets.set_for(id, SecretKind::Password, &self.password)?;
            secrets.set_for(id, SecretKind::SshPassword, &self.ssh_password)?;
            secrets.set_for(id, SecretKind::SshPassphrase, &self.ssh_passphrase)?;
        } else {
            secrets.delete_all(id)?;
        }
        Ok(())
    }

    fn sync_ports(&mut self) {
        if let Ok(p) = self.port.trim().parse() {
            self.config.port = p;
        }
        if let Ok(p) = self.ssh_port.trim().parse() {
            self.config.ssh.port = p;
        }
    }

    fn set_driver(&mut self, driver: DriverKind) {
        if self.config.driver == driver {
            return;
        }
        let old = self.config.driver;
        if self.config.port == old.default_port() || self.port.trim().is_empty() {
            self.config.port = driver.default_port();
            self.port = self.config.port.to_string();
        }
        if self.config.user.is_empty() || self.config.user == old.default_user() {
            self.config.user = driver.default_user().into();
        }
        if !driver.is_file_based() && self.config.host.is_empty() {
            self.config.host = "127.0.0.1".into();
        }
        self.config.driver = driver;
    }

    pub fn show(&mut self, ctx: &egui::Context, spawner: &Spawner) -> DialogOutcome {
        if let Some(r) = poll_slot(&mut self.test) {
            self.test_result = Some(r.map_err(|e| format!("{e:#}")));
        }
        let mut outcome = DialogOutcome::Open;
        let modal = crate::widgets::modal(ctx, "conn_dialog").show(ctx, |ui| {
            ui.set_width(580.0);
            let p = palette(ui);
            if self.step == Step::Engine {
                ui.heading("New Connection");
                ui.label(widgets::muted(ui, "Choose a database engine."));
                ui.add_space(12.0);
                if let Some(d) = self.engine_grid(ui) {
                    self.set_driver(d);
                    self.test_result = None;
                    self.step = Step::Form;
                }
                ui.add_space(4.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, "Cancel").clicked() {
                        outcome = DialogOutcome::Cancel;
                    }
                });
                return;
            }
            if self.is_new {
                let back = ui.add(
                    egui::Label::new(RichText::new("‹ Back").color(p.accent).family(theme::semibold()))
                        .sense(Sense::click()),
                );
                if back.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.step = Step::Engine;
                }
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let (badge, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
                driver_badge(ui, badge, self.config.driver);
                let verb = if self.is_new { "New" } else { "Edit" };
                ui.heading(format!("{verb} {} Connection", self.config.driver.label()));
            });
            ui.add_space(12.0);

            let label_w = 104.0;
            let row = |ui: &mut Ui, label: &str, add: &mut dyn FnMut(&mut Ui)| {
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        vec2(label_w, 24.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(RichText::new(label).color(palette(ui).text_muted));
                        },
                    );
                    add(ui);
                });
            };
            let full = |ui: &Ui| ui.available_width();

            row(ui, "Name", &mut |ui| {
                ui.add(TextEdit::singleline(&mut self.config.name).hint_text("My database").desired_width(220.0));
                ui.add_space(6.0);
                for tag in ColorTag::ALL {
                    color_swatch(ui, &mut self.config.color, tag);
                }
            });
            row(ui, "Tag", &mut |ui| {
                egui::ComboBox::from_id_salt("env_tag").selected_text(self.config.env.label()).width(140.0).show_ui(
                    ui,
                    |ui| {
                        for e in EnvTag::ALL {
                            ui.selectable_value(&mut self.config.env, e, e.label());
                        }
                    },
                );
                ui.label(RichText::new("Group").color(palette(ui).text_muted));
                ui.add(TextEdit::singleline(&mut self.config.group).hint_text("optional").desired_width(full(ui)));
            });
            ui.add_space(4.0);

            if self.config.driver.is_file_based() {
                let duck = self.config.driver == DriverKind::DuckDb;
                let memory = duck && self.config.path.trim() == duckdb::MEMORY;
                row(ui, "Database file", &mut |ui| {
                    let hint = if duck {
                        "/path/to/database.duckdb, or a .parquet, .csv or .json file"
                    } else {
                        "/path/to/database.sqlite"
                    };
                    ui.add_enabled(
                        !memory,
                        TextEdit::singleline(&mut self.config.path).hint_text(hint).desired_width(full(ui) - 84.0),
                    );
                    if ui.add_enabled(!memory, egui::Button::new("Browse…").min_size(vec2(0.0, 26.0))).clicked()
                        && let Some(path) = file_picker(self.config.driver).pick_file()
                    {
                        self.config.path = path.display().to_string();
                    }
                });
                if duck {
                    self.duckdb_options(ui, label_w, memory);
                }
            } else {
                row(ui, "Host", &mut |ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.config.host)
                            .hint_text("127.0.0.1")
                            .desired_width(full(ui) - 130.0),
                    );
                    ui.label(RichText::new("Port").color(palette(ui).text_muted));
                    ui.add(TextEdit::singleline(&mut self.port).desired_width(full(ui)));
                });
                row(ui, "User", &mut |ui| {
                    ui.add(TextEdit::singleline(&mut self.config.user).desired_width(full(ui)));
                });
                row(ui, "Password", &mut |ui| {
                    ui.add(TextEdit::singleline(&mut self.password).password(true).desired_width(full(ui) - 150.0));
                    ui.checkbox(&mut self.config.save_password, crate::util::SAVE_PASSWORD_LABEL);
                });
                row(ui, "Database", &mut |ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.config.database).hint_text("default").desired_width(full(ui)),
                    );
                });
                row(ui, "SSL mode", &mut |ui| {
                    egui::ComboBox::from_id_salt("ssl_mode")
                        .selected_text(self.config.tls.mode.label())
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for m in SslMode::ALL {
                                ui.selectable_value(&mut self.config.tls.mode, m, m.label());
                            }
                        });
                    if matches!(self.config.tls.mode, SslMode::VerifyCa | SslMode::VerifyFull) {
                        ui.add(
                            TextEdit::singleline(&mut self.config.tls.ca_cert)
                                .hint_text("CA certificate (PEM)")
                                .desired_width(full(ui)),
                        );
                    }
                });

                ui.add_space(6.0);
                ui.separator();
                row(ui, "", &mut |ui| {
                    ui.checkbox(
                        &mut self.config.ssh.enabled,
                        RichText::new("Connect over SSH").family(theme::semibold()),
                    );
                });
                if self.config.ssh.enabled {
                    row(ui, "SSH host", &mut |ui| {
                        ui.add(
                            TextEdit::singleline(&mut self.config.ssh.host)
                                .hint_text("bastion.example.com")
                                .desired_width(full(ui) - 130.0),
                        );
                        ui.label(RichText::new("Port").color(palette(ui).text_muted));
                        ui.add(TextEdit::singleline(&mut self.ssh_port).desired_width(full(ui)));
                    });
                    row(ui, "SSH user", &mut |ui| {
                        ui.add(TextEdit::singleline(&mut self.config.ssh.user).desired_width(full(ui)));
                    });
                    row(ui, "Auth", &mut |ui| {
                        widgets::segmented(
                            ui,
                            &mut self.config.ssh.auth,
                            &[(SshAuth::Password, "Password"), (SshAuth::PrivateKey, "Private key")],
                        );
                    });
                    match self.config.ssh.auth {
                        SshAuth::Password => row(ui, "SSH password", &mut |ui| {
                            ui.add(TextEdit::singleline(&mut self.ssh_password).password(true).desired_width(full(ui)));
                        }),
                        SshAuth::PrivateKey => {
                            row(ui, "Key file", &mut |ui| {
                                ui.add(
                                    TextEdit::singleline(&mut self.config.ssh.key_path)
                                        .hint_text("~/.ssh/id_ed25519")
                                        .desired_width(full(ui) - 84.0),
                                );
                                if widgets::button(ui, "Browse…").clicked()
                                    && let Some(path) = rfd::FileDialog::new().pick_file()
                                {
                                    self.config.ssh.key_path = path.display().to_string();
                                }
                            });
                            row(ui, "Passphrase", &mut |ui| {
                                ui.add(
                                    TextEdit::singleline(&mut self.ssh_passphrase)
                                        .password(true)
                                        .hint_text("if the key is encrypted")
                                        .desired_width(full(ui)),
                                );
                            });
                        }
                    }
                }
                ui.separator();
                row(ui, "From URL", &mut |ui| {
                    let r = ui.add(
                        TextEdit::singleline(&mut self.url)
                            .hint_text("postgres://user:pass@host:5432/db")
                            .desired_width(full(ui) - 70.0),
                    );
                    let go = widgets::button(ui, "Apply").clicked()
                        || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
                    if go && !self.url.trim().is_empty() {
                        match ConnectionConfig::from_url(self.url.trim()) {
                            Ok((parsed, password)) => {
                                let keep = (
                                    self.config.id,
                                    self.config.name.clone(),
                                    self.config.color,
                                    self.config.env,
                                    self.config.group.clone(),
                                );
                                self.config = parsed;
                                (
                                    self.config.id,
                                    self.config.name,
                                    self.config.color,
                                    self.config.env,
                                    self.config.group,
                                ) = keep;
                                if let Some(pw) = password {
                                    self.password = pw;
                                }
                                self.port = self.config.port.to_string();
                                self.url_error = None;
                                self.url.clear();
                            }
                            Err(e) => self.url_error = Some(e.to_string()),
                        }
                    }
                });
                if let Some(e) = &self.url_error {
                    row(ui, "", &mut |ui| {
                        ui.label(RichText::new(e.as_str()).color(palette(ui).danger));
                    });
                }
            }

            ui.add_space(12.0);
            self.sync_ports();
            ui.horizontal(|ui| {
                let testing = self.test.is_some();
                if ui.add_enabled(!testing, egui::Button::new("Test").min_size(vec2(64.0, 26.0))).clicked() {
                    let cfg = self.config.clone();
                    let creds = self.credentials();
                    self.test_result = None;
                    self.test = Some(spawner.spawn(async move { driver::test_connection(&cfg, &creds).await }));
                }
                if testing {
                    ui.add(egui::Spinner::new().size(14.0));
                } else if let Some(r) = &self.test_result {
                    let (text, color) = match r {
                        Ok(v) => (format!("Connection is OK · {v}"), p.success),
                        Err(e) => (e.lines().next().unwrap_or_default().to_string(), p.danger),
                    };
                    ui.add(egui::Label::new(RichText::new(text).color(color)).truncate()).on_hover_text(match r {
                        Ok(v) => v.clone(),
                        Err(e) => e.clone(),
                    });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let valid = if self.config.driver.is_file_based() {
                        !self.config.path.trim().is_empty()
                    } else {
                        !self.config.host.trim().is_empty()
                    };
                    if widgets::primary_button_enabled(ui, "Connect", valid).clicked() {
                        outcome = DialogOutcome::Save { connect: true };
                    }
                    if ui.add_enabled(valid, egui::Button::new("Save").min_size(vec2(64.0, 26.0))).clicked() {
                        outcome = DialogOutcome::Save { connect: false };
                    }
                    if widgets::button(ui, "Cancel").clicked() {
                        outcome = DialogOutcome::Cancel;
                    }
                });
            });
        });
        if modal.should_close() && matches!(outcome, DialogOutcome::Open) {
            outcome = DialogOutcome::Cancel;
        }
        outcome
    }
}

impl ConnDialog {
    /// Large cards, three per row, one per engine. Returns the engine clicked.
    fn engine_grid(&self, ui: &mut Ui) -> Option<DriverKind> {
        let p = palette(ui);
        let (columns, gap, height) = (3, 10.0, 104.0);
        let width = (ui.available_width() - gap * (columns as f32 - 1.0)) / columns as f32;
        let mut picked = None;
        for engines in DriverKind::ALL.chunks(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for &d in engines {
                    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    let hovered = resp.hovered();
                    let stroke = if hovered { Stroke::new(1.5, p.accent) } else { Stroke::new(1.0, p.border) };
                    let fill = if hovered { p.accent.gamma_multiply(0.07) } else { p.content_bg };
                    ui.painter().rect(rect, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);
                    let name = ui.painter().layout_no_wrap(d.label().to_string(), theme::bold_font(13.0), p.text);
                    let note =
                        self.missing_helpers.contains(&d).then(|| d.helper().map(|e| e.size_label)).flatten().map(
                            |size| ui.painter().layout_no_wrap(size.to_string(), theme::ui_font(11.0), p.text_muted),
                        );
                    let badge = 36.0;
                    let note_h = note.as_ref().map_or(0.0, |n| n.size().y + 2.0);
                    let top = rect.center().y - (badge + 8.0 + name.size().y + note_h) / 2.0;
                    let badge_rect =
                        egui::Rect::from_min_size(egui::pos2(rect.center().x - badge / 2.0, top), vec2(badge, badge));
                    driver_badge(ui, badge_rect, d);
                    let name_y = badge_rect.bottom() + 8.0;
                    let name_w = name.size().x;
                    ui.painter().galley(egui::pos2(rect.center().x - name_w / 2.0, name_y), name.clone(), p.text);
                    if let Some(note) = note {
                        let y = name_y + name.size().y + 2.0;
                        let x = rect.center().x - note.size().x / 2.0;
                        ui.painter().galley(egui::pos2(x, y), note, p.text_muted);
                    }
                    if resp.clicked() {
                        picked = Some(d);
                    }
                }
            });
            ui.add_space(gap);
        }
        picked
    }

    /// In memory and read-only, and what a data file as the path does.
    fn duckdb_options(&mut self, ui: &mut Ui, label_w: f32, memory: bool) {
        let target = DuckDbTarget::of(&self.config).ok();
        let data_file = matches!(target, Some(DuckDbTarget::DataFile { .. }));
        ui.horizontal(|ui| {
            ui.add_space(label_w + ui.spacing().item_spacing.x);
            let mut in_memory = memory;
            if ui.checkbox(&mut in_memory, "In memory").changed() {
                self.config.path = if in_memory { duckdb::MEMORY.into() } else { String::new() };
                self.config.read_only = false;
            }
            if !in_memory && !data_file {
                ui.checkbox(&mut self.config.read_only, "Read-only").on_hover_text(
                    "Other programs can read the file while it is open here. Only one program at a time can open \
                     a DuckDB file for writing.",
                );
            }
        });
        let note = match target {
            Some(DuckDbTarget::DataFile { view, .. }) => Some(format!(
                "Opens the file as a view named \"{view}\" in an in-memory database. The file is only read."
            )),
            Some(DuckDbTarget::Memory) => Some("The database lives in memory and is gone when you disconnect.".into()),
            _ => None,
        };
        if let Some(note) = note {
            ui.horizontal(|ui| {
                ui.add_space(label_w + ui.spacing().item_spacing.x);
                ui.vertical(|ui| {
                    ui.set_max_width(ui.available_width());
                    ui.add(egui::Label::new(widgets::muted(ui, note)).wrap());
                });
            });
        }
    }
}

/// The Browse… dialog for a file-based engine's database file.
fn file_picker(driver: DriverKind) -> rfd::FileDialog {
    match driver {
        DriverKind::DuckDb => {
            let all: Vec<&str> =
                duckdb::DATABASE_EXTENSIONS.iter().chain(duckdb::DATA_FILE_EXTENSIONS).copied().collect();
            rfd::FileDialog::new()
                .add_filter("DuckDB and data files", &all)
                .add_filter("DuckDB databases", duckdb::DATABASE_EXTENSIONS)
                .add_filter("Parquet, CSV and JSON files", duckdb::DATA_FILE_EXTENSIONS)
        }
        _ => rfd::FileDialog::new().add_filter("SQLite", &["sqlite", "sqlite3", "db", "db3"]),
    }
}

fn color_swatch(ui: &mut Ui, value: &mut ColorTag, tag: ColorTag) {
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
    let p = palette(ui);
    let selected = *value == tag;
    match crate::widgets::tag_color(tag) {
        Some(c) => {
            ui.painter().circle_filled(rect.center(), 7.0, c);
        }
        None => {
            ui.painter().circle_stroke(rect.center(), 6.5, Stroke::new(1.0, p.text_faint));
            ui.painter().line_segment(
                [rect.center() + vec2(-4.5, 4.5), rect.center() + vec2(4.5, -4.5)],
                Stroke::new(1.0, p.text_faint),
            );
        }
    }
    if selected {
        ui.painter().circle_stroke(
            rect.center(),
            9.0,
            Stroke::new(1.5, if ui.visuals().dark_mode { Color32::WHITE } else { p.text }),
        );
    }
    if resp.on_hover_text(tag.label()).clicked() {
        *value = tag;
    }
}
