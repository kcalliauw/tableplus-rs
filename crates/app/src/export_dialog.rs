//! Export (CSV / JSON / SQL INSERT) and import (CSV / SQL) dialogs. Large tables are
//! streamed page by page so exports never hold the whole table in memory.

use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use eframe::egui::{self, Align, Key, Layout, RichText, TextEdit, vec2};
use tableplus_core::Dialect;
use tableplus_core::driver::DynConnection;
use tableplus_core::export::{
    CsvImportOptions, ExportFormat, ExportOptions, Exporter, csv_to_inserts, sql_script_statements,
};
use tableplus_core::model::{ColumnMeta, Row, SchemaObject};
use tableplus_core::sqlgen::{self, SortKey};

use crate::app::{Action, ToastKind};
use crate::task::{Spawner, Task, poll_slot};
use crate::theme::palette;
use crate::widgets::{self, fmt_count};

pub enum ExportSource {
    /// Rows already in memory (a query result or the current page).
    Rows { name: String, table: String, columns: Vec<ColumnMeta>, rows: Vec<Row> },
    /// A whole table/view, re-queried in pages.
    Table { object: SchemaObject, where_clause: Option<String>, sort: Vec<SortKey> },
}

impl ExportSource {
    fn name(&self) -> &str {
        match self {
            ExportSource::Rows { name, .. } => name,
            ExportSource::Table { object, .. } => &object.name,
        }
    }
}

fn default_dir() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

pub struct ExportDialog {
    source: ExportSource,
    conn: DynConnection,
    dialect: Dialect,
    format: ExportFormat,
    path: String,
    include_header: bool,
    delimiter: u8,
    rows_per_insert: usize,
    progress: Arc<AtomicU64>,
    task: Option<Task<u64>>,
    error: Option<String>,
}

impl ExportDialog {
    pub fn new(source: ExportSource, conn: DynConnection, dialect: Dialect) -> Self {
        let path = default_dir().join(format!("{}.csv", source.name())).display().to_string();
        Self {
            source,
            conn,
            dialect,
            format: ExportFormat::Csv,
            path,
            include_header: true,
            delimiter: b',',
            rows_per_insert: 100,
            progress: Arc::new(AtomicU64::new(0)),
            task: None,
            error: None,
        }
    }

    fn set_format(&mut self, f: ExportFormat) {
        self.format = f;
        let p = PathBuf::from(&self.path);
        self.path = p.with_extension(f.extension()).display().to_string();
    }

