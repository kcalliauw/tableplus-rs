//! SQL editor tab: run the statement under the cursor / the selection / everything, with one
//! result tab per statement and a messages log.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use chrono::Utc;
use eframe::egui::{self, Align, CornerRadius, Layout, RichText, Sense, Stroke, Ui, vec2};
use tableplus_core::driver;
use tableplus_core::model::{QueryOutput, StatementResult};
use tableplus_core::sqltext::{split_statements, statement_at};
use tableplus_core::store::HistoryEntry;
use tableplus_core::{Dialect, Value};

use crate::app::{Action, shortcut_label};
use crate::editor::CodeEditor;
use crate::export_dialog::ExportSource;
use crate::grid::{self, GridEvent, GridState, ResultData};
use crate::icons::Icon;
use crate::task::{Task, poll_slot};
use crate::theme::{self, palette};
use crate::widgets::{self, fmt_count, fmt_duration};
use crate::workspace::TabCtx;

struct Outcome {
    sql: String,
    result: Result<QueryOutput, String>,
    elapsed: Duration,
}

struct ResultPane {
    output: QueryOutput,
    grid: GridState,
    sort: Option<(usize, bool)>,
}

pub struct QueryTab {
    pub title: String,
    pub sql: String,
    editor: CodeEditor,
    cursor: usize,
    selection: Option<(usize, usize)>,
    panes: Vec<ResultPane>,
    messages: Vec<(bool, String)>,
    active: usize,
    show_messages: bool,
    running: Option<Task<Vec<Outcome>>>,
    started: Option<Instant>,
    total_elapsed: Option<Duration>,
}

impl QueryTab {
    pub fn new(title: String, sql: String) -> Self {
        let mut editor = CodeEditor::default();
        editor.set_cursor(sql.len());
        Self {
            title,
            sql,
            editor,
            cursor: 0,
            selection: None,
            panes: Vec::new(),
            messages: Vec::new(),
            active: 0,
            show_messages: false,
            running: None,
            started: None,
            total_elapsed: None,
        }
    }

    fn statements_to_run(&self, dialect: Dialect, all: bool) -> Vec<String> {
        if let Some((a, b)) = self.selection.filter(|_| !all) {
            let sel = &self.sql[a.min(self.sql.len())..b.min(self.sql.len())];
            return split_statements(sel, dialect).iter().map(|s| s.text(sel).to_string()).collect();
        }
        if all {
            return split_statements(&self.sql, dialect).iter().map(|s| s.text(&self.sql).to_string()).collect();
        }
        statement_at(&self.sql, self.cursor, dialect).map(|s| vec![s.text(&self.sql).to_string()]).unwrap_or_default()
    }

    fn run(&mut self, ctx: &mut TabCtx, all: bool) {
        if self.running.is_some() {
            return;
        }
        let stmts = self.statements_to_run(ctx.dialect, all);
        if stmts.is_empty() {
            return;
        }
        let conn = ctx.conn.clone();
        let limit = ctx.settings.row_limit();
        self.started = Some(Instant::now());
        self.running = Some(ctx.spawner.spawn(async move {
            let mut out = Vec::new();
            for sql in stmts {
                let start = Instant::now();
                let result = driver::run(conn.as_ref(), &sql, limit).await.map_err(|e| format!("{e:#}"));
                let failed = result.is_err();
                out.push(Outcome { sql, result, elapsed: start.elapsed() });
                if failed {
                    break;
                }
            }
            Ok(out)
        }));
    }

