//! Schemas, objects and table structure from DuckDB's `duckdb_*()` table functions, for the
//! current database.

use anyhow::{Result, bail};
use tableplus_core::alter::{ColumnDef, create_table_sql};
use tableplus_core::model::{ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, ObjectKind};
use tableplus_core::{Dialect, SchemaObject, StatementResult, TableStructure};

use crate::raw::Conn;
use crate::run;

const D: Dialect = Dialect::DuckDb;

/// Separates list elements that `array_to_string` joins, since names may contain commas.
const SEP: char = '\u{1f}';

type Cells = Vec<Vec<Option<String>>>;

fn cells(c: &Conn, sql: &str) -> Result<Cells> {
    match run::collect(c, sql, None)? {
        StatementResult::Rows(rs) => Ok(rs.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect()),
        StatementResult::Affected(_) => Ok(Vec::new()),
    }
}

fn text(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn list(v: &Option<String>) -> Vec<String> {
    v.as_deref().filter(|s| !s.is_empty()).map(|s| s.split(SEP).map(str::to_string).collect()).unwrap_or_default()
}

fn joined(column: &str) -> String {
    format!("array_to_string({column}, chr({}))", SEP as u32)
}

pub fn databases(c: &Conn) -> Result<Vec<String>> {
    let rows = cells(c, "SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name")?;
    Ok(rows.iter().map(|r| text(&r[0])).collect())
}

pub fn schemas(c: &Conn) -> Result<Vec<String>> {
    let rows = cells(
        c,
        "SELECT schema_name FROM duckdb_schemas() WHERE database_name = current_database() \
         AND (NOT internal OR schema_name = 'main') ORDER BY schema_name <> 'main', schema_name",
    )?;
    Ok(rows.iter().map(|r| text(&r[0])).collect())
}

pub fn objects(c: &Conn) -> Result<Vec<SchemaObject>> {
    let rows = cells(
        c,
        "SELECT schema_name, table_name, 'table' FROM duckdb_tables() \
           WHERE database_name = current_database() AND NOT internal \
         UNION ALL SELECT schema_name, view_name, 'view' FROM duckdb_views() \
           WHERE database_name = current_database() AND NOT internal \
         UNION ALL SELECT DISTINCT schema_name, function_name, 'macro' FROM duckdb_functions() \
           WHERE database_name = current_database() AND NOT internal AND function_type IN ('macro', 'table_macro') \
         ORDER BY 1, 2",
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let kind = match r[2].as_deref() {
                Some("view") => ObjectKind::View,
                Some("macro") => ObjectKind::Function,
                _ => ObjectKind::Table,
            };
            SchemaObject::new(text(&r[0]), text(&r[1]), kind)
        })
        .collect())
}

/// `database_name = current_database() AND schema_name = … AND <column> = …`.
fn filter(schema: &str, column: &str, name: &str) -> String {
    let schema = if schema.is_empty() { "main" } else { schema };
    format!(
        "database_name = current_database() AND schema_name = {} AND {column} = {}",
        D.quote_str(schema),
        D.quote_str(name)
    )
}