    fn start(&mut self, spawner: &Spawner) {
        let path = PathBuf::from(crate::util::expand_tilde(self.path.trim()));
        let dialect = self.dialect;
        let progress = self.progress.clone();
        progress.store(0, Ordering::Relaxed);
        let mut opts = ExportOptions {
            format: self.format,
            csv_delimiter: self.delimiter,
            include_header: self.include_header,
            dialect,
            rows_per_insert: self.rows_per_insert,
            ..Default::default()
        };
        match &self.source {
            ExportSource::Rows { table, columns, rows, .. } => {
                opts.table = table.clone();
                let cols: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
                let rows = rows.clone();
                self.task = Some(spawner.spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        let file =
                            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
                        let mut ex = Exporter::new(BufWriter::new(file), opts, cols)?;
                        ex.write_rows(rows.iter().map(|r| &r[..]))?;
                        progress.store(rows.len() as u64, Ordering::Relaxed);
                        ex.finish()
                    })
                    .await?
                }));
            }
            ExportSource::Table { object, where_clause, sort } => {
                opts.table = dialect.qualified(&object.schema, &object.name);
                let conn = self.conn.clone();
                let object = object.clone();
                let where_clause = where_clause.clone();
                let sort = sort.clone();
                self.task = Some(spawner.spawn(async move {
                    // One streaming query: rows flow to the file in batches, so memory stays
                    // flat and no LIMIT/OFFSET paging (or total ordering) is needed.
                    let sql = sqlgen::select_page_sql(
                        dialect,
                        &object.schema,
                        &object.name,
                        where_clause.as_deref(),
                        &sort,
                        None,
                        0,
                    );
                    let file = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
                    let mut file = Some(file);
                    let mut exporter: Option<Exporter<BufWriter<std::fs::File>>> = None;
                    let mut written = 0u64;
                    conn.query_batches(&sql, 5_000, &mut |cols, rows| {
                        if exporter.is_none() {
                            let names = cols.iter().map(|c| c.name.clone()).collect();
                            exporter =
                                Some(Exporter::new(BufWriter::new(file.take().expect("file")), opts.clone(), names)?);
                        }
                        let ex = exporter.as_mut().expect("exporter");
                        ex.write_rows(rows.iter().map(|r| &r[..]))?;
                        written += rows.len() as u64;
                        progress.store(written, Ordering::Relaxed);
                        Ok(())
                    })
                    .await?;
                    match exporter {
                        Some(ex) => ex.finish(),
                        None => Ok(0),
                    }
                }));
            }
        }
    }

    /// Returns false once the dialog should close.
    pub fn show(&mut self, ctx: &egui::Context, spawner: &Spawner, actions: &mut Vec<Action>) -> bool {
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(n) => {
                    actions.push(Action::Toast(
                        ToastKind::Success,
                        format!("Exported {} to {}", widgets::plural(n, "row"), self.path),
                    ));
                    return false;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
        let mut open = true;
        crate::widgets::modal(ctx, "export_dialog").show(ctx, |ui| {
            ui.set_width(520.0);
            let p = palette(ui);
            ui.heading(format!("Export {}", self.source.name()));
            let what = match &self.source {
                ExportSource::Rows { rows, .. } => {
                    format!("{} from the current result", widgets::plural(rows.len() as u64, "row"))
                }
                ExportSource::Table { where_clause: Some(_), .. } => "All rows matching the current filter".to_string(),
                ExportSource::Table { .. } => "All rows of the table (streamed from the server)".to_string(),
            };
            ui.label(widgets::muted(ui, what));
            ui.add_space(10.0);
            let mut fmt = self.format;
            if widgets::segmented(
                ui,
                &mut fmt,
                &[(ExportFormat::Csv, "CSV"), (ExportFormat::Json, "JSON"), (ExportFormat::SqlInsert, "SQL INSERT")],
            ) {
                self.set_format(fmt);
            }
            ui.add_space(8.0);
            match self.format {
                ExportFormat::Csv => {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.include_header, "Header row");
                        ui.add_space(16.0);
                        ui.label("Delimiter");
                        for (d, label) in [(b',', "Comma"), (b';', "Semicolon"), (b'\t', "Tab")] {
                            ui.selectable_value(&mut self.delimiter, d, label);
                        }
                    });
                }
                ExportFormat::SqlInsert => {
                    ui.horizontal(|ui| {
                        ui.label("Rows per INSERT");
                        ui.add(egui::DragValue::new(&mut self.rows_per_insert).range(1..=10_000));
                    });
                }
                ExportFormat::Json => {
                    ui.label(widgets::muted(ui, "An array of objects keyed by column name."));
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Save to");
                ui.add(TextEdit::singleline(&mut self.path).desired_width(ui.available_width() - 84.0));
                if widgets::button(ui, "Browse…").clicked() {
                    let file_name = PathBuf::from(&self.path)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if let Some(path) = rfd::FileDialog::new().set_file_name(file_name).save_file() {
                        self.path = path.display().to_string();
                    }
                }
            });
            if let Some(e) = &self.error {
                ui.add_space(6.0);
                ui.label(RichText::new(e).color(p.danger));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if self.task.is_some() {
                    ui.add(egui::Spinner::new());
                    ui.label(widgets::muted(
                        ui,
                        format!("{} rows written…", fmt_count(self.progress.load(Ordering::Relaxed))),
                    ));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::primary_button_enabled(
                        ui,
                        "Export",
                        self.task.is_none() && !self.path.trim().is_empty(),
                    )
                    .clicked()
                    {
                        self.error = None;
                        self.start(spawner);
                    }
                    if widgets::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        open = false;
                    }
                });
            });
        });
        open
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportKind {
    Csv,
    Sql,
}

pub struct ImportDialog {
    object: SchemaObject,
    conn: DynConnection,
    dialect: Dialect,
    kind: ImportKind,
    path: String,
    has_header: bool,
    empty_as_null: bool,
    delimiter: u8,
    task: Option<Task<String>>,
    error: Option<String>,
}

impl ImportDialog {
    pub fn new(object: SchemaObject, conn: DynConnection, dialect: Dialect) -> Self {
        Self {
            object,
            conn,
            dialect,
            kind: ImportKind::Csv,
            path: String::new(),
            has_header: true,
            empty_as_null: true,
            delimiter: b',',
            task: None,
            error: None,
        }
    }

