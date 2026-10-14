//! A table/view tab: paginated data grid with filters, sorting and staged edits, plus the
//! structure editor.

use std::time::Duration;

use chrono::Utc;
use eframe::egui::{self, Align, Key, Layout, RichText, Stroke, TextEdit, Ui, vec2};
use tableplus_core::Dialect;
use tableplus_core::driver::DynConnection;
use tableplus_core::export::{ExportFormat, ExportOptions, export_to_vec};
use tableplus_core::model::{ResultSet, Row, SchemaObject, StatementResult, TableStructure};
use tableplus_core::sqlgen::{self, Filter, FilterOp, SortKey};
use tableplus_core::store::HistoryEntry;

use crate::app::{Action, ToastKind, shortcut_label};
use crate::export_dialog::ExportSource;
use crate::grid::{self, CopyFormat, GridEvent, GridState};
use crate::icons::Icon;
use crate::staged::{StagedChanges, TableData};
use crate::structure::StructureEditor;
use crate::task::{Spawner, Task, poll_slot};
use crate::theme::palette;
use crate::widgets::{self, fmt_count, fmt_duration};
use crate::workspace::TabCtx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableView {
    Data,
    Structure,
}

struct PageResult {
    rs: ResultSet,
    sql: String,
    elapsed: Duration,
}

pub struct TableTab {
    pub object: SchemaObject,
    conn: DynConnection,
    pub view: TableView,
    pub structure: Option<TableStructure>,
    structure_task: Option<Task<TableStructure>>,
    structure_error: Option<String>,
    pub editor: StructureEditor,
    page_size: usize,
    page: u64,
    result: Option<ResultSet>,
    load_task: Option<Task<PageResult>>,
    load_error: Option<String>,
    last_sql: String,
    elapsed: Option<Duration>,
    total: Option<u64>,
    count_task: Option<Task<u64>>,
    filters: Vec<Filter>,
    applied_filters: Vec<Filter>,
    show_filters: bool,
    focus_filter: bool,
    sort: Vec<SortKey>,
    grid: GridState,
    staged: StagedChanges,
    commit_task: Option<Task<u64>>,
    preview: Option<Vec<String>>,
    commit_error: Option<String>,
    initial_load: bool,
}

impl TableTab {
    pub fn new(object: SchemaObject, page_size: usize, conn: DynConnection, spawner: &Spawner) -> Self {
        let mut t = Self {
            object,
            conn,
            view: TableView::Data,
            structure: None,
            structure_task: None,
            structure_error: None,
            editor: StructureEditor::default(),
            page_size,
            page: 0,
            result: None,
            load_task: None,
            load_error: None,
            last_sql: String::new(),
            elapsed: None,
            total: None,
            count_task: None,
            filters: vec![Filter::default()],
            applied_filters: Vec::new(),
            show_filters: false,
            focus_filter: false,
            sort: Vec::new(),
            grid: GridState::default(),
            staged: StagedChanges::default(),
            commit_task: None,
            preview: None,
            commit_error: None,
            initial_load: false,
        };
        // The first page waits for the structure so it can be ordered by the primary key.
        t.load_structure(spawner);
        t.initial_load = true;
        t
    }

    pub fn show_structure(&mut self) {
        self.view = TableView::Structure;
    }

    pub fn has_pending_changes(&self) -> bool {
        !self.staged.is_empty() || self.editor.has_changes()
    }

    fn dialect(&self) -> Dialect {
        self.conn.dialect()
    }

    pub fn load_structure(&mut self, spawner: &Spawner) {
        let conn = self.conn.clone();
        let (s, t) = (self.object.schema.clone(), self.object.name.clone());
        self.structure_task = Some(spawner.spawn(async move { conn.table_structure(&s, &t).await }));
    }

