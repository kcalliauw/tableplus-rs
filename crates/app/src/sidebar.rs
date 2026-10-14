//! Sidebar: schema tree with search filter, and per-connection query history.

use chrono::Utc;
use eframe::egui::{self, Align2, CornerRadius, Id, RichText, Sense, Stroke, Ui, vec2};
use tableplus_core::fuzzy::fuzzy_match;
use tableplus_core::model::{ObjectKind, SchemaObject};
use tableplus_core::store::History;

use crate::app::Action;
use crate::icons::{self, Icon};
use crate::session::{Session, SidebarMode};
use crate::theme::{self, palette};
use crate::widgets;

const ROW: f32 = 24.0;
const KINDS: [ObjectKind; 5] =
    [ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView, ObjectKind::Function, ObjectKind::Procedure];

enum Row<'a> {
    Schema { name: &'a str, count: usize, open: bool },
    Group { schema: &'a str, kind: ObjectKind, count: usize, open: bool, depth: usize },
    Object { obj: &'a SchemaObject, depth: usize, highlight: Vec<usize>, show_schema: bool },
    Message(String),
}

pub fn show(ui: &mut Ui, session: &mut Session, history: &History, actions: &mut Vec<Action>) {
    let p = palette(ui);
    let rect = ui.max_rect();
    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, p.border));

    egui::Panel::bottom("sidebar_bottom").exact_size(40.0).frame(egui::Frame::NONE).show(ui, |ui| {
        ui.painter().line_segment([ui.max_rect().left_top(), ui.max_rect().right_top()], Stroke::new(1.0, p.border));
        ui.horizontal_centered(|ui| {
            ui.add_space(8.0);
            let mut mode = session.sidebar.mode;
            widgets::segmented(ui, &mut mode, &[(SidebarMode::Items, "Items"), (SidebarMode::History, "History")]);
            session.sidebar.mode = mode;
            if session.is_loading_objects() {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    });

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 8, right: 8, top: 8, bottom: 0 }))
        .show(ui, |ui| match session.sidebar.mode {
            SidebarMode::Items => items(ui, session, actions),
            SidebarMode::History => history_list(ui, session, history, actions),
        });
}

