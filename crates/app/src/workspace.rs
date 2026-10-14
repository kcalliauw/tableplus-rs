//! The connected window: sidebar, tab strip and the active tab.

use eframe::egui::{self, Align, Align2, CornerRadius, Layout, RichText, Sense, Stroke, Ui, vec2};
use tableplus_core::Dialect;
use tableplus_core::config::ConnectionConfig;
use tableplus_core::driver::DynConnection;
use tableplus_core::store::History;

use crate::app::{Action, Shortcuts, shortcut_label};
use crate::icons::{self, Icon};
use crate::session::{Session, SessionCatalog, TabKind};
use crate::settings::Settings;
use crate::task::{Spawner, poll_slot};
use crate::theme::{self, palette};
use crate::widgets;

/// Everything a tab needs from its session and the app.
pub struct TabCtx<'a> {
    pub spawner: &'a Spawner,
    pub conn: &'a DynConnection,
    pub dialect: Dialect,
    pub settings: &'a Settings,
    pub catalog: &'a SessionCatalog,
    pub shortcuts: &'a Shortcuts,
    pub actions: &'a mut Vec<Action>,
    pub connection: &'a ConnectionConfig,
    pub egui: egui::Context,
}

pub fn show(
    ui: &mut Ui,
    session: &mut Session,
    spawner: &Spawner,
    settings: &Settings,
    history: &History,
    shortcuts: &Shortcuts,
    actions: &mut Vec<Action>,
) {
    let p = palette(ui);
    egui::Panel::left("sidebar")
        .resizable(true)
        .default_size(settings.sidebar_width)
        .size_range(180.0..=520.0)
        .frame(egui::Frame::new().fill(p.sidebar_bg))
        .show(ui, |ui| {
            crate::sidebar::show(ui, session, history, actions);
        });

    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.content_bg)).show(ui, |ui| {
        egui::Panel::top("tab_strip").exact_size(32.0).frame(egui::Frame::NONE).show(ui, |ui| {
            tab_strip(ui, session, actions);
        });
        let Session { tabs, active_tab, catalog, conn, config, dialect, .. } = session;
        let mut ctx = TabCtx {
            spawner,
            conn,
            dialect: *dialect,
            settings,
            catalog,
            shortcuts,
            actions,
            connection: config,
            egui: ui.ctx().clone(),
        };
        let no_shortcuts = Shortcuts::default();
        for (i, tab) in tabs.iter_mut().enumerate() {
            let active = Some(i) == *active_tab;
            ctx.shortcuts = if active { shortcuts } else { &no_shortcuts };
            match &mut tab.kind {
                TabKind::Table(t) => t.poll(&mut ctx),
                TabKind::Query(q) => q.poll(&mut ctx),
                TabKind::Definition(d) => {
                    if let Some(r) = poll_slot(&mut d.task) {
                        d.ddl = Some(r.map_err(|e| format!("{e:#}")));
                    }
                }
            }
        }
        ctx.shortcuts = shortcuts;
        match active_tab.and_then(|i| tabs.get_mut(i)) {
            Some(tab) => {
                let id = tab.id;
                ui.push_id(id, |ui| match &mut tab.kind {
                    TabKind::Table(t) => t.ui(ui, &mut ctx),
                    TabKind::Query(q) => q.ui(ui, &mut ctx),
                    TabKind::Definition(d) => definition_view(ui, d, &mut ctx),
                });
            }
            None => empty_state(ui, session_summary(catalog)),
        }
    });
}

fn session_summary(catalog: &SessionCatalog) -> String {
    use tableplus_core::model::ObjectKind::*;
    let count = |k| catalog.objects.iter().filter(|o| o.kind == k).count();
    format!(
        "{} tables · {} views · {} routines",
        count(Table),
        count(View) + count(MaterializedView),
        count(Function) + count(Procedure)
    )
}

fn empty_state(ui: &mut Ui, summary: String) {
    let p = palette(ui);
    ui.add_space(ui.available_height() * 0.28);
    ui.vertical_centered(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
        icons::paint(ui.painter(), r, Icon::Database, p.text_faint);
        ui.add_space(8.0);
        ui.label(RichText::new("Open a table from the sidebar").font(theme::bold_font(15.0)).color(p.text_muted));
        ui.label(RichText::new(summary).color(p.text_faint));
        ui.add_space(14.0);
        for (k, what) in [
            (shortcut_label("K"), "Open anything"),
            (shortcut_label("T"), "New SQL query"),
            (shortcut_label("R"), "Reload"),
            (shortcut_label("W"), "Close tab"),
        ] {
            ui.label(RichText::new(format!("{k}   {what}")).font(theme::mono_font(12.0)).color(p.text_faint));
        }
    });
}

