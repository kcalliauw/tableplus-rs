//! Start screen: saved connections with search, color tags and quick actions.

use std::collections::HashMap;

use eframe::egui::{self, Align2, Color32, CornerRadius, Id, Key, RichText, Sense, Stroke, StrokeKind, Ui, vec2};
use tableplus_core::config::{ConnectionConfig, DriverKind};
use tableplus_core::fuzzy::fuzzy_match;
use uuid::Uuid;

use crate::app::{Action, shortcut_label};
use crate::icons::{self, Icon};
use crate::theme::{self, palette};
use crate::widgets;

#[derive(Default)]
pub struct WelcomeState {
    pub search: String,
    pub selected: Option<Uuid>,
}

pub fn driver_color(driver: DriverKind) -> Color32 {
    match driver {
        DriverKind::Postgres => Color32::from_rgb(0x33, 0x67, 0x91),
        DriverKind::MySql => Color32::from_rgb(0x00, 0x75, 0x8F),
        DriverKind::MariaDb => Color32::from_rgb(0xA0, 0x5A, 0x3C),
        DriverKind::Sqlite => Color32::from_rgb(0x0F, 0x80, 0xCC),
    }
}

/// Rounded driver badge (`Pg`, `My`, …).
pub fn driver_badge(ui: &Ui, rect: egui::Rect, driver: DriverKind) {
    ui.painter().rect_filled(rect, CornerRadius::same((rect.height() * 0.28) as u8), driver_color(driver));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        driver.badge(),
        theme::bold_font(rect.height() * 0.38),
        Color32::WHITE,
    );
}

fn logo(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same((size * 0.24) as u8), Color32::from_rgb(0xF5, 0xB7, 0x2E));
    let inner = rect.shrink(size * 0.24);
    icons::paint(painter, inner, Icon::Database, Color32::from_rgb(0x3A, 0x2A, 0x05));
}

/// What the welcome screen shows.
pub struct WelcomeView<'a> {
    pub connections: &'a [ConnectionConfig],
    /// Connection currently being opened (spinner).
    pub connecting: Option<Uuid>,
    pub errors: &'a HashMap<Uuid, String>,
    pub secrets_backend: &'a str,
    /// False while a dialog or overlay owns the keyboard.
    pub keyboard: bool,
}