    pub fn poll(&mut self, ctx: &mut TabCtx) {
        let Some(result) = poll_slot(&mut self.running) else { return };
        self.total_elapsed = self.started.take().map(|s| s.elapsed());
        let outcomes = match result {
            Ok(o) => o,
            Err(e) => vec![Outcome { sql: String::new(), result: Err(format!("{e:#}")), elapsed: Duration::ZERO }],
        };
        self.panes.clear();
        self.messages.clear();
        let mut ddl = false;
        for o in outcomes {
            let first_word = o.sql.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
            ddl |= matches!(first_word.as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE");
            let (rows, error) = match &o.result {
                Ok(out) => match &out.result {
                    StatementResult::Rows(rs) => (Some(rs.rows.len() as u64), None),
                    StatementResult::Affected(n) => (Some(*n), None),
                },
                Err(e) => (None, Some(e.clone())),
            };
            if !o.sql.is_empty() {
                ctx.actions.push(Action::RecordHistory(HistoryEntry {
                    connection_id: ctx.connection.id,
                    connection_name: ctx.connection.display_name(),
                    sql: o.sql.clone(),
                    executed_at: Utc::now(),
                    duration_ms: o.elapsed.as_millis() as u64,
                    rows,
                    error: error.clone(),
                }));
            }
            let short: String = o.sql.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
            match o.result {
                Ok(out) => {
                    let msg = match &out.result {
                        StatementResult::Rows(rs) => {
                            let more = if rs.truncated { " (truncated by row limit)" } else { "" };
                            format!(
                                "{} rows returned{more} · {} · {short}",
                                fmt_count(rs.rows.len() as u64),
                                fmt_duration(out.elapsed)
                            )
                        }
                        StatementResult::Affected(n) => {
                            format!("{} rows affected · {} · {short}", fmt_count(*n), fmt_duration(out.elapsed))
                        }
                    };
                    self.messages.push((false, msg));
                    if out.rows().is_some() {
                        self.panes.push(ResultPane { output: out, grid: GridState::default(), sort: None });
                    }
                }
                Err(e) => self.messages.push((true, format!("{e}\n→ {short}"))),
            }
        }
        self.active = 0;
        self.show_messages = self.panes.is_empty() || self.messages.iter().any(|(err, _)| *err);
        if ddl {
            ctx.actions.push(Action::RefreshObjects);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let sc = *ctx.shortcuts;
        if sc.run {
            self.run(ctx, false);
        }
        if sc.run_all {
            self.run(ctx, true);
        }
        let p = palette(ui);

        egui::Panel::top(egui::Id::new(("query_toolbar", &self.title))).exact_size(38.0).frame(egui::Frame::NONE).show(
            ui,
            |ui| {
                let rect = ui.max_rect();
                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
                ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    let running = self.running.is_some();
                    let label = if self.selection.is_some() { "Run selection" } else { "Run current" };
                    if ui
                        .add_enabled_ui(!running, |ui| widgets::icon_text_button(ui, Icon::Play, label))
                        .inner
                        .on_hover_text(shortcut_label("↵"))
                        .clicked()
                    {
                        self.run(ctx, false);
                    }
                    if ui
                        .add_enabled_ui(!running, |ui| widgets::icon_text_button(ui, Icon::PlayAll, "Run all"))
                        .inner
                        .on_hover_text(shortcut_label("⇧↵"))
                        .clicked()
                    {
                        self.run(ctx, true);
                    }
                    if running {
                        if widgets::icon_text_button(ui, Icon::Stop, "Stop").clicked() {
                            let conn = ctx.conn.clone();
                            ctx.spawner.detach(async move {
                                let _ = conn.cancel().await;
                            });
                        }
                        ui.add(egui::Spinner::new().size(14.0));
                        if let Some(s) = self.started {
                            ui.label(widgets::muted(ui, fmt_duration(s.elapsed())));
                            ui.ctx().request_repaint_after(Duration::from_millis(100));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(10.0);
                        let limit = match ctx.settings.row_limit() {
                            Some(n) => format!("Limit {} rows", fmt_count(n as u64)),
                            None => "No row limit".into(),
                        };
                        ui.label(RichText::new(limit).size(11.5).color(p.text_faint));
                    });
                });
            },
        );

        let editor_h = (ui.available_height() * 0.42).clamp(140.0, 420.0);
        egui::Panel::top(egui::Id::new(("query_editor", &self.title)))
            .resizable(true)
            .default_size(editor_h)
            .size_range(80.0..=ui.available_height() - 80.0)
            .frame(egui::Frame::new().fill(p.editor_bg))
            .show(ui, |ui| {
                let id = egui::Id::new(("sql_editor", &self.title));
                let out =
                    self.editor.show(ui, id, &mut self.sql, ctx.dialect, ctx.catalog, ctx.settings.editor_font_size);
                self.cursor = out.cursor;
                self.selection = out.selection;
                for (schema, table) in out.missing_columns {
                    ctx.actions.push(Action::LoadStructure { schema, table });
                }
            });

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            self.results(ui, ctx);
        });
    }

    fn results(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let p = palette(ui);
        // Result tab strip.
        egui::Panel::top(egui::Id::new(("result_tabs", &self.title))).exact_size(30.0).frame(egui::Frame::NONE).show(
            ui,
            |ui| {
                let rect = ui.max_rect();
                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
                ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
                ui.horizontal_centered(|ui| {
                    ui.add_space(6.0);
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for i in 0..self.panes.len() {
                        let selected = !self.show_messages && self.active == i;
                        let label =
                            if self.panes.len() == 1 { "Result".to_string() } else { format!("Result {}", i + 1) };
                        if result_tab(ui, &label, selected) {
                            self.active = i;
                            self.show_messages = false;
                        }
                    }
                    let errors = self.messages.iter().filter(|(e, _)| *e).count();
                    let label = if errors > 0 { format!("Messages ({errors} error)") } else { "Messages".to_string() };
                    if result_tab(ui, &label, self.show_messages) {
                        self.show_messages = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        if let Some(pane) = self.panes.get(self.active).filter(|_| !self.show_messages)
                            && let Some(rs) = pane.output.rows()
                        {
                            if widgets::icon_button(ui, Icon::Export, "Export result…").clicked() {
                                ctx.actions.push(Action::Export(ExportSource::Rows {
                                    name: "query_result".into(),
                                    table: ctx.dialect.quote_ident("query_result"),
                                    columns: rs.columns.clone(),
                                    rows: rs.rows.clone(),
                                }));
                            }
                            let trunc = if rs.truncated { " (limited)" } else { "" };
                            ui.label(
                                RichText::new(format!(
                                    "{} rows{trunc} · {}",
                                    fmt_count(rs.rows.len() as u64),
                                    fmt_duration(pane.output.elapsed)
                                ))
                                .size(12.0)
                                .color(p.text_muted),
                            )
                            .on_hover_text(&pane.output.sql);
                        }
                    });
                });
            },
        );

        if self.show_messages || self.panes.is_empty() {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(8.0);
                if self.messages.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(widgets::muted(
                            ui,
                            format!(
                                "Write SQL above and press {} to run the statement under the cursor, or {} to run everything.",
                                shortcut_label("↵"),
                                shortcut_label("⇧↵")
                            ),
                        ));
                    });
                }
                for (err, msg) in &self.messages {
                    ui.horizontal_top(|ui| {
                        ui.add_space(14.0);
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 18.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, if *err { p.danger } else { p.success });
                        ui.add(egui::Label::new(RichText::new(msg).font(theme::mono_font(12.0)).color(if *err { p.danger } else { p.text })).wrap());
                    });
                    ui.add_space(4.0);
                }
                if let Some(t) = self.total_elapsed {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(RichText::new(format!("Total time {}", fmt_duration(t))).size(11.5).color(p.text_faint));
                    });
                }
            });
            return;
        }

        let active = self.active.min(self.panes.len() - 1);
        let pane = &mut self.panes[active];
        let Some(rs) = (match &mut pane.output.result {
            StatementResult::Rows(rs) => Some(rs),
            StatementResult::Affected(_) => None,
        }) else {
            return;
        };
        let id = egui::Id::new(("result_grid", &self.title, active));
        let events = grid::show(ui, id, &mut pane.grid, &SortedData { inner: ResultData { rs }, sort: pane.sort });
        for ev in events {
            match ev {
                GridEvent::Sort { col, .. } => {
                    let desc = match pane.sort {
                        Some((c, false)) if c == col => true,
                        Some((c, true)) if c == col => {
                            pane.sort = None;
                            continue;
                        }
                        _ => false,
                    };
                    pane.sort = Some((col, desc));
                    rs.rows.sort_by(|a, b| {
                        let o = cmp_values(&a[col], &b[col]);
                        if desc { o.reverse() } else { o }
                    });
                }
                GridEvent::CopyAs { rows, format } => {
                    let obj = tableplus_core::model::SchemaObject::new(
                        "",
                        "query_result",
                        tableplus_core::model::ObjectKind::Table,
                    );
                    ctx.egui.copy_text(crate::table_tab::copy_rows(rs, &rows, format, ctx.dialect, &obj));
                }
                GridEvent::FilterByValue { .. } => {}
                _ => {}
            }
        }
    }
}

