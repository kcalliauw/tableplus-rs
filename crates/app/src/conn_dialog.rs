//! New/edit connection dialog with test connection, TLS and SSH settings.

use eframe::egui::{self, Color32, CornerRadius, Key, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, vec2};
use tableplus_core::config::{ColorTag, ConnectionConfig, Credentials, DriverKind, EnvTag, SshAuth, SslMode};
use tableplus_core::driver;
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

pub struct ConnDialog {
    pub config: ConnectionConfig,
    is_new: bool,
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
        let get = |k| secrets.get_for(config.id, k).unwrap_or_default();
        Self {
            password: get(SecretKind::Password),
            ssh_password: get(SecretKind::SshPassword),
            ssh_passphrase: get(SecretKind::SshPassphrase),
            port: config.port.to_string(),
            ssh_port: config.ssh.port.to_string(),
            config,
            is_new,
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
            ui.horizontal(|ui| {
                ui.heading(if self.is_new { "New Connection" } else { "Edit Connection" });
            });
            ui.add_space(10.0);

            // Driver picker.
            ui.horizontal(|ui| {
                for d in DriverKind::ALL {
                    let selected = self.config.driver == d;
                    let (rect, resp) = ui.allocate_exact_size(vec2(132.0, 48.0), Sense::click());
                    let stroke = if selected { Stroke::new(1.5, p.accent) } else { Stroke::new(1.0, p.border) };
                    let fill = if selected {
                        p.accent.gamma_multiply(0.08)
                    } else if resp.hovered() {
                        p.hover.gamma_multiply(0.6)
                    } else {
                        p.content_bg
                    };
                    ui.painter().rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
                    let badge = egui::Rect::from_min_size(rect.min + vec2(10.0, 10.0), vec2(28.0, 28.0));
                    driver_badge(ui, badge, d);
                    ui.painter().text(
                        egui::pos2(badge.right() + 9.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        d.label(),
                        theme::ui_font(12.5),
                        p.text,
                    );
                    if resp.clicked() {
                        self.set_driver(d);
                    }
                }
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
                row(ui, "Database file", &mut |ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.config.path)
                            .hint_text("/path/to/database.sqlite")
                            .desired_width(full(ui) - 84.0),
                    );
                    if widgets::button(ui, "Browse…").clicked()
                        && let Some(path) =
                            rfd::FileDialog::new().add_filter("SQLite", &["sqlite", "sqlite3", "db", "db3"]).pick_file()
                    {
                        self.config.path = path.display().to_string();
                    }
                });
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
                    ui.checkbox(&mut self.config.save_password, "Save in keychain");
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