    fn column_names(&self) -> Vec<String> {
        if let Some(st) = &self.structure {
            return st.columns.iter().map(|c| c.name.clone()).collect();
        }
        self.result.as_ref().map(|r| r.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default()
    }

    /// The user's sort followed by the primary key, so paging and post-commit reloads are
    /// stable even when the sort column has duplicates.
    fn effective_sort(&self) -> Vec<SortKey> {
        let pk = self.structure.as_ref().map(|st| st.primary_key()).unwrap_or_default();
        sqlgen::with_tiebreaker(&self.sort, &pk)
    }

    fn where_clause(&self) -> Option<String> {
        sqlgen::where_clause(self.dialect(), &self.applied_filters, &self.column_names())
    }

    /// Reloads the current page (and the row count when `recount`).
    pub fn reload(&mut self, spawner: &Spawner, recount: bool) {
        let d = self.dialect();
        let w = self.where_clause();
        let sql = sqlgen::select_page_sql(
            d,
            &self.object.schema,
            &self.object.name,
            w.as_deref(),
            &self.effective_sort(),
            Some(self.page_size as u64),
            self.page * self.page_size as u64,
        );
        let conn = self.conn.clone();
        let q = sql.clone();
        self.load_task = Some(spawner.spawn(async move {
            let started = std::time::Instant::now();
            match conn.query(&q, None).await? {
                StatementResult::Rows(rs) => Ok(PageResult { rs, sql: q, elapsed: started.elapsed() }),
                StatementResult::Affected(_) => anyhow::bail!("query returned no rows"),
            }
        }));
        if recount {
            let conn = self.conn.clone();
            let csql = sqlgen::count_sql(d, &self.object.schema, &self.object.name, w.as_deref());
            self.total = None;
            self.count_task = Some(spawner.spawn(async move {
                let v = tableplus_core::driver::scalar(conn.as_ref(), &csql).await?;
                Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
            }));
        }
    }

    fn apply_filters(&mut self, spawner: &Spawner) {
        self.applied_filters = self.filters.clone();
        self.page = 0;
        self.staged.clear();
        self.reload(spawner, true);
    }

    pub fn poll(&mut self, ctx: &mut TabCtx) {
        if let Some(r) = poll_slot(&mut self.structure_task) {
            match r {
                Ok(st) => {
                    self.editor.reset(&st);
                    self.structure = Some(st);
                    self.structure_error = None;
                }
                Err(e) => self.structure_error = Some(format!("{e:#}")),
            }
            if std::mem::take(&mut self.initial_load) {
                self.reload(ctx.spawner, true);
            }
        }
        if let Some(r) = poll_slot(&mut self.load_task) {
            match r {
                Ok(page) => {
                    let same_columns = self.result.as_ref().is_some_and(|old| old.columns == page.rs.columns);
                    if same_columns {
                        self.grid.remeasure();
                    } else {
                        self.grid.reset();
                    }
                    self.result = Some(page.rs);
                    self.last_sql = page.sql;
                    self.elapsed = Some(page.elapsed);
                    self.load_error = None;
                }
                Err(e) => self.load_error = Some(format!("{e:#}")),
            }
        }
        if let Some(r) = poll_slot(&mut self.count_task) {
            self.total = r.ok();
        }
        if let Some(r) = poll_slot(&mut self.commit_task) {
            match r {
                Ok(n) => {
                    ctx.actions.push(Action::Toast(
                        ToastKind::Success,
                        format!("Committed changes ({n} row{} affected)", if n == 1 { "" } else { "s" }),
                    ));
                    self.staged.clear();
                    self.commit_error = None;
                    self.preview = None;
                    self.reload(ctx.spawner, true);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    ctx.actions.push(Action::Toast(
                        ToastKind::Error,
                        format!("Commit failed: {}", msg.lines().next().unwrap_or_default()),
                    ));
                    self.commit_error = Some(msg);
                }
            }
        }
        self.editor.poll(ctx, &self.object);
        if self.editor.take_applied() {
            self.load_structure(ctx.spawner);
            self.reload(ctx.spawner, true);
            ctx.actions.push(Action::InvalidateStructure {
                schema: self.object.schema.clone(),
                table: self.object.name.clone(),
            });
        }
    }

    fn pending_statements(&self) -> Vec<String> {
        match (&self.structure, &self.result) {
            (Some(st), Some(rs)) => {
                self.staged.statements(self.dialect(), &self.object.schema, &self.object.name, &st.columns, &rs.rows)
            }
            _ => Vec::new(),
        }
    }

    fn commit(&mut self, ctx: &mut TabCtx) {
        let statements = self.pending_statements();
        if statements.is_empty() {
            return;
        }
        let conn = self.conn.clone();
        let stmts = statements.clone();
        ctx.actions.push(Action::RecordHistory(HistoryEntry {
            connection_id: ctx.connection.id,
            connection_name: ctx.connection.display_name(),
            sql: statements.join(";\n") + ";",
            executed_at: Utc::now(),
            duration_ms: 0,
            rows: Some(self.staged.count() as u64),
            error: None,
        }));
        self.commit_task = Some(ctx.spawner.spawn(async move { conn.execute_in_transaction(&stmts).await }));
    }

    fn editable(&self) -> bool {
        self.object.kind == tableplus_core::model::ObjectKind::Table && self.structure.is_some()
    }

    fn handle_grid_events(&mut self, events: Vec<GridEvent>, ctx: &mut TabCtx) {
        let Some(rs) = &self.result else { return };
        let ncols = rs.columns.len();
        for ev in events {
            match ev {
                GridEvent::Edit { row, col, value } => {
                    if self.editable() {
                        self.staged.set(&rs.rows, row, col, value);
                    }
                }
                GridEvent::Sort { col, add } => {
                    // Click cycles ascending → descending → unsorted; shift-click adds a key.
                    let name = rs.columns[col].name.clone();
                    if !add {
                        self.sort.retain(|s| s.column == name);
                    }
                    match self.sort.iter().position(|s| s.column == name) {
                        None => self.sort.push(SortKey { column: name, descending: false }),
                        Some(i) if !self.sort[i].descending => self.sort[i].descending = true,
                        Some(i) => {
                            self.sort.remove(i);
                        }
                    }
                    self.staged.clear();
                    self.reload(ctx.spawner, false);
                    return;
                }
                GridEvent::DeleteRows(rows) => {
                    if self.editable() {
                        self.staged.delete(rs.rows.len(), &rows);
                        self.grid.selected.clear();
                    }
                }
                GridEvent::UndeleteRows(rows) => self.staged.undelete(&rows),
                GridEvent::DuplicateRow(row) => {
                    if let Some(st) = &self.structure {
                        self.staged.duplicate(&rs.rows, &st.columns, row);
                        let new_row = rs.rows.len() + self.staged.inserted.len() - 1;
                        self.grid.select_row(new_row, 0);
                    }
                }
                GridEvent::AddRow => {
                    if self.editable() {
                        self.staged.add_row(ncols);
                        let new_row = rs.rows.len() + self.staged.inserted.len() - 1;
                        self.grid.select_row(new_row, 0);
                    }
                }
                GridEvent::FilterByValue { col, value } => {
                    let column = Some(rs.columns[col].name.clone());
                    self.filters = vec![match value {
                        Some(v) => Filter { column, op: FilterOp::Equal, value: v, ..Default::default() },
                        None => Filter { column, op: FilterOp::IsNull, ..Default::default() },
                    }];
                    self.show_filters = true;
                    self.apply_filters(ctx.spawner);
                    return;
                }
                GridEvent::CopyAs { rows, format } => {
                    let text = copy_rows(rs, &rows, format, self.dialect(), &self.object);
                    ctx.actions.push(Action::Toast(
                        ToastKind::Info,
                        format!("Copied {} row{}", rows.len(), if rows.len() == 1 { "" } else { "s" }),
                    ));
                    ctx.egui.copy_text(text);
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let sc = *ctx.shortcuts;
        if sc.refresh {
            if self.view == TableView::Structure {
                self.load_structure(ctx.spawner);
            } else {
                self.reload(ctx.spawner, true);
            }
        }
        if sc.find && self.view == TableView::Data {
            self.show_filters = !self.show_filters || !self.focus_filter;
            self.focus_filter = self.show_filters;
        }
        if sc.commit {
            if self.view == TableView::Data && !self.staged.is_empty() {
                self.commit(ctx);
            } else if self.view == TableView::Structure {
                self.editor.request_preview(self.structure.as_ref(), self.dialect(), &self.object);
            }
        }

        egui::Panel::bottom(egui::Id::new(("table_bottom", &self.object)))
            .exact_size(36.0)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.bottom_bar(ui, ctx);
            });

        match self.view {
            TableView::Structure => {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                    let st = self.structure.clone();
                    match (&st, &self.structure_error) {
                        (Some(st), _) => self.editor.ui(ui, ctx, st, &self.object),
                        (None, Some(e)) => error_box(ui, e),
                        (None, None) => loading(ui, "Loading structure…"),
                    }
                });
            }
            TableView::Data => {
                if self.show_filters {
                    egui::Panel::top(egui::Id::new(("filters", &self.object))).frame(egui::Frame::NONE).show(
                        ui,
                        |ui| {
                            self.filter_bar(ui, ctx);
                        },
                    );
                }
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                    if let Some(e) = &self.load_error {
                        error_box(ui, e);
                        return;
                    }
                    let Some(rs) = &self.result else {
                        loading(ui, "Loading rows…");
                        return;
                    };
                    let keys: Vec<bool> = rs
                        .columns
                        .iter()
                        .map(|c| {
                            self.structure.as_ref().and_then(|s| s.column(&c.name)).is_some_and(|ci| ci.primary_key)
                        })
                        .collect();
                    let editable = self.editable();
                    let data = TableData {
                        columns: &rs.columns,
                        rows: &rs.rows,
                        staged: &self.staged,
                        sort: &self.sort,
                        keys: &keys,
                        editable,
                        first_row_number: self.page * self.page_size as u64 + 1,
                    };
                    let id = egui::Id::new(("grid", &self.object));
                    let events = grid::show(ui, id, &mut self.grid, &data);
                    self.handle_grid_events(events, ctx);
                });
            }
        }

        if let Some(stmts) = self.preview.clone() {
            let dialect = ctx.dialect;
            let mut close = false;
            let mut run = false;
            crate::widgets::modal(ui.ctx(), ("commit_preview", &self.object)).show(ui.ctx(), |ui| {
                ui.set_width(640.0);
                ui.heading("Review changes");
                ui.label(widgets::muted(
                    ui,
                    format!("{} will run in one transaction.", widgets::plural(stmts.len() as u64, "statement")),
                ));
                ui.add_space(6.0);
                let text = stmts.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n");
                widgets::sql_view(ui, &text, dialect, 320.0);
                if let Some(e) = &self.commit_error {
                    ui.colored_label(palette(ui).danger, e);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if widgets::button(ui, "Copy SQL").clicked() {
                        ui.ctx().copy_text(text.clone());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::primary_button_enabled(ui, "Commit", self.commit_task.is_none()).clicked() {
                            run = true;
                        }
                        if widgets::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if self.commit_task.is_some() {
                            ui.add(egui::Spinner::new());
                        }
                    });
                });
            });
            if run {
                self.commit(ctx);
            }
            if close {
                self.preview = None;
                self.commit_error = None;
            }
        }
    }

    fn filter_bar(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let p = palette(ui);
        let rect = ui.max_rect();
        let columns = self.column_names();
        let mut apply = false;
        let mut remove = None;
        let mut add_after = None;
        egui::Frame::new().fill(p.header_bg).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
            ui.set_width(rect.width() - 20.0);
            for (i, f) in self.filters.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut f.enabled, "");
                    let col_label = f.column.clone().unwrap_or_else(|| "Any column".into());
                    egui::ComboBox::from_id_salt(("fcol", i)).selected_text(col_label).width(150.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut f.column, None, "Any column");
                        for c in &columns {
                            ui.selectable_value(&mut f.column, Some(c.clone()), c);
                        }
                    });
                    egui::ComboBox::from_id_salt(("fop", i)).selected_text(f.op.label()).width(110.0).show_ui(
                        ui,
                        |ui| {
                            for op in FilterOp::ALL {
                                ui.selectable_value(&mut f.op, op, op.label());
                            }
                        },
                    );
                    if f.op.needs_value() {
                        let hint = match f.op {
                            FilterOp::In | FilterOp::NotIn => "a, b, c",
                            FilterOp::Raw => "price > 10 AND stock < 5",
                            _ => "Value",
                        };
                        let w = if f.op.needs_second_value() {
                            140.0
                        } else {
                            (ui.available_width() - 80.0).clamp(120.0, 420.0)
                        };
                        let r = ui.add(TextEdit::singleline(&mut f.value).hint_text(hint).desired_width(w));
                        if i == 0 && std::mem::take(&mut self.focus_filter) {
                            r.request_focus();
                        }
                        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            apply = true;
                        }
                        if f.op.needs_second_value() {
                            ui.label("and");
                            let r2 = ui.add(TextEdit::singleline(&mut f.value2).desired_width(140.0));
                            if r2.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                apply = true;
                            }
                        }
                    }
                    if widgets::icon_button(ui, Icon::Minus, "Remove filter").clicked() {
                        remove = Some(i);
                    }
                    if widgets::icon_button(ui, Icon::Plus, "Add filter").clicked() {
                        add_after = Some(i);
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                if widgets::primary_button(ui, "Apply").clicked() {
                    apply = true;
                }
                if widgets::button(ui, "Clear").clicked() {
                    self.filters = vec![Filter::default()];
                    apply = true;
                }
                if let Some(w) = sqlgen::where_clause(self.dialect(), &self.filters, &columns) {
                    let shown: String = w.chars().take(160).collect();
                    ui.label(
                        RichText::new(format!("WHERE {shown}")).font(crate::theme::mono_font(11.0)).color(p.text_faint),
                    );
                }
            });
        });
        ui.painter()
            .line_segment([ui.min_rect().left_bottom(), ui.min_rect().right_bottom()], Stroke::new(1.0, p.border));
        if let Some(i) = remove {
            self.filters.remove(i);
            if self.filters.is_empty() {
                self.filters.push(Filter::default());
                self.show_filters = false;
            }
            apply = true;
        }
        if let Some(i) = add_after {
            self.filters.insert(i + 1, Filter::default());
        }
        if apply {
            self.apply_filters(ctx.spawner);
        }
    }

    fn bottom_bar(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let p = palette(ui);
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, egui::CornerRadius::ZERO, p.header_bg);
        ui.painter().line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, p.border));
        ui.horizontal_centered(|ui| {
            ui.add_space(8.0);
            let mut view = self.view;
            if widgets::segmented(ui, &mut view, &[(TableView::Data, "Data"), (TableView::Structure, "Structure")]) {
                self.view = view;
            }
            ui.add_space(6.0);
            if self.view == TableView::Data {
                let editable = self.editable();
                if widgets::icon_button_sized(ui, Icon::Plus, "Add row", vec2(26.0, 24.0), editable).clicked() {
                    let ncols = self.result.as_ref().map_or(0, |r| r.columns.len());
                    self.staged.add_row(ncols);
                    let rows = self.result.as_ref().map_or(0, |r| r.rows.len());
                    self.grid.select_row(rows + self.staged.inserted.len() - 1, 0);
                }
                let has_sel = !self.grid.selected_rows().is_empty();
                if widgets::icon_button_sized(
                    ui,
                    Icon::Minus,
                    "Delete selected rows",
                    vec2(26.0, 24.0),
                    editable && has_sel,
                )
                .clicked()
                {
                    let rows = self.grid.selected_rows();
                    let page_len = self.result.as_ref().map_or(0, |r| r.rows.len());
                    self.staged.delete(page_len, &rows);
                }
                let filter_active = self
                    .applied_filters
                    .iter()
                    .any(|f| sqlgen::filter_condition(self.dialect(), f, &self.column_names()).is_some());
                let fr = widgets::icon_button(ui, Icon::Filter, &format!("Filter rows ({})", shortcut_label("F")));
                if filter_active {
                    ui.painter().circle_filled(fr.rect.right_top() + vec2(-5.0, 5.0), 3.5, p.accent);
                }
                if fr.clicked() {
                    self.show_filters = !self.show_filters;
                    self.focus_filter = self.show_filters;
                }
                if widgets::icon_button(ui, Icon::Refresh, &format!("Reload ({})", shortcut_label("R"))).clicked() {
                    self.reload(ctx.spawner, true);
                }
                let r = widgets::icon_button(ui, Icon::Export, "Export / Import");
                egui::Popup::menu(&r).show(|ui| {
                    ui.set_min_width(200.0);
                    if ui.button("Export table…").clicked() {
                        ctx.actions.push(Action::Export(ExportSource::Table {
                            object: self.object.clone(),
                            where_clause: self.where_clause(),
                            sort: self.sort.clone(),
                        }));
                    }
                    if let Some(rs) = &self.result
                        && ui.button("Export current page…").clicked()
                    {
                        ctx.actions.push(Action::Export(ExportSource::Rows {
                            name: self.object.name.clone(),
                            table: self.dialect().qualified(&self.object.schema, &self.object.name),
                            columns: rs.columns.clone(),
                            rows: rs.rows.clone(),
                        }));
                    }
                    ui.separator();
                    if self.object.kind == tableplus_core::model::ObjectKind::Table
                        && ui.button("Import CSV / SQL…").clicked()
                    {
                        ctx.actions.push(Action::Import(self.object.clone()));
                    }
                });
            } else if widgets::icon_button(ui, Icon::Refresh, "Reload structure").clicked() {
                self.load_structure(ctx.spawner);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                if self.view == TableView::Data {
                    if !self.staged.is_empty() {
                        let n = self.staged.count();
                        let busy = self.commit_task.is_some();
                        if widgets::primary_button_enabled(ui, &format!("Commit ({n})"), !busy)
                            .on_hover_text(shortcut_label("S"))
                            .clicked()
                        {
                            self.commit(ctx);
                        }
                        if widgets::button(ui, "Preview").clicked() {
                            self.preview = Some(self.pending_statements());
                        }
                        if widgets::button(ui, "Discard").clicked() {
                            self.staged.clear();
                        }
                        if busy {
                            ui.add(egui::Spinner::new().size(14.0));
                        }
                        ui.add_space(8.0);
                    }
                    self.pagination(ui, ctx);
                } else {
                    self.editor.bottom_actions(ui, self.structure.as_ref(), self.dialect(), &self.object);
                }
            });
        });
    }

    fn pagination(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let p = palette(ui);
        let loaded = self.result.as_ref().map_or(0, |r| r.rows.len()) as u64;
        let start = self.page * self.page_size as u64;
        let pages = self.total.map(|t| t.div_ceil(self.page_size as u64).max(1));
        let has_next = match self.total {
            Some(t) => start + loaded < t,
            None => loaded == self.page_size as u64,
        };
        let size_label = format!("{} rows", self.page_size);
        let mut new_size = None;
        egui::ComboBox::from_id_salt(("page_size", &self.object)).selected_text(size_label).width(84.0).show_ui(
            ui,
            |ui| {
                for s in [100usize, 300, 500, 1000, 5000, 20000] {
                    if ui.selectable_label(self.page_size == s, format!("{s} rows")).clicked() {
                        new_size = Some(s);
                    }
                }
            },
        );
        if let Some(s) = new_size {
            self.page_size = s;
            self.page = 0;
            self.staged.clear();
            self.reload(ctx.spawner, false);
        }
        let dirty = !self.staged.is_empty();
        if widgets::icon_button_sized(ui, Icon::ChevronRight, "Next page", vec2(24.0, 24.0), has_next && !dirty)
            .clicked()
        {
            self.page += 1;
            self.reload(ctx.spawner, false);
        }
        let label = match pages {
            Some(n) => format!("Page {} of {}", fmt_count(self.page + 1), fmt_count(n)),
            None => format!("Page {}", self.page + 1),
        };
        ui.label(RichText::new(label).color(p.text_muted).size(12.0));
        if widgets::icon_button_sized(ui, Icon::ChevronLeft, "Previous page", vec2(24.0, 24.0), self.page > 0 && !dirty)
            .clicked()
        {
            self.page -= 1;
            self.reload(ctx.spawner, false);
        }
        ui.add_space(6.0);
        let info = if self.load_task.is_some() {
            "Loading…".to_string()
        } else if loaded == 0 {
            "0 rows".to_string()
        } else {
            let total = match (self.total, self.count_task.is_some()) {
                (Some(t), _) => fmt_count(t),
                (None, true) => "…".into(),
                (None, false) => "?".into(),
            };
            format!("{}–{} of {} rows", fmt_count(start + 1), fmt_count(start + loaded), total)
        };
        let mut r = ui.label(RichText::new(info).color(p.text_muted).size(12.0));
        if let Some(e) = self.elapsed {
            r = r.on_hover_text(format!("{}\n{}", self.last_sql, fmt_duration(e)));
        }
        let _ = r;
        if self.load_task.is_some() {
            ui.add(egui::Spinner::new().size(13.0));
        }
    }
}

