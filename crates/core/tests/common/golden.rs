//! Reads the EXPLAIN goldens: real servers' EXPLAIN output, captured by the EXPLAIN lab
//! (`tableplus-rs-explain-lab`, a sibling of this checkout). `TABLEPLUS_EXPLAIN_LAB` points at
//! another copy. The goldens stay outside the repository; without them the tests skip.
//!
//! A golden is a `key: value` header, then `== statement`, `== columns` and one
//! `== row R, col C (name): Kind, N bytes` section per cell holding exactly N bytes.

use std::path::{Path, PathBuf};

use tableplus_core::model::{ColumnMeta, ResultSet};
use tableplus_core::{Flavor, ServerInfo, Value};

pub struct Golden {
    /// `mysql-8.4/portable.analyze`
    pub name: String,
    pub target: String,
    pub server: ServerInfo,
    pub query: String,
    pub variant: String,
    /// `ok`, `error` or `skipped`.
    pub outcome: String,
    pub statement: String,
    /// The statement's rows, for `ok` goldens.
    pub result: Option<ResultSet>,
}

impl Golden {
    /// The single text cell of a one-cell result (MySQL TREE and JSON, PostgreSQL JSON).
    pub fn text(&self) -> Option<String> {
        let rs = self.result.as_ref()?;
        (rs.rows.len() == 1 && rs.columns.len() == 1).then(|| rs.rows[0][0].to_text()).flatten()
    }
}

/// The goldens directory, or `None` (skipping the test, with a message) when it's missing.
pub fn dir() -> Option<PathBuf> {
    let lab = std::env::var_os("TABLEPLUS_EXPLAIN_LAB")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tableplus-rs-explain-lab"));
    let dir = lab.join("goldens");
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!("SKIPPED: no EXPLAIN goldens at {}; set TABLEPLUS_EXPLAIN_LAB to the lab checkout", dir.display());
        None
    }
}

/// Every golden, sorted by name, or `None` when the lab is missing.
pub fn load() -> Option<Vec<Golden>> {
    let dir = dir()?;
    let mut out = Vec::new();
    for target in std::fs::read_dir(&dir).expect("goldens directory").flatten() {
        for file in std::fs::read_dir(target.path()).expect("target directory").flatten() {
            if file.path().extension().is_some_and(|e| e == "txt") {
                out.push(read(&file.path()));
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

/// One golden by target, query and variant, such as `("mysql-8.4", "portable", "analyze")`.
pub fn one(target: &str, query: &str, variant: &str) -> Option<Golden> {
    Some(read(&dir()?.join(target).join(format!("{query}.{variant}.txt"))))
}

pub fn read(path: &Path) -> Golden {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let text = String::from_utf8(bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let header = |key: &str| {
        text.lines()
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(": ")))
            .unwrap_or_default()
            .to_string()
    };
    let server_text = header("server");
    let (engine, version) = server_text.split_once(' ').unwrap_or((&server_text, ""));
    let flavor = match engine {
        "MySQL" => Flavor::MySql,
        "MariaDB" => Flavor::MariaDb,
        "PostgreSQL" => Flavor::Postgres,
        "SQLite" => Flavor::Sqlite,
        other => panic!("{}: unknown server {other}", path.display()),
    };
    let section = |name: &str| {
        let start = text.find(&format!("\n== {name}\n"))? + name.len() + 5;
        let end = text[start..].find("\n== ").map_or(text.len(), |i| start + i);
        Some(text[start..end].trim_end().to_string())
    };
    let outcome = header("outcome");
    let result = (outcome == "ok").then(|| result(path, &text, &section("columns").unwrap_or_default()));
    let target = header("target");
    let query = header("query");
    let variant = header("variant");
    Golden {
        name: format!("{target}/{query}.{variant}"),
        target,
        server: ServerInfo::from_version_text(flavor, version),
        query,
        variant,
        outcome,
        statement: section("statement").unwrap_or_default(),
        result,
    }
}

fn result(path: &Path, text: &str, columns: &str) -> ResultSet {
    let columns: Vec<ColumnMeta> = columns
        .lines()
        .map(|l| {
            let l = l.split_once(". ").map_or(l, |(_, rest)| rest);
            let (name, ty) = l.rsplit_once(" (").unwrap_or((l, ")"));
            ColumnMeta::new(name, ty.trim_end_matches(')'))
        })
        .collect();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("\n== row ") {
        let line_end = rest[at + 1..].find('\n').map_or(rest.len(), |i| at + 1 + i);
        let head = &rest[at + 8..line_end];
        let (pos, kind) = head.rsplit_once("): ").unwrap_or_else(|| panic!("{}: bad cell {head}", path.display()));
        let (row, col) = pos.split_once(", col ").expect("row and column");
        let (row, col): (usize, usize) = (row.parse().unwrap(), col.split(' ').next().unwrap().parse().unwrap());
        let (value, consumed) = match kind.split_once(", ") {
            None => (Value::Null, 0),
            Some((kind, size)) => {
                let n: usize = size.trim_end_matches(" bytes").parse().expect("byte count");
                let raw = &rest[line_end + 1..line_end + 1 + n];
                let value = match kind {
                    "Text" => Value::text(raw),
                    "Int" => Value::Int(raw.parse().unwrap()),
                    "UInt" => Value::UInt(raw.parse().unwrap()),
                    "Float" => Value::Float(raw.parse().unwrap()),
                    other => panic!("{}: unknown value kind {other}", path.display()),
                };
                (value, n)
            }
        };
        if rows.len() < row {
            rows.resize_with(row, || vec![Value::Null; columns.len()]);
        }
        rows[row - 1][col - 1] = value;
        rest = &rest[line_end + 1 + consumed..];
    }
    ResultSet { columns, rows: rows.into_iter().map(Vec::into_boxed_slice).collect(), truncated: false }
}
