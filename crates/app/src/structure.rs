//! Structure tab: columns, indexes and constraints with staged edits that become previewed
//! `ALTER TABLE` statements.

use std::collections::BTreeSet;

use eframe::egui::{self, Align, Color32, CornerRadius, Key, Layout, RichText, Sense, Stroke, TextEdit, Ui, vec2};
use tableplus_core::Dialect;
use tableplus_core::alter::{ColumnDef, StructureChange, alter_statements};
use tableplus_core::model::{ColumnInfo, ConstraintKind, SchemaObject, TableStructure};

use crate::app::{Action, ToastKind, shortcut_label};
use crate::icons::{self, Icon};
use crate::task::{Task, poll_slot};
use crate::theme::{self, palette};
use crate::widgets;
use crate::workspace::TabCtx;

struct ColumnDraft {
    original: Option<String>,
    def: ColumnDef,
    dropped: bool,
}

#[derive(Default, Clone)]
struct IndexDraft {
    name: String,
    columns: Vec<String>,
    unique: bool,
}

#[derive(Default)]
pub struct StructureEditor {
    base: Option<TableStructure>,
    columns: Vec<ColumnDraft>,
    new_indexes: Vec<IndexDraft>,
    dropped_indexes: BTreeSet<String>,
    dropped_constraints: Vec<(String, ConstraintKind)>,
    index_form: Option<IndexDraft>,
    preview: Option<Result<Vec<String>, String>>,
    apply_task: Option<Task<u64>>,
    apply_error: Option<String>,
    applied: bool,
}

fn column_changed(orig: &ColumnInfo, def: &ColumnDef) -> bool {
    let norm = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    orig.name != def.name
        || orig.data_type.trim() != def.data_type.trim()
        || orig.nullable != def.nullable
        || norm(&orig.default) != norm(&def.default)
        || norm(&orig.comment) != norm(&def.comment)
}

const W_NAME: f32 = 170.0;
const W_TYPE: f32 = 190.0;
const W_NULL: f32 = 64.0;
const W_DEFAULT: f32 = 190.0;
const W_EXTRA: f32 = 150.0;
const W_COMMENT: f32 = 200.0;

impl StructureEditor {
    pub fn reset(&mut self, st: &TableStructure) {
        self.columns = st
            .columns
            .iter()
            .map(|c| ColumnDraft { original: Some(c.name.clone()), def: ColumnDef::from(c), dropped: false })
            .collect();
        self.new_indexes.clear();
        self.dropped_indexes.clear();
        self.dropped_constraints.clear();
        self.index_form = None;
        self.base = Some(st.clone());
    }

    pub fn changes(&self) -> Vec<StructureChange> {
        let Some(base) = &self.base else { return Vec::new() };
        let mut out: Vec<StructureChange> = self
            .dropped_constraints
            .iter()
            .map(|(name, kind)| StructureChange::DropConstraint { name: name.clone(), kind: *kind })
            .collect();
        out.extend(self.dropped_indexes.iter().map(|n| StructureChange::DropIndex(n.clone())));
        for c in &self.columns {
            if let (Some(o), false) = (&c.original, c.dropped)
                && let Some(orig) = base.column(o)
                && column_changed(orig, &c.def)
            {
                out.push(StructureChange::ModifyColumn { original: o.clone(), new: c.def.clone() });
            }
        }
        out.extend(
            self.columns
                .iter()
                .filter(|c| c.original.is_none() && !c.dropped)
                .map(|c| StructureChange::AddColumn(c.def.clone())),
        );
        out.extend(
            self.columns
                .iter()
                .filter(|c| c.dropped)
                .filter_map(|c| c.original.clone())
                .map(StructureChange::DropColumn),
        );
        out.extend(self.new_indexes.iter().map(|i| StructureChange::AddIndex {
            name: i.name.clone(),
            columns: i.columns.clone(),
            unique: i.unique,
        }));
        out
    }

    pub fn has_changes(&self) -> bool {
        !self.changes().is_empty()
    }

    pub fn take_applied(&mut self) -> bool {
        std::mem::take(&mut self.applied)
    }

    pub fn request_preview(&mut self, st: Option<&TableStructure>, dialect: Dialect, object: &SchemaObject) {
        let Some(st) = st else { return };
        let changes = self.changes();
        if changes.is_empty() {
            return;
        }
        self.apply_error = None;
        self.preview = Some(alter_statements(dialect, &object.schema, &object.name, st, &changes));
    }