pub fn loading(ui: &mut Ui, text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.add(egui::Spinner::new().size(18.0));
        ui.label(widgets::muted(ui, text));
    });
}

pub fn error_box(ui: &mut Ui, err: &str) {
    let p = palette(ui);
    ui.add_space(16.0);
    egui::Frame::new()
        .fill(p.danger.gamma_multiply(0.1))
        .stroke(Stroke::new(1.0, p.danger.gamma_multiply(0.5)))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .outer_margin(egui::Margin::symmetric(16, 0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(err).color(p.danger).font(crate::theme::mono_font(12.0)));
        });
}

/// Text for "Copy rows as …".
pub fn copy_rows(
    rs: &ResultSet,
    rows: &[usize],
    format: CopyFormat,
    dialect: Dialect,
    object: &SchemaObject,
) -> String {
    let picked: Vec<Row> = rows.iter().filter_map(|r| rs.rows.get(*r).cloned()).collect();
    let cols: Vec<String> = rs.columns.iter().map(|c| c.name.clone()).collect();
    match format {
        CopyFormat::Tsv => picked
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.to_text().unwrap_or_else(|| "NULL".into()).replace(['\t', '\n'], " "))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            let opts = ExportOptions {
                format: match other {
                    CopyFormat::Csv => ExportFormat::Csv,
                    CopyFormat::Json => ExportFormat::Json,
                    _ => ExportFormat::SqlInsert,
                },
                table: dialect.qualified(&object.schema, &object.name),
                dialect,
                rows_per_insert: 1,
                ..Default::default()
            };
            String::from_utf8(export_to_vec(opts, &cols, &picked).unwrap_or_default()).unwrap_or_default()
        }
    }
}