pub fn show(ui: &mut Ui, state: &mut WelcomeState, view: WelcomeView<'_>, actions: &mut Vec<Action>) {
    let WelcomeView { connections, connecting, errors, secrets_backend, keyboard } = view;
    let p = palette(ui);
    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.sidebar_bg)).show(ui, |ui| {
        let avail = ui.available_rect_before_wrap();
        let card_w = avail.width().min(960.0) - 48.0;
        let card_h = avail.height().min(640.0) - 48.0;
        let card = egui::Rect::from_center_size(avail.center(), vec2(card_w, card_h));
        ui.painter().rect(card, CornerRadius::same(12), p.content_bg, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let left = egui::Rect::from_min_size(card.min, vec2(300.0, card.height()));
        let right = egui::Rect::from_min_max(egui::pos2(left.right(), card.top()), card.max);
        ui.painter().rect_filled(
            left.shrink(1.0),
            CornerRadius { nw: 11, sw: 11, ne: 0, se: 0 },
            p.sidebar_bg.gamma_multiply(if ui.visuals().dark_mode { 1.0 } else { 0.97 }),
        );
        ui.painter().line_segment([left.right_top(), left.right_bottom()], Stroke::new(1.0, p.border));

        // Left: branding and actions.
        let mut lui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(left.shrink2(vec2(28.0, 36.0)))
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        lui.add_space(20.0);
        logo(&mut lui, 84.0);
        lui.add_space(14.0);
        lui.label(RichText::new("tableplus-rs").font(theme::bold_font(22.0)));
        lui.label(widgets::muted(&lui, format!("Version {}", env!("CARGO_PKG_VERSION"))));
        lui.add_space(28.0);
        if widgets::primary_button(&mut lui, "  Create a new connection  ").clicked() {
            actions.push(Action::NewConnection);
        }
        lui.add_space(8.0);
        if lui.add(egui::Button::new("Open quickly…").min_size(vec2(196.0, 26.0))).clicked() {
            actions.push(Action::QuickOpen);
        }
        lui.add_space(24.0);
        for (keys, what) in [
            (shortcut_label("N"), "New connection"),
            (shortcut_label("K"), "Open anything"),
            ("↵".to_string(), "Connect"),
        ] {
            lui.horizontal(|ui| {
                ui.add_space(44.0);
                ui.label(RichText::new(keys).font(theme::mono_font(11.5)).color(p.text_muted));
                ui.label(RichText::new(what).color(p.text_faint).size(12.0));
            });
        }
        lui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("Passwords: {secrets_backend}")).size(11.0).color(p.text_faint));
        });

        // Right: connection list.
        let mut rui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(right.shrink2(vec2(20.0, 18.0)))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        rui.horizontal(|ui| {
            let w = ui.available_width() - 34.0;
            let search =
                widgets::search_field(ui, &mut state.search, "Search for connection…", Id::new("welcome_search"), w);
            if keyboard && ui.memory(|m| m.focused().is_none()) && !connections.is_empty() {
                search.request_focus();
            }
            if widgets::icon_button(ui, Icon::Plus, "New connection").clicked() {
                actions.push(Action::NewConnection);
            }
        });
        rui.add_space(10.0);

        let mut list: Vec<(i64, &ConnectionConfig)> = connections
            .iter()
            .filter_map(|c| {
                let hay = format!("{} {} {}", c.display_name(), c.summary(), c.group);
                fuzzy_match(&state.search, &hay).map(|(s, _)| (s, c))
            })
            .collect();
        if !state.search.is_empty() {
            list.sort_by_key(|x| std::cmp::Reverse(x.0));
        } else {
            list.sort_by(|a, b| {
                a.1.group
                    .cmp(&b.1.group)
                    .then_with(|| a.1.display_name().to_lowercase().cmp(&b.1.display_name().to_lowercase()))
            });
        }

        if connections.is_empty() {
            rui.add_space(rui.available_height() / 3.0);
            rui.vertical_centered(|ui| {
                ui.label(RichText::new("No connections yet").font(theme::bold_font(15.0)));
                ui.label(widgets::muted(ui, "Create one to browse PostgreSQL, MySQL, MariaDB or SQLite."));
            });
            return;
        }

        // Keyboard navigation.
        let ids: Vec<Uuid> = list.iter().map(|(_, c)| c.id).collect();
        if !ids.is_empty() && keyboard {
            let (down, up, enter) =
                rui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
            let pos = state.selected.and_then(|s| ids.iter().position(|i| *i == s));
            if down {
                state.selected = Some(ids[pos.map_or(0, |p| (p + 1).min(ids.len() - 1))]);
            }
            if up {
                state.selected = Some(ids[pos.map_or(0, |p| p.saturating_sub(1))]);
            }
            if enter && let Some(id) = state.selected.or(ids.first().copied()) {
                actions.push(Action::Connect(id));
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut rui, |ui| {
            let mut last_group: Option<&str> = None;
            for (_, c) in &list {
                if state.search.is_empty() && last_group != Some(c.group.as_str()) && !c.group.is_empty() {
                    ui.add_space(4.0);
                    widgets::section_title(ui, &c.group);
                    last_group = Some(c.group.as_str());
                }
                connection_row(ui, c, state, connecting == Some(c.id), errors.get(&c.id), actions);
            }
        });
    });
}