    pub fn poll(&mut self, ctx: &mut TabCtx, object: &SchemaObject) {
        if let Some(r) = poll_slot(&mut self.apply_task) {
            match r {
                Ok(_) => {
                    ctx.actions
                        .push(Action::Toast(ToastKind::Success, format!("Structure of {} updated", object.name)));
                    self.preview = None;
                    self.applied = true;
                    self.apply_error = None;
                }
                Err(e) => self.apply_error = Some(format!("{e:#}")),
            }
        }
    }

    pub fn bottom_actions(
        &mut self,
        ui: &mut Ui,
        st: Option<&TableStructure>,
        dialect: Dialect,
        object: &SchemaObject,
    ) {
        let n = self.changes().len();
        if n == 0 {
            ui.label(widgets::muted(ui, format!("{} columns", st.map_or(0, |s| s.columns.len()))));
            return;
        }
        if widgets::primary_button(ui, &format!("Review {n} change{}…", if n == 1 { "" } else { "s" }))
            .on_hover_text(shortcut_label("S"))
            .clicked()
        {
            self.request_preview(st, dialect, object);
        }
        if widgets::button(ui, "Discard").clicked()
            && let Some(base) = self.base.clone()
        {
            self.reset(&base);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, ctx: &mut TabCtx, st: &TableStructure, object: &SchemaObject) {
        if self.base.as_ref() != Some(st) && !self.has_changes() {
            self.reset(st);
        }
        let p = palette(ui);
        let dialect = ctx.dialect;
        let editable = object.kind == tableplus_core::model::ObjectKind::Table;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(12.0);
            egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 0)).show(ui, |ui| {
                // Columns.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Columns").font(theme::bold_font(14.0)));
                    if let Some(c) = &st.comment {
                        ui.label(widgets::muted(ui, format!("— {c}")));
                    }
                });
                ui.add_space(4.0);
                header_row(
                    ui,
                    &[
                        ("", 22.0),
                        ("Name", W_NAME),
                        ("Type", W_TYPE),
                        ("Nullable", W_NULL),
                        ("Default", W_DEFAULT),
                        ("Extra", W_EXTRA),
                        ("Comment", W_COMMENT),
                    ],
                );
                let base = self.base.clone();
                let mut remove_new = None;
                for (i, c) in self.columns.iter_mut().enumerate() {
                    let orig = c.original.as_ref().and_then(|o| base.as_ref().and_then(|b| b.column(o)));
                    let fill = if c.dropped {
                        p.deleted_bg
                    } else if c.original.is_none() {
                        p.inserted_bg
                    } else if orig.is_some_and(|o| column_changed(o, &c.def)) {
                        p.modified_bg
                    } else if i % 2 == 1 {
                        p.row_alt
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::new()
                        .fill(fill)
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(4, 2))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(editable && !c.dropped, |ui| {
                                    fixed(ui, 14.0, |ui| {
                                        let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                                        if c.def.primary_key {
                                            icons::paint(ui.painter(), r, Icon::Key, p.warning);
                                        }
                                    });
                                    fixed(ui, W_NAME - 8.0, |ui| {
                                        ui.add(
                                            TextEdit::singleline(&mut c.def.name)
                                                .font(theme::mono_font(12.5))
                                                .desired_width(W_NAME - 8.0),
                                        );
                                    });
                                    fixed(ui, W_TYPE - 8.0, |ui| {
                                        ui.spacing_mut().item_spacing.x = 2.0;
                                        ui.add(
                                            TextEdit::singleline(&mut c.def.data_type)
                                                .font(theme::mono_font(12.5))
                                                .desired_width(W_TYPE - 34.0),
                                        );
                                        let r = widgets::icon_button_sized(
                                            ui,
                                            Icon::ChevronDown,
                                            "Common types",
                                            vec2(22.0, 22.0),
                                            true,
                                        );
                                        egui::Popup::menu(&r).show(|ui| {
                                            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                                                for t in dialect.common_types() {
                                                    if ui.button(*t).clicked() {
                                                        c.def.data_type = (*t).to_string();
                                                        ui.close();
                                                    }
                                                }
                                            });
                                        });
                                    });
                                    fixed(ui, W_NULL - 8.0, |ui| {
                                        ui.checkbox(&mut c.def.nullable, "");
                                    });
                                    fixed(ui, W_DEFAULT - 8.0, |ui| {
                                        let mut default = c.def.default.clone().unwrap_or_default();
                                        let hint = RichText::new("NULL").color(p.text_faint);
                                        if ui
                                            .add(
                                                TextEdit::singleline(&mut default)
                                                    .hint_text(hint)
                                                    .font(theme::mono_font(12.5))
                                                    .desired_width(W_DEFAULT - 8.0),
                                            )
                                            .changed()
                                        {
                                            c.def.default = (!default.trim().is_empty()).then_some(default);
                                        }
                                    });
                                    fixed(ui, W_EXTRA - 8.0, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(c.def.extra.clone().unwrap_or_default())
                                                    .color(p.text_muted)
                                                    .size(12.0),
                                            )
                                            .truncate(),
                                        );
                                    });
                                    fixed(ui, W_COMMENT - 8.0, |ui| {
                                        let mut comment = c.def.comment.clone().unwrap_or_default();
                                        if dialect != Dialect::Sqlite
                                            && ui
                                                .add(TextEdit::singleline(&mut comment).desired_width(W_COMMENT - 8.0))
                                                .changed()
                                        {
                                            c.def.comment = (!comment.is_empty()).then_some(comment);
                                        }
                                    });
                                });
                                if editable {
                                    let tip = if c.dropped { "Keep column" } else { "Drop column" };
                                    if widgets::icon_button_sized(
                                        ui,
                                        if c.dropped { Icon::Refresh } else { Icon::Trash },
                                        tip,
                                        vec2(24.0, 22.0),
                                        true,
                                    )
                                    .clicked()
                                    {
                                        if c.original.is_none() {
                                            remove_new = Some(i);
                                        } else {
                                            c.dropped = !c.dropped;
                                        }
                                    }
                                }
                            });
                        });
                }
                if let Some(i) = remove_new {
                    self.columns.remove(i);
                }
                if editable {
                    ui.add_space(4.0);
                    if widgets::icon_text_button(ui, Icon::Plus, "Column").clicked() {
                        let ty = dialect
                            .common_types()
                            .iter()
                            .find(|t| t.to_lowercase().contains("text"))
                            .copied()
                            .unwrap_or("text");
                        self.columns.push(ColumnDraft {
                            original: None,
                            def: ColumnDef {
                                name: format!("column_{}", self.columns.len() + 1),
                                data_type: ty.into(),
                                nullable: true,
                                ..Default::default()
                            },
                            dropped: false,
                        });
                    }
                }

                // Indexes.
                ui.add_space(18.0);
                ui.label(RichText::new("Indexes").font(theme::bold_font(14.0)));
                ui.add_space(4.0);
                header_row(
                    ui,
                    &[("Name", 240.0), ("Columns", 260.0), ("Unique", 70.0), ("Method", 90.0), ("Definition", 380.0)],
                );
                for (i, idx) in st.indexes.iter().enumerate() {
                    let dropped = self.dropped_indexes.contains(&idx.name);
                    let fill = if dropped {
                        p.deleted_bg
                    } else if i % 2 == 1 {
                        p.row_alt
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::new()
                        .fill(fill)
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(4, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                cell(ui, &idx.name, 240.0, true);
                                cell(ui, &idx.columns.join(", "), 260.0, true);
                                cell(
                                    ui,
                                    if idx.primary {
                                        "PRIMARY"
                                    } else if idx.unique {
                                        "YES"
                                    } else {
                                        ""
                                    },
                                    70.0,
                                    false,
                                );
                                cell(ui, idx.method.as_deref().unwrap_or(""), 90.0, false);
                                cell(ui, idx.definition.as_deref().unwrap_or(""), 380.0, true);
                                let constraint_backed = st.constraints.iter().any(|c| c.name == idx.name);
                                if editable && !idx.primary && !constraint_backed {
                                    let icon = if dropped { Icon::Refresh } else { Icon::Trash };
                                    if widgets::icon_button_sized(
                                        ui,
                                        icon,
                                        if dropped { "Keep index" } else { "Drop index" },
                                        vec2(24.0, 22.0),
                                        true,
                                    )
                                    .clicked()
                                    {
                                        if dropped {
                                            self.dropped_indexes.remove(&idx.name);
                                        } else {
                                            self.dropped_indexes.insert(idx.name.clone());
                                        }
                                    }
                                }
                            });
                        });
                }
                let mut remove_idx = None;
                for (i, idx) in self.new_indexes.iter().enumerate() {
                    egui::Frame::new()
                        .fill(p.inserted_bg)
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(4, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                cell(ui, &idx.name, 240.0, true);
                                cell(ui, &idx.columns.join(", "), 260.0, true);
                                cell(ui, if idx.unique { "YES" } else { "" }, 70.0, false);
                                cell(ui, "", 90.0, false);
                                cell(ui, "(new)", 380.0, false);
                                if widgets::icon_button_sized(ui, Icon::Trash, "Remove", vec2(24.0, 22.0), true)
                                    .clicked()
                                {
                                    remove_idx = Some(i);
                                }
                            });
                        });
                }
                if let Some(i) = remove_idx {
                    self.new_indexes.remove(i);
                }
                if editable {
                    ui.add_space(4.0);
                    match &mut self.index_form {
                        None => {
                            if widgets::icon_text_button(ui, Icon::Plus, "Index").clicked() {
                                self.index_form = Some(IndexDraft {
                                    name: format!(
                                        "{}_idx_{}",
                                        object.name,
                                        st.indexes.len() + self.new_indexes.len() + 1
                                    ),
                                    ..Default::default()
                                });
                            }
                        }
                        Some(form) => {
                            let mut done = None;
                            egui::Frame::new()
                                .stroke(Stroke::new(1.0, p.border))
                                .corner_radius(CornerRadius::same(6))
                                .inner_margin(egui::Margin::same(8))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label("Name");
                                        ui.add(TextEdit::singleline(&mut form.name).desired_width(200.0));
                                        ui.checkbox(&mut form.unique, "Unique");
                                    });
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("Columns");
                                        for c in &st.columns {
                                            let pos = form.columns.iter().position(|x| x == &c.name);
                                            let label = match pos {
                                                Some(n) => format!("{} ({})", c.name, n + 1),
                                                None => c.name.clone(),
                                            };
                                            if ui.selectable_label(pos.is_some(), label).clicked() {
                                                match pos {
                                                    Some(n) => {
                                                        form.columns.remove(n);
                                                    }
                                                    None => form.columns.push(c.name.clone()),
                                                }
                                            }
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        let ok = !form.name.trim().is_empty() && !form.columns.is_empty();
                                        if widgets::primary_button_enabled(ui, "Add index", ok).clicked() {
                                            done = Some(true);
                                        }
                                        if widgets::button(ui, "Cancel").clicked() {
                                            done = Some(false);
                                        }
                                    });
                                });
                            match done {
                                Some(true) => {
                                    let f = self.index_form.take().expect("form");
                                    self.new_indexes.push(f);
                                }
                                Some(false) => self.index_form = None,
                                None => {}
                            }
                        }
                    }
                }

                // Constraints.
                ui.add_space(18.0);
                ui.label(RichText::new("Constraints").font(theme::bold_font(14.0)));
                ui.add_space(4.0);
                header_row(ui, &[("Name", 240.0), ("Type", 110.0), ("Columns", 200.0), ("Definition", 470.0)]);
                if st.constraints.is_empty() {
                    ui.label(widgets::muted(ui, "No constraints"));
                }
                for (i, c) in st.constraints.iter().enumerate() {
                    let dropped = self.dropped_constraints.iter().any(|(n, _)| n == &c.name);
                    let fill = if dropped {
                        p.deleted_bg
                    } else if i % 2 == 1 {
                        p.row_alt
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::new()
                        .fill(fill)
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(4, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                cell(ui, &c.name, 240.0, true);
                                let color = match c.kind {
                                    ConstraintKind::PrimaryKey => p.warning,
                                    ConstraintKind::ForeignKey => p.accent,
                                    ConstraintKind::Unique => p.success,
                                    ConstraintKind::Check => p.syntax_keyword,
                                };
                                fixed(ui, 102.0, |ui| {
                                    widgets::pill(ui, c.kind.label(), color);
                                });
                                cell(ui, &c.columns.join(", "), 200.0, true);
                                cell(ui, &c.definition, 470.0, true);
                                if editable && dialect != Dialect::Sqlite {
                                    let icon = if dropped { Icon::Refresh } else { Icon::Trash };
                                    if widgets::icon_button_sized(
                                        ui,
                                        icon,
                                        if dropped { "Keep constraint" } else { "Drop constraint" },
                                        vec2(24.0, 22.0),
                                        true,
                                    )
                                    .clicked()
                                    {
                                        if dropped {
                                            self.dropped_constraints.retain(|(n, _)| n != &c.name);
                                        } else {
                                            self.dropped_constraints.push((c.name.clone(), c.kind));
                                        }
                                    }
                                }
                            });
                        });
                }
                ui.add_space(24.0);
            });
        });

        // Preview / apply modal.
        if let Some(preview) = self.preview.clone() {
            let mut close = false;
            let mut apply = None;
            crate::widgets::modal(ui.ctx(), ("alter_preview", object)).show(ui.ctx(), |ui| {
                ui.set_width(660.0);
                ui.heading("Review structure changes");
                match &preview {
                    Ok(stmts) => {
                        ui.label(widgets::muted(
                            ui,
                            format!("{} statement(s) will be executed on {}.", stmts.len(), object.qualified_name()),
                        ));
                        if dialect == Dialect::MySql {
                            ui.label(
                                RichText::new("MySQL commits DDL implicitly; statements run one by one.")
                                    .color(p.warning)
                                    .size(12.0),
                            );
                        }
                        ui.add_space(6.0);
                        let mut text = stmts.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n");
                        egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                            ui.add(TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY));
                        });
                        if let Some(e) = &self.apply_error {
                            ui.colored_label(p.danger, e);
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if widgets::button(ui, "Copy SQL").clicked() {
                                ui.ctx().copy_text(text.clone());
                            }
                            if widgets::button(ui, "Open in SQL editor").clicked() {
                                ctx.actions.push(Action::NewQuery(Some(text.clone())));
                                close = true;
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::primary_button_enabled(ui, "Apply", self.apply_task.is_none()).clicked() {
                                    apply = Some(stmts.clone());
                                }
                                if widgets::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                                    close = true;
                                }
                                if self.apply_task.is_some() {
                                    ui.add(egui::Spinner::new());
                                }
                            });
                        });
                    }
                    Err(e) => {
                        ui.colored_label(p.danger, e);
                        if widgets::button(ui, "Close").clicked() {
                            close = true;
                        }
                    }
                }
            });
            if let Some(stmts) = apply {
                let conn = ctx.conn.clone();
                let mysql = dialect == Dialect::MySql;
                ctx.actions.push(Action::RecordHistory(tableplus_core::store::HistoryEntry {
                    connection_id: ctx.connection.id,
                    connection_name: ctx.connection.display_name(),
                    sql: stmts.join(";\n") + ";",
                    executed_at: chrono::Utc::now(),
                    duration_ms: 0,
                    rows: None,
                    error: None,
                }));
                self.apply_task = Some(ctx.spawner.spawn(async move {
                    if mysql {
                        for s in &stmts {
                            conn.query(s, None).await?;
                        }
                        Ok(0)
                    } else {
                        conn.execute_in_transaction(&stmts).await
                    }
                }));
            }
            if close {
                self.preview = None;
                self.apply_error = None;
            }
        }
    }
}

/// Runs `add` in a cell of exactly `width` points (so rows line up with the header).
fn fixed<R>(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(vec2(width, 24.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(width);
        ui.set_max_width(width);
        add(ui)
    })
    .inner
}

fn header_row(ui: &mut Ui, cols: &[(&str, f32)]) {
    let p = palette(ui);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        for (label, w) in cols {
            fixed(ui, *w - 8.0, |ui| {
                ui.label(RichText::new(*label).font(theme::bold_font(11.5)).color(p.text_muted));
            });
        }
    });
}

fn cell(ui: &mut Ui, text: &str, width: f32, mono: bool) {
    fixed(ui, width - 8.0, |ui| {
        let rt = if mono { RichText::new(text).font(theme::mono_font(12.0)) } else { RichText::new(text).size(12.5) };
        ui.add(egui::Label::new(rt).truncate());
    });
}