fn tab_strip(ui: &mut Ui, session: &mut Session, actions: &mut Vec<Action>) {
    let p = palette(ui);
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
    ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
    let mut activate = None;
    let mut close = None;
    egui::ScrollArea::horizontal()
        .id_salt("tab_scroll")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (i, tab) in session.tabs.iter().enumerate() {
                    let active = session.active_tab == Some(i);
                    let title = tab.title();
                    let font = if active { theme::bold_font(12.5) } else { theme::ui_font(12.5) };
                    let galley =
                        ui.painter().layout_no_wrap(title.clone(), font, if active { p.text } else { p.text_muted });
                    let w = (galley.size().x + 60.0).clamp(110.0, 240.0);
                    let (r, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
                    if active {
                        ui.painter().rect_filled(r, CornerRadius::ZERO, p.content_bg);
                        ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(2.0, p.accent));
                    } else if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::ZERO, p.hover.gamma_multiply(0.6));
                    }
                    ui.painter().line_segment(
                        [r.right_top() + vec2(0.0, 6.0), r.right_bottom() - vec2(0.0, 6.0)],
                        Stroke::new(1.0, p.border),
                    );
                    let icon_color = match tab.object() {
                        Some(o) => icons::kind_color(o.kind, ui.visuals().dark_mode),
                        None => p.text_muted,
                    };
                    icons::paint(
                        ui.painter(),
                        egui::Rect::from_center_size(egui::pos2(r.left() + 16.0, r.center().y), vec2(13.0, 13.0)),
                        tab.icon(),
                        icon_color,
                    );
                    let text_rect = egui::Rect::from_min_max(
                        egui::pos2(r.left() + 28.0, r.top()),
                        egui::pos2(r.right() - 24.0, r.bottom()),
                    );
                    ui.painter().with_clip_rect(text_rect).galley(
                        egui::pos2(text_rect.left(), r.center().y - galley.size().y / 2.0),
                        galley,
                        p.text,
                    );
                    let close_rect =
                        egui::Rect::from_center_size(egui::pos2(r.right() - 14.0, r.center().y), vec2(16.0, 16.0));
                    let close_resp = ui.interact(close_rect, ui.id().with(("close_tab", tab.id)), Sense::click());
                    if tab.has_unsaved_changes() && !close_resp.hovered() {
                        ui.painter().circle_filled(close_rect.center(), 3.5, p.warning);
                    } else if active || resp.hovered() || close_resp.hovered() {
                        if close_resp.hovered() {
                            ui.painter().rect_filled(close_rect, CornerRadius::same(4), p.border);
                        }
                        icons::paint(ui.painter(), close_rect.shrink(3.0), Icon::Close, p.text_muted);
                    }
                    if close_resp.clicked() || resp.middle_clicked() {
                        close = Some(i);
                    } else if resp.clicked() {
                        activate = Some(i);
                    }
                    resp.on_hover_text(match tab.object() {
                        Some(o) => o.qualified_name(),
                        None => title,
                    });
                }
                let plus = widgets::icon_button_sized(
                    ui,
                    Icon::Plus,
                    &format!("New query ({})", shortcut_label("T")),
                    vec2(30.0, 30.0),
                    true,
                );
                if plus.clicked() {
                    actions.push(Action::NewQuery(None));
                }
            });
        });
    if let Some(i) = activate {
        session.active_tab = Some(i);
    }
    if let Some(i) = close {
        actions.push(Action::CloseTab(i));
    }
}

fn definition_view(ui: &mut Ui, d: &mut crate::session::DefinitionTab, ctx: &mut TabCtx) {
    let p = palette(ui);
    egui::Panel::top(egui::Id::new(("def_head", &d.object))).exact_size(44.0).frame(egui::Frame::NONE).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            ui.add_space(14.0);
            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
            icons::paint(
                ui.painter(),
                r,
                icons::kind_icon(d.object.kind),
                icons::kind_color(d.object.kind, ui.visuals().dark_mode),
            );
            ui.label(RichText::new(d.object.qualified_name()).font(theme::bold_font(14.0)));
            if let Some(sig) = &d.object.signature {
                ui.label(RichText::new(format!("({sig})")).font(theme::mono_font(12.0)).color(p.text_muted));
            }
            ui.label(widgets::muted(ui, d.object.kind.label()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(12.0);
                if let Some(Ok(ddl)) = &d.ddl {
                    if widgets::icon_text_button(ui, Icon::Sql, "Open in SQL editor").clicked() {
                        ctx.actions.push(Action::NewQuery(Some(ddl.clone())));
                    }
                    if widgets::icon_text_button(ui, Icon::Copy, "Copy").clicked() {
                        ctx.egui.copy_text(ddl.clone());
                    }
                }
            });
        });
    });
    egui::CentralPanel::default().frame(egui::Frame::NONE.inner_margin(egui::Margin::same(14))).show(ui, |ui| match &d
        .ddl
    {
        None => crate::table_tab::loading(ui, "Loading definition…"),
        Some(Err(e)) => crate::table_tab::error_box(ui, e),
        Some(Ok(ddl)) => {
            let mut text = ddl.clone();
            let dialect = ctx.dialect;
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                let job = crate::editor::highlight(ui, buf.as_str(), dialect, 13.0, wrap);
                ui.painter().layout_job(job)
            };
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .layouter(&mut layouter),
                );
            });
        }
    });
    let _ = Align2::CENTER_CENTER;
}