fn connection_row(
    ui: &mut Ui,
    c: &ConnectionConfig,
    state: &mut WelcomeState,
    connecting: bool,
    error: Option<&String>,
    actions: &mut Vec<Action>,
) {
    let p = palette(ui);
    let height = if error.is_some() { 66.0 } else { 52.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let selected = state.selected == Some(c.id);
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.selection_inactive);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.hover.gamma_multiply(0.7));
    }
    if let Some(color) = widgets::tag_color(c.color) {
        let stripe = egui::Rect::from_min_size(egui::pos2(rect.left() + 4.0, rect.top() + 10.0), vec2(4.0, 32.0));
        ui.painter().rect_filled(stripe, CornerRadius::same(2), color);
    }
    let badge = egui::Rect::from_min_size(egui::pos2(rect.left() + 16.0, rect.top() + 8.0), vec2(36.0, 36.0));
    driver_badge(ui, badge, c.driver);
    let x = badge.right() + 12.0;
    let name = ui.painter().text(
        egui::pos2(x, rect.top() + 9.0),
        Align2::LEFT_TOP,
        c.display_name(),
        theme::bold_font(13.5),
        p.text,
    );
    let env_color = widgets::env_color(c.env, ui.visuals().dark_mode);
    let env_font = theme::bold_font(9.5);
    let env_galley = ui.painter().layout_no_wrap(c.env.label().to_uppercase(), env_font, env_color);
    let env_rect = egui::Rect::from_min_size(
        egui::pos2(name.right() + 8.0, name.center().y - 7.5),
        env_galley.size() + vec2(10.0, 4.0),
    );
    ui.painter().rect_filled(env_rect, CornerRadius::same(4), env_color.gamma_multiply(0.15));
    ui.painter().galley(env_rect.center() - env_galley.size() / 2.0, env_galley, env_color);
    if c.tls.mode != tableplus_core::config::SslMode::Disable && !c.driver.is_file_based() {
        icons::paint(
            ui.painter(),
            egui::Rect::from_min_size(egui::pos2(env_rect.right() + 6.0, name.top() + 1.0), vec2(12.0, 12.0)),
            Icon::Lock,
            p.text_faint,
        );
    }
    let summary_rect =
        egui::Rect::from_min_max(egui::pos2(x, rect.top() + 28.0), egui::pos2(rect.right() - 40.0, rect.top() + 46.0));
    ui.painter().with_clip_rect(summary_rect).text(
        summary_rect.left_top(),
        Align2::LEFT_TOP,
        c.summary(),
        theme::ui_font(12.0),
        p.text_muted,
    );
    if let Some(e) = error {
        let first = e.lines().next().unwrap_or_default();
        let er =
            egui::Rect::from_min_max(egui::pos2(x, rect.top() + 46.0), egui::pos2(rect.right() - 12.0, rect.bottom()));
        ui.painter().with_clip_rect(er).text(er.left_top(), Align2::LEFT_TOP, first, theme::ui_font(11.5), p.danger);
    }
    if connecting {
        let spin = egui::Rect::from_center_size(egui::pos2(rect.right() - 22.0, rect.top() + 26.0), vec2(16.0, 16.0));
        ui.put(spin, egui::Spinner::new().size(14.0));
    } else if resp.hovered() {
        let chev = egui::Rect::from_center_size(egui::pos2(rect.right() - 20.0, rect.top() + 26.0), vec2(12.0, 12.0));
        icons::paint(ui.painter(), chev, Icon::ChevronRight, p.text_faint);
    }
    if resp.clicked() {
        state.selected = Some(c.id);
    }
    if resp.double_clicked() {
        actions.push(Action::Connect(c.id));
    }
    resp.context_menu(|ui| {
        state.selected = Some(c.id);
        if ui.button("Connect").clicked() {
            actions.push(Action::Connect(c.id));
        }
        if ui.button("Edit…").clicked() {
            actions.push(Action::EditConnection(c.id));
        }
        if ui.button("Duplicate").clicked() {
            actions.push(Action::DuplicateConnection(c.id));
        }
        ui.separator();
        if ui.button(RichText::new("Delete").color(palette(ui).danger)).clicked() {
            actions.push(Action::DeleteConnection(c.id));
        }
    });
}