fn result_tab(ui: &mut Ui, label: &str, selected: bool) -> bool {
    let p = palette(ui);
    let font = if selected { theme::bold_font(12.0) } else { theme::ui_font(12.0) };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, if selected { p.text } else { p.text_muted });
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 24.0), Sense::click());
    if selected {
        ui.painter().rect(
            rect,
            CornerRadius::same(6),
            p.content_bg,
            Stroke::new(1.0, p.border),
            egui::StrokeKind::Inside,
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
    }
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text);
    resp.clicked()
}

/// Orders values for client-side sorting of query results.
pub fn cmp_values(a: &Value, b: &Value) -> Ordering {
    fn num(v: &Value) -> Option<f64> {
        match v {
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Float(f) => Some(*f),
            Value::Decimal(d) => d.parse().ok(),
            _ => None,
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => match (a, b) {
                (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
                (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
                _ => a.display().cmp(&b.display()),
            },
        },
    }
}

/// Wraps a result to show the client-side sort indicator.
struct SortedData<'a> {
    inner: ResultData<'a>,
    sort: Option<(usize, bool)>,
}

impl grid::GridData for SortedData<'_> {
    fn columns(&self) -> &[tableplus_core::model::ColumnMeta] {
        self.inner.columns()
    }
    fn row_count(&self) -> usize {
        self.inner.row_count()
    }
    fn cell(&self, row: usize, col: usize) -> grid::CellView<'_> {
        self.inner.cell(row, col)
    }
    fn row_state(&self, row: usize) -> grid::CellState {
        self.inner.row_state(row)
    }
    fn full_text(&self, row: usize, col: usize) -> Option<String> {
        self.inner.full_text(row, col)
    }
    fn sort_of(&self, col: usize) -> Option<bool> {
        self.sort.filter(|(c, _)| *c == col).map(|(_, d)| d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_ordering() {
        let mut v = [
            Value::text("b"),
            Value::Null,
            Value::Int(10),
            Value::Float(2.5),
            Value::Decimal("3".into()),
            Value::text("a"),
        ];
        v.sort_by(cmp_values);
        assert_eq!(v[0], Value::Null);
        assert_eq!(v[1], Value::Float(2.5));
        assert_eq!(v[2], Value::Decimal("3".into()));
        assert_eq!(v[3], Value::Int(10));
    }

    #[test]
    fn picks_statements() {
        let mut t = QueryTab::new("q".into(), "select 1;\nselect 2;\nselect 3".into());
        t.cursor = 12;
        assert_eq!(t.statements_to_run(Dialect::Postgres, false), vec!["select 2"]);
        assert_eq!(t.statements_to_run(Dialect::Postgres, true).len(), 3);
        t.selection = Some((0, 18));
        assert_eq!(t.statements_to_run(Dialect::Postgres, false), vec!["select 1", "select 2"]);
    }
}
