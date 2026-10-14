//! Per-database SQL spelling: identifier quoting, literals and common type names.

use serde::{Deserialize, Serialize};

use crate::value::{Value, hex_string, parse_hex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
}

impl Dialect {
    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            Dialect::MySql => format!("`{}`", ident.replace('`', "``")),
            Dialect::Postgres | Dialect::Sqlite => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// `schema.table`, omitting the schema when empty (SQLite `main` is also omitted).
    pub fn qualified(self, schema: &str, name: &str) -> String {
        if schema.is_empty() || (self == Dialect::Sqlite && schema == "main") {
            self.quote_ident(name)
        } else {
            format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
        }
    }

    /// A string literal that round-trips any text.
    pub fn quote_str(self, s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            match c {
                '\'' => out.push_str("''"),
                '\\' if self == Dialect::MySql => out.push_str("\\\\"),
                '\0' if self == Dialect::MySql => out.push_str("\\0"),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    }

    pub fn bytes_literal(self, bytes: &[u8]) -> String {
        let hex = &hex_string(bytes)[2..];
        match self {
            Dialect::Postgres => format!("'\\x{}'::bytea", hex.to_ascii_lowercase()),
            Dialect::MySql | Dialect::Sqlite => format!("X'{hex}'"),
        }
    }

    pub fn bool_literal(self, b: bool) -> &'static str {
        match (self, b) {
            (Dialect::Sqlite, true) => "1",
            (Dialect::Sqlite, false) => "0",
            (_, true) => "TRUE",
            (_, false) => "FALSE",
        }
    }

    /// SQL literal for a value read from the database (used to identify rows).
    pub fn literal(self, value: &Value) -> String {
        match value {
            Value::Null => "NULL".into(),
            Value::Bool(b) => self.bool_literal(*b).into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => format!("{f:?}"),
            Value::Float(f) => self.quote_str(&f.to_string()),
            Value::Decimal(s) => s.to_string(),
            Value::Text(s) => self.quote_str(s),
            Value::Bytes(b) => self.bytes_literal(b),
        }
    }

    /// SQL literal for text the user typed into a cell of the given column type.
    ///
    /// Text is sent as a quoted literal and left to the server to coerce into the column type,
    /// which works for numbers, booleans, dates, enums, JSON and arrays alike. Binary columns
    /// accept `0x…` hex.
    pub fn edit_literal(self, column_type: &str, text: &str) -> String {
        if is_binary_type(column_type)
            && let Some(bytes) = parse_hex(text)
        {
            return self.bytes_literal(&bytes);
        }
        if self == Dialect::MySql
            && column_type.to_ascii_lowercase().starts_with("bit")
            && let Ok(n) = text.trim().parse::<u64>()
        {
            return n.to_string();
        }
        self.quote_str(text)
    }

    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "PostgreSQL",
            Dialect::MySql => "MySQL",
            Dialect::Sqlite => "SQLite",
        }
    }

    /// Frequently used column types offered in the structure editor.
    pub fn common_types(self) -> &'static [&'static str] {
        match self {
            Dialect::Postgres => &[
                "integer",
                "bigint",
                "smallint",
                "serial",
                "bigserial",
                "numeric",
                "real",
                "double precision",
                "boolean",
                "text",
                "varchar(255)",
                "char(1)",
                "uuid",
                "date",
                "time",
                "timestamp",
                "timestamptz",
                "interval",
                "json",
                "jsonb",
                "bytea",
                "inet",
                "text[]",
                "integer[]",
            ],
            Dialect::MySql => &[
                "int",
                "bigint",
                "smallint",
                "tinyint(1)",
                "decimal(10,2)",
                "float",
                "double",
                "varchar(255)",
                "char(1)",
                "text",
                "mediumtext",
                "longtext",
                "date",
                "time",
                "datetime",
                "timestamp",
                "json",
                "blob",
                "varbinary(255)",
                "enum('a','b')",
            ],
            Dialect::Sqlite => &["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC", "BOOLEAN", "DATE", "DATETIME"],
        }
    }
}

pub fn is_binary_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    t == "bytea" || t.contains("blob") || t.contains("binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert_eq!(Dialect::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect::MySql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Dialect::Postgres.qualified("public", "users"), "\"public\".\"users\"");
        assert_eq!(Dialect::Sqlite.qualified("main", "users"), "\"users\"");
        assert_eq!(Dialect::MySql.qualified("", "users"), "`users`");
    }

    #[test]
    fn strings() {
        assert_eq!(Dialect::Postgres.quote_str("it's \\ ok"), "'it''s \\ ok'");
        assert_eq!(Dialect::MySql.quote_str("it's \\ ok"), "'it''s \\\\ ok'");
        assert_eq!(Dialect::Sqlite.quote_str("x"), "'x'");
    }

    #[test]
    fn literals() {
        assert_eq!(Dialect::Postgres.literal(&Value::Bool(true)), "TRUE");
        assert_eq!(Dialect::Sqlite.literal(&Value::Bool(true)), "1");
        assert_eq!(Dialect::Postgres.literal(&Value::Float(0.1)), "0.1");
        assert_eq!(Dialect::Postgres.literal(&Value::Bytes(vec![0xAB].into())), "'\\xab'::bytea");
        assert_eq!(Dialect::MySql.literal(&Value::Bytes(vec![0xAB].into())), "X'AB'");
        assert_eq!(Dialect::Postgres.edit_literal("bytea", "0xDEAD"), "'\\xdead'::bytea");
        assert_eq!(Dialect::Postgres.edit_literal("integer", "12"), "'12'");
        assert_eq!(Dialect::MySql.edit_literal("bit(8)", "165"), "165");
    }
}