fn items(ui: &mut Ui, session: &mut Session, actions: &mut Vec<Action>) {
    let p = palette(ui);
    let w = ui.available_width();
    widgets::search_field(
        ui,
        &mut session.sidebar.filter,
        "Search for item…",
        Id::new(("sidebar_search", session.key)),
        w,
    );
    ui.add_space(6.0);

    let filter = session.sidebar.filter.trim().to_string();
    let objects = &session.catalog.objects;
    let mut schemas: Vec<&str> = session.schemas.iter().map(String::as_str).collect();
    for o in objects {
        if !schemas.contains(&o.schema.as_str()) {
            schemas.push(&o.schema);
        }
    }
    // The default schema (public / the connected database) comes first.
    let default_schema = session.catalog.default_schema.clone();
    schemas.sort_by_key(|s| *s != default_schema.as_str());
    let multi_schema = schemas.len() > 1;
    let expanded = &session.sidebar.expanded;
    let mut rows: Vec<Row> = Vec::new();

    if let Some(e) = &session.objects_error {
        rows.push(Row::Message(format!("Error: {}", e.lines().next().unwrap_or_default())));
    }
    if !filter.is_empty() {
        let mut matches: Vec<(i64, &SchemaObject, Vec<usize>)> =
            objects.iter().filter_map(|o| fuzzy_match(&filter, &o.name).map(|(s, h)| (s, o, h))).collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        for (_, o, h) in matches {
            rows.push(Row::Object { obj: o, depth: 0, highlight: h, show_schema: multi_schema });
        }
        if rows.is_empty() {
            rows.push(Row::Message("No matching items".into()));
        }
    } else {
        for schema in &schemas {
            let in_schema: Vec<&SchemaObject> = objects.iter().filter(|o| o.schema == *schema).collect();
            let base_depth = if multi_schema { 1 } else { 0 };
            if multi_schema {
                let open = expanded.contains(*schema);
                rows.push(Row::Schema { name: schema, count: in_schema.len(), open });
                if !open {
                    continue;
                }
            }
            for kind in KINDS {
                let of_kind: Vec<&&SchemaObject> = in_schema.iter().filter(|o| o.kind == kind).collect();
                if of_kind.is_empty() {
                    continue;
                }
                let key = format!("{schema}/{}", kind.plural());
                let open = expanded.contains(&key)
                    || (!multi_schema && kind.is_relation() && !expanded.contains(&format!("!{key}")));
                rows.push(Row::Group { schema, kind, count: of_kind.len(), open, depth: base_depth });
                if open {
                    for o in of_kind {
                        rows.push(Row::Object {
                            obj: o,
                            depth: base_depth + 1,
                            highlight: Vec::new(),
                            show_schema: false,
                        });
                    }
                }
            }
        }
        if objects.is_empty() && session.objects_task.is_some() {
            rows.push(Row::Message("Loading…".into()));
        }
    }

    let active = session.active_object().cloned();
    let mut toggle: Option<(String, bool)> = None;
    let dark = ui.visuals().dark_mode;
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW, rows.len(), |ui, range| {
        for row in &rows[range] {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
            let painter = ui.painter();
            match row {
                Row::Message(m) => {
                    painter.text(
                        rect.left_center() + vec2(6.0, 0.0),
                        Align2::LEFT_CENTER,
                        m,
                        theme::ui_font(12.0),
                        p.text_faint,
                    );
                }
                Row::Schema { name, count, open } => {
                    if resp.hovered() {
                        painter.rect_filled(rect, CornerRadius::same(5), p.hover);
                    }
                    let chev = egui::Rect::from_center_size(rect.left_center() + vec2(9.0, 0.0), vec2(10.0, 10.0));
                    icons::paint(
                        painter,
                        chev,
                        if *open { Icon::ChevronDown } else { Icon::ChevronRight },
                        p.text_muted,
                    );
                    icons::paint(
                        painter,
                        egui::Rect::from_center_size(rect.left_center() + vec2(26.0, 0.0), vec2(14.0, 14.0)),
                        Icon::Schema,
                        p.text_muted,
                    );
                    painter.text(
                        rect.left_center() + vec2(38.0, 0.0),
                        Align2::LEFT_CENTER,
                        *name,
                        theme::bold_font(12.5),
                        p.text,
                    );
                    painter.text(
                        rect.right_center() - vec2(6.0, 0.0),
                        Align2::RIGHT_CENTER,
                        count.to_string(),
                        theme::ui_font(11.0),
                        p.text_faint,
                    );
                    if resp.clicked() {
                        toggle = Some((name.to_string(), *open));
                    }
                }
                Row::Group { schema, kind, count, open, depth } => {
                    if resp.hovered() {
                        painter.rect_filled(rect, CornerRadius::same(5), p.hover);
                    }
                    let x = rect.left() + 4.0 + *depth as f32 * 14.0;
                    let chev = egui::Rect::from_center_size(egui::pos2(x + 5.0, rect.center().y), vec2(10.0, 10.0));
                    icons::paint(
                        painter,
                        chev,
                        if *open { Icon::ChevronDown } else { Icon::ChevronRight },
                        p.text_faint,
                    );
                    painter.text(
                        egui::pos2(x + 15.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        kind.plural().to_uppercase(),
                        theme::bold_font(10.5),
                        p.text_faint,
                    );
                    painter.text(
                        rect.right_center() - vec2(6.0, 0.0),
                        Align2::RIGHT_CENTER,
                        count.to_string(),
                        theme::ui_font(11.0),
                        p.text_faint,
                    );
                    if resp.clicked() {
                        let key = format!("{schema}/{}", kind.plural());
                        toggle = Some((key, *open));
                    }
                }
                Row::Object { obj, depth, highlight, show_schema } => {
                    let selected = active.as_ref() == Some(*obj);
                    if selected {
                        painter.rect_filled(rect, CornerRadius::same(5), p.selection);
                    } else if resp.hovered() {
                        painter.rect_filled(rect, CornerRadius::same(5), p.hover);
                    }
                    let x = rect.left() + 6.0 + *depth as f32 * 14.0;
                    let icon_color = if selected { p.accent_text } else { icons::kind_color(obj.kind, dark) };
                    icons::paint(
                        painter,
                        egui::Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), vec2(14.0, 14.0)),
                        icons::kind_icon(obj.kind),
                        icon_color,
                    );
                    let text_color = if selected { p.accent_text } else { p.text };
                    let mut job = egui::text::LayoutJob::default();
                    if *show_schema {
                        job.append(
                            &format!("{}.", obj.schema),
                            0.0,
                            egui::TextFormat {
                                font_id: theme::ui_font(13.0),
                                color: if selected { p.accent_text } else { p.text_faint },
                                ..Default::default()
                            },
                        );
                    }
                    for (i, ch) in obj.name.chars().enumerate() {
                        let hit = highlight.contains(&i);
                        let fmt = egui::TextFormat {
                            font_id: if hit { theme::bold_font(13.0) } else { theme::ui_font(13.0) },
                            color: if hit && !selected { p.accent } else { text_color },
                            ..Default::default()
                        };
                        job.append(&ch.to_string(), 0.0, fmt);
                    }
                    let galley = ui.painter().layout_job(job);
                    let text_rect = egui::Rect::from_min_max(
                        egui::pos2(x + 20.0, rect.top()),
                        rect.right_bottom() - vec2(4.0, 0.0),
                    );
                    ui.painter().with_clip_rect(text_rect).galley(
                        egui::pos2(text_rect.left(), rect.center().y - galley.size().y / 2.0),
                        galley,
                        text_color,
                    );
                    if resp.clicked() || resp.double_clicked() {
                        actions.push(Action::OpenObject { object: (*obj).clone(), structure: false });
                    }
                    let tip = match &obj.signature {
                        Some(sig) => format!("{}({sig})", obj.qualified_name()),
                        None => obj.qualified_name(),
                    };
                    let resp = resp.on_hover_text_at_pointer(tip);
                    object_menu(&resp, obj, actions, session_dialect_quote(obj));
                }
            }
        }
    });
    if let Some((key, open)) = toggle {
        let exp = &mut session.sidebar.expanded;
        let negated = format!("!{key}");
        if open {
            exp.remove(&key);
            exp.insert(negated);
        } else {
            exp.insert(key);
            exp.remove(&negated);
        }
    }
}