pub fn structure(c: &Conn, schema: &str, table: &str) -> Result<TableStructure> {
    let columns = cells(
        c,
        &format!(
            "SELECT column_name, data_type, is_nullable, column_default, comment FROM duckdb_columns() \
             WHERE {} ORDER BY column_index",
            filter(schema, "table_name", table)
        ),
    )?;
    if columns.is_empty() {
        bail!("{table} has no columns or does not exist");
    }
    let constraints = cells(
        c,
        &format!(
            "SELECT constraint_type, constraint_name, {}, expression, referenced_table, {} FROM duckdb_constraints() \
             WHERE {} AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY', 'CHECK') \
             ORDER BY constraint_index",
            joined("constraint_column_names"),
            joined("referenced_column_names"),
            filter(schema, "table_name", table)
        ),
    )?;
    let indexes = cells(
        c,
        &format!(
            "SELECT index_name, is_unique, {}, sql FROM duckdb_indexes() WHERE {} ORDER BY index_name",
            joined("expressions"),
            filter(schema, "table_name", table)
        ),
    )?;
    let comment =
        cells(c, &format!("SELECT comment FROM duckdb_tables() WHERE {}", filter(schema, "table_name", table)))?
            .first()
            .and_then(|r| r[0].clone());

    let mut out = TableStructure { comment, ..Default::default() };
    let mut primary = Vec::new();
    for r in &constraints {
        let columns = list(&r[2]);
        let quoted = |cols: &[String]| cols.iter().map(|c| D.quote_ident(c)).collect::<Vec<_>>().join(", ");
        let (kind, definition) = match r[0].as_deref() {
            Some("PRIMARY KEY") => {
                primary = columns.clone();
                (ConstraintKind::PrimaryKey, format!("PRIMARY KEY ({})", quoted(&columns)))
            }
            Some("UNIQUE") => (ConstraintKind::Unique, format!("UNIQUE ({})", quoted(&columns))),
            // Built from the lists: DuckDB 1.5.6 leaves a self-reference out of its own text.
            Some("FOREIGN KEY") => (
                ConstraintKind::ForeignKey,
                format!("REFERENCES {} ({})", D.quote_ident(&text(&r[4])), quoted(&list(&r[5]))),
            ),
            _ => (ConstraintKind::Check, format!("CHECK ({})", text(&r[3]))),
        };
        out.constraints.push(ConstraintInfo { name: text(&r[1]), kind, columns, definition });
    }
    out.columns = columns
        .iter()
        .map(|r| {
            let name = text(&r[0]);
            ColumnInfo {
                primary_key: primary.contains(&name),
                name,
                data_type: text(&r[1]),
                nullable: r[2].as_deref() == Some("true"),
                default: r[3].clone(),
                extra: None,
                comment: r[4].clone().filter(|c| !c.is_empty()),
            }
        })
        .collect();
    out.indexes = indexes
        .iter()
        .map(|r| IndexInfo {
            name: text(&r[0]),
            columns: list(&r[2]).into_iter().map(|e| e.trim_matches('"').to_string()).collect(),
            unique: r[1].as_deref() == Some("true"),
            primary: false,
            method: Some("art".into()),
            definition: r[3].clone(),
        })
        .collect();
    Ok(out)
}

pub fn definition(c: &Conn, o: &SchemaObject) -> Result<String> {
    let found = match o.kind {
        ObjectKind::View => {
            cells(c, &format!("SELECT sql FROM duckdb_views() WHERE {}", filter(&o.schema, "view_name", &o.name)))?
        }
        ObjectKind::Function => cells(
            c,
            &format!(
                "SELECT 'CREATE MACRO ' || function_name || '(' || {} || ') AS ' || \
                 CASE function_type WHEN 'table_macro' THEN 'TABLE ' ELSE '' END || macro_definition \
                 FROM duckdb_functions() WHERE {} AND function_type IN ('macro', 'table_macro')",
                "array_to_string(parameters, ', ')",
                filter(&o.schema, "function_name", &o.name)
            ),
        )?,
        _ => Vec::new(),
    };
    if let Some(sql) = found.into_iter().next().and_then(|r| r[0].clone()) {
        return Ok(sql);
    }
    if o.kind != ObjectKind::Table {
        bail!("definition not found");
    }
    // A table's own text can be broken (a self-referencing foreign key is missing in 1.5.6), so
    // rebuild it from the structure, which comes from DuckDB's structured catalog.
    let st = structure(c, &o.schema, &o.name)?;
    let cols: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();
    let extra: Vec<String> = st
        .constraints
        .iter()
        .filter_map(|k| match k.kind {
            ConstraintKind::PrimaryKey => None,
            ConstraintKind::ForeignKey => Some(format!(
                "FOREIGN KEY ({}) {}",
                k.columns.iter().map(|c| D.quote_ident(c)).collect::<Vec<_>>().join(", "),
                k.definition
            )),
            ConstraintKind::Unique | ConstraintKind::Check => Some(k.definition.clone()),
        })
        .collect();
    let schema = if o.schema == "main" { "" } else { &o.schema };
    Ok(create_table_sql(D, schema, &o.name, &cols, &extra))
}
