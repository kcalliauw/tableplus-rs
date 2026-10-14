//! Driver-independent descriptions of schema objects and query results.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
}

impl ObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Table => "Table",
            ObjectKind::View => "View",
            ObjectKind::MaterializedView => "Materialized View",
            ObjectKind::Function => "Function",
            ObjectKind::Procedure => "Procedure",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            ObjectKind::Table => "Tables",
            ObjectKind::View => "Views",
            ObjectKind::MaterializedView => "Materialized Views",
            ObjectKind::Function => "Functions",
            ObjectKind::Procedure => "Procedures",
        }
    }

    /// Tables, views and materialized views can be browsed in the data grid.
    pub fn is_relation(self) -> bool {
        matches!(self, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView)
    }
}

/// A table, view or routine as listed in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchemaObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    /// Routine signature (arguments) used to disambiguate overloads.
    pub signature: Option<String>,
}

impl SchemaObject {
    pub fn new(schema: impl Into<String>, name: impl Into<String>, kind: ObjectKind) -> Self {
        Self { schema: schema.into(), name: name.into(), kind, signature: None }
    }

    pub fn qualified_name(&self) -> String {
        if self.schema.is_empty() { self.name.clone() } else { format!("{}.{}", self.schema, self.name) }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColumnInfo {
    pub name: String,
    /// Full type as the server spells it, e.g. `character varying(255)` or `decimal(12,2)`.
    pub data_type: String,
    pub nullable: bool,
    /// Default expression as SQL text.
    pub default: Option<String>,
    pub primary_key: bool,
    /// AUTO_INCREMENT / identity / generated hints.
    pub extra: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// Access method (btree, hash, gin…) when known.
    pub method: Option<String>,
    /// Full DDL when the server exposes it (Postgres, SQLite).
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    PrimaryKey,
    ForeignKey,
    Unique,
    Check,
}

impl ConstraintKind {
    pub fn label(self) -> &'static str {
        match self {
            ConstraintKind::PrimaryKey => "PRIMARY KEY",
            ConstraintKind::ForeignKey => "FOREIGN KEY",
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::Check => "CHECK",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintInfo {
    pub name: String,
    pub kind: ConstraintKind,
    pub columns: Vec<String>,
    /// Human-readable definition (`REFERENCES users(id) ON DELETE CASCADE`, `CHECK (price >= 0)`).
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableStructure {
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub constraints: Vec<ConstraintInfo>,
    pub comment: Option<String>,
}

impl TableStructure {
    pub fn primary_key(&self) -> Vec<String> {
        self.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()
    }

    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self { name: name.into(), type_name: type_name.into() }
    }

    /// Whether the declared type is a number type (used to right-align values the user typed).
    pub fn is_numeric(&self) -> bool {
        let t = self.type_name.to_ascii_lowercase();
        let base = t.split(|c: char| c == '(' || c.is_whitespace()).next().unwrap_or("");
        matches!(
            base,
            "int"
                | "integer"
                | "smallint"
                | "bigint"
                | "tinyint"
                | "mediumint"
                | "int2"
                | "int4"
                | "int8"
                | "serial"
                | "smallserial"
                | "bigserial"
                | "numeric"
                | "decimal"
                | "dec"
                | "real"
                | "double"
                | "float"
                | "float4"
                | "float8"
                | "money"
        )
    }
}

pub type Row = Box<[Value]>;

/// Rows returned by a statement.
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    /// Set when the driver stopped reading because the row limit was reached.
    pub truncated: bool,
}

impl ResultSet {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

#[derive(Debug, Clone)]
pub enum StatementResult {
    Rows(ResultSet),
    Affected(u64),
}

/// The outcome of one executed statement.
#[derive(Debug, Clone)]
pub struct QueryOutput {
    pub sql: String,
    pub result: StatementResult,
    pub elapsed: Duration,
}

impl QueryOutput {
    pub fn rows(&self) -> Option<&ResultSet> {
        match &self.result {
            StatementResult::Rows(rs) => Some(rs),
            StatementResult::Affected(_) => None,
        }
    }

    pub fn into_rows(self) -> Option<ResultSet> {
        match self.result {
            StatementResult::Rows(rs) => Some(rs),
            StatementResult::Affected(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ColumnMeta;

    #[test]
    fn numeric_column_types() {
        for t in ["integer", "BIGINT", "numeric(12,2)", "double precision", "int unsigned", "tinyint(1)", "float8"] {
            assert!(ColumnMeta::new("c", t).is_numeric(), "{t}");
        }
        for t in ["text", "varchar(20)", "timestamp with time zone", "jsonb", "boolean", "interval", ""] {
            assert!(!ColumnMeta::new("c", t).is_numeric(), "{t}");
        }
    }
}