fn session_dialect_quote(obj: &SchemaObject) -> String {
    obj.qualified_name()
}

fn object_menu(resp: &egui::Response, obj: &SchemaObject, actions: &mut Vec<Action>, qualified: String) {
    resp.context_menu(|ui| {
        ui.set_min_width(190.0);
        if obj.kind.is_relation() {
            if ui.button("Open data").clicked() {
                actions.push(Action::OpenObject { object: obj.clone(), structure: false });
                ui.close();
            }
            if ui.button("Open structure").clicked() {
                actions.push(Action::OpenObject { object: obj.clone(), structure: true });
                ui.close();
            }
            if ui.button("New query").clicked() {
                actions.push(Action::NewQuery(Some(format!("SELECT * FROM {qualified} LIMIT 100;"))));
                ui.close();
            }
        }
        if ui.button("Show definition").clicked() {
            actions.push(Action::ShowDefinition(obj.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button("Copy name").clicked() {
            ui.ctx().copy_text(obj.name.clone());
            ui.close();
        }
        if ui.button("Copy qualified name").clicked() {
            ui.ctx().copy_text(qualified.clone());
            ui.close();
        }
        if obj.kind.is_relation() {
            ui.separator();
            if ui.button("Export…").clicked() {
                actions.push(Action::Export(crate::export_dialog::ExportSource::Table {
                    object: obj.clone(),
                    where_clause: None,
                    sort: Vec::new(),
                }));
                ui.close();
            }
            if obj.kind == ObjectKind::Table && ui.button("Import…").clicked() {
                actions.push(Action::Import(obj.clone()));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Refresh").clicked() {
            actions.push(Action::RefreshObjects);
            ui.close();
        }
    });
}

fn ago(t: chrono::DateTime<Utc>) -> String {
    let s = (Utc::now() - t).num_seconds().max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => t.format("%Y-%m-%d").to_string(),
    }
}

fn history_list(ui: &mut Ui, session: &mut Session, history: &History, actions: &mut Vec<Action>) {
    let p = palette(ui);
    let w = ui.available_width();
    widgets::search_field(
        ui,
        &mut session.sidebar.history_filter,
        "Search history…",
        Id::new(("history_search", session.key)),
        w,
    );
    ui.add_space(6.0);
    let entries: Vec<_> = history.search(Some(session.config.id), &session.sidebar.history_filter).take(500).collect();
    if entries.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(widgets::muted(ui, "No queries yet"));
        });
        return;
    }
    ui.spacing_mut().item_spacing.y = 2.0;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 46.0, entries.len(), |ui, range| {
        for e in &entries[range] {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(5), p.hover);
            }
            let first: String = e.sql.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
            let text_rect = rect.shrink2(vec2(6.0, 4.0));
            ui.painter().with_clip_rect(text_rect).text(
                text_rect.left_top(),
                Align2::LEFT_TOP,
                first,
                theme::mono_font(11.5),
                p.text,
            );
            let meta = match (&e.error, e.rows) {
                (Some(err), _) => {
                    (format!("{} · {}", ago(e.executed_at), err.lines().next().unwrap_or_default()), p.danger)
                }
                (None, Some(n)) => (
                    format!("{} · {} ms · {}", ago(e.executed_at), e.duration_ms, widgets::plural(n, "row")),
                    p.text_faint,
                ),
                (None, None) => (format!("{} · {} ms", ago(e.executed_at), e.duration_ms), p.text_faint),
            };
            ui.painter().with_clip_rect(text_rect).text(
                text_rect.left_bottom(),
                Align2::LEFT_BOTTOM,
                meta.0,
                theme::ui_font(11.0),
                meta.1,
            );
            ui.painter().line_segment(
                [rect.left_bottom() + vec2(6.0, 1.0), rect.right_bottom() + vec2(-6.0, 1.0)],
                Stroke::new(1.0, p.border.gamma_multiply(0.6)),
            );
            if resp.clicked() {
                actions.push(Action::NewQuery(Some(e.sql.clone())));
            }
            let sql = e.sql.clone();
            resp.on_hover_text(&e.sql).context_menu(|ui| {
                if ui.button("Open in new query tab").clicked() {
                    actions.push(Action::NewQuery(Some(sql.clone())));
                    ui.close();
                }
                if ui.button("Copy SQL").clicked() {
                    ui.ctx().copy_text(sql.clone());
                    ui.close();
                }
                ui.separator();
                if ui.button("Clear all history").clicked() {
                    actions.push(Action::ClearHistory);
                    ui.close();
                }
            });
        }
    });
    let _ = RichText::new("");
}