    fn start(&mut self, spawner: &Spawner) {
        let path = PathBuf::from(crate::util::expand_tilde(self.path.trim()));
        let conn = self.conn.clone();
        let dialect = self.dialect;
        let object = self.object.clone();
        let kind = self.kind;
        let opts = CsvImportOptions {
            delimiter: self.delimiter,
            has_header: self.has_header,
            empty_as_null: self.empty_as_null,
            ..Default::default()
        };
        self.task = Some(spawner.spawn(async move {
            let bytes = tokio::fs::read(&path).await.with_context(|| format!("reading {}", path.display()))?;
            match kind {
                ImportKind::Csv => {
                    let st = conn.table_structure(&object.schema, &object.name).await?;
                    let (stmts, n) =
                        csv_to_inserts(dialect, &object.schema, &object.name, &st.columns, bytes.as_slice(), &opts)?;
                    conn.execute_in_transaction(&stmts).await?;
                    Ok(format!("Imported {} into {}", widgets::plural(n, "row"), object.name))
                }
                ImportKind::Sql => {
                    let script = String::from_utf8(bytes).context("SQL file is not UTF-8")?;
                    let stmts = sql_script_statements(&script, dialect);
                    let n = stmts.len();
                    if dialect == Dialect::MySql {
                        for s in &stmts {
                            conn.query(s, None).await.with_context(|| format!("executing: {s}"))?;
                        }
                    } else {
                        conn.execute_in_transaction(&stmts).await?;
                    }
                    Ok(format!("Executed {n} statements"))
                }
            }
        }));
    }

    pub fn show(&mut self, ctx: &egui::Context, spawner: &Spawner, actions: &mut Vec<Action>) -> bool {
        if let Some(r) = poll_slot(&mut self.task) {
            match r {
                Ok(msg) => {
                    actions.push(Action::Toast(ToastKind::Success, msg));
                    actions.push(Action::RefreshObjects);
                    actions.push(Action::ReloadTable(self.object.clone()));
                    return false;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
        let mut open = true;
        crate::widgets::modal(ctx, "import_dialog").show(ctx, |ui| {
            ui.set_width(520.0);
            let p = palette(ui);
            ui.heading(format!("Import into {}", self.object.name));
            ui.add_space(8.0);
            widgets::segmented(ui, &mut self.kind, &[(ImportKind::Csv, "CSV file"), (ImportKind::Sql, "SQL file")]);
            ui.add_space(8.0);
            match self.kind {
                ImportKind::Csv => {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.has_header, "First row is a header");
                        ui.checkbox(&mut self.empty_as_null, "Empty fields are NULL");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Delimiter");
                        for (d, label) in [(b',', "Comma"), (b';', "Semicolon"), (b'\t', "Tab")] {
                            ui.selectable_value(&mut self.delimiter, d, label);
                        }
                    });
                    ui.label(widgets::muted(
                        ui,
                        "Header names are matched to columns; all rows are inserted in one transaction.",
                    ));
                }
                ImportKind::Sql => {
                    ui.label(widgets::muted(
                        ui,
                        "Statements are split on ; (and DELIMITER for MySQL dumps) and run in order.",
                    ));
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("File");
                ui.add(
                    TextEdit::singleline(&mut self.path)
                        .hint_text("/path/to/file")
                        .desired_width(ui.available_width() - 84.0),
                );
                if widgets::button(ui, "Browse…").clicked() {
                    let ext: &[&str] = if self.kind == ImportKind::Csv { &["csv", "tsv", "txt"] } else { &["sql"] };
                    if let Some(path) = rfd::FileDialog::new().add_filter("Data", ext).pick_file() {
                        self.path = path.display().to_string();
                    }
                }
            });
            if let Some(e) = &self.error {
                ui.add_space(6.0);
                ui.label(RichText::new(e).color(p.danger));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if self.task.is_some() {
                    ui.add(egui::Spinner::new());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::primary_button_enabled(
                        ui,
                        "Import",
                        self.task.is_none() && !self.path.trim().is_empty(),
                    )
                    .clicked()
                    {
                        self.error = None;
                        self.start(spawner);
                    }
                    if widgets::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        open = false;
                    }
                });
            });
            let _ = vec2(0.0, 0.0);
        });
        open
    }
}
