//! Exporting result sets (CSV / JSON / SQL INSERT) and importing CSV / SQL files.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;
use crate::model::{ColumnInfo, Row};
use crate::sqlgen::CellEdit;
use crate::sqltext::split_statements;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExportFormat {
    Csv,
    Json,
    SqlInsert,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 3] = [ExportFormat::Csv, ExportFormat::Json, ExportFormat::SqlInsert];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::SqlInsert => "SQL INSERT",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert => "sql",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: ExportFormat,
    pub csv_delimiter: u8,
    pub include_header: bool,
    /// Text written for NULL in CSV.
    pub csv_null: String,
    /// Already-quoted target table for SQL INSERT exports.
    pub table: String,
    pub dialect: Dialect,
    /// Rows per INSERT statement.
    pub rows_per_insert: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Csv,
            csv_delimiter: b',',
            include_header: true,
            csv_null: String::new(),
            table: "\"table\"".into(),
            dialect: Dialect::Postgres,
            rows_per_insert: 100,
        }
    }
}

/// Streaming exporter: create, feed rows in batches, then `finish`.
pub struct Exporter<W: Write> {
    out: W,
    opts: ExportOptions,
    columns: Vec<String>,
    rows: u64,
    pending_insert: Vec<String>,
}

impl<W: Write> Exporter<W> {
    pub fn new(mut out: W, opts: ExportOptions, columns: Vec<String>) -> Result<Self> {
        match opts.format {
            ExportFormat::Csv if opts.include_header => {
                let mut w = csv::WriterBuilder::new().delimiter(opts.csv_delimiter).from_writer(Vec::new());
                w.write_record(&columns)?;
                out.write_all(&w.into_inner().map_err(|e| e.into_error())?)?;
            }
            ExportFormat::Csv => {}
            ExportFormat::Json => out.write_all(b"[")?,
            ExportFormat::SqlInsert => {}
        }
        Ok(Self { out, opts, columns, rows: 0, pending_insert: Vec::new() })
    }

    pub fn write_rows<'a>(&mut self, rows: impl IntoIterator<Item = &'a [Value]>) -> Result<()> {
        if self.opts.format == ExportFormat::Csv {
            let mut w = csv::WriterBuilder::new().delimiter(self.opts.csv_delimiter).from_writer(Vec::new());
            let null = self.opts.csv_null.as_str();
            for row in rows {
                w.write_record(row.iter().map(|v| v.to_text().unwrap_or_else(|| null.to_string())))?;
                self.rows += 1;
            }
            self.out.write_all(&w.into_inner().map_err(|e| e.into_error())?)?;
            return Ok(());
        }
        for row in rows {
            self.write_row(row)?;
        }
        Ok(())
    }

    fn write_row(&mut self, row: &[Value]) -> Result<()> {
        match self.opts.format {
            ExportFormat::Csv => unreachable!("CSV rows are written in batches"),
            ExportFormat::Json => {
                let sep = if self.rows == 0 { "\n  " } else { ",\n  " };
                let mut obj = String::from("{");
                for (i, (name, v)) in self.columns.iter().zip(row).enumerate() {
                    if i > 0 {
                        obj.push_str(", ");
                    }
                    obj.push_str(&serde_json::to_string(name)?);
                    obj.push_str(": ");
                    obj.push_str(&json_value(v));
                }
                obj.push('}');
                write!(self.out, "{sep}{obj}")?;
            }
            ExportFormat::SqlInsert => {
                let d = self.opts.dialect;
                let values = row.iter().map(|v| d.literal(v)).collect::<Vec<_>>().join(", ");
                self.pending_insert.push(format!("({values})"));
                if self.pending_insert.len() >= self.opts.rows_per_insert.max(1) {
                    self.flush_insert()?;
                }
            }
        }
        self.rows += 1;
        Ok(())
    }

    fn flush_insert(&mut self) -> Result<()> {
        if self.pending_insert.is_empty() {
            return Ok(());
        }
        let cols = self.columns.iter().map(|c| self.opts.dialect.quote_ident(c)).collect::<Vec<_>>().join(", ");
        writeln!(self.out, "INSERT INTO {} ({cols}) VALUES\n{};", self.opts.table, self.pending_insert.join(",\n"))?;
        self.pending_insert.clear();
        Ok(())
    }

    /// Finishes the document and returns the number of rows written.
    pub fn finish(mut self) -> Result<u64> {
        match self.opts.format {
            ExportFormat::Csv => {}
            ExportFormat::Json => self.out.write_all(if self.rows == 0 { b"]\n" } else { b"\n]\n" })?,
            ExportFormat::SqlInsert => self.flush_insert()?,
        }
        self.out.flush()?;
        Ok(self.rows)
    }
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) && !s.contains(['e', 'E', 'N', 'n']) => {
            s.to_string()
        }
        other => serde_json::to_string(&other.to_text().unwrap_or_default()).unwrap_or_else(|_| "null".into()),
    }
}

/// Convenience: export a whole in-memory result to bytes.
pub fn export_to_vec(opts: ExportOptions, columns: &[String], rows: &[Row]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut ex = Exporter::new(&mut buf, opts, columns.to_vec())?;
    ex.write_rows(rows.iter().map(|r| &r[..]))?;
    ex.finish()?;
    Ok(buf)
}

#[derive(Debug, Clone)]
pub struct CsvImportOptions {
    pub delimiter: u8,
    pub has_header: bool,
    pub empty_as_null: bool,
    pub rows_per_insert: usize,
}

impl Default for CsvImportOptions {
    fn default() -> Self {
        Self { delimiter: b',', has_header: true, empty_as_null: true, rows_per_insert: 500 }
    }
}

/// Converts CSV into batched INSERT statements for `table`. Header names are matched to
/// table columns case-insensitively; without a header, fields map to columns by position.
pub fn csv_to_inserts(
    dialect: Dialect,
    schema: &str,
    table: &str,
    columns: &[ColumnInfo],
    input: impl Read,
    opts: &CsvImportOptions,
) -> Result<(Vec<String>, u64)> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(opts.delimiter)
        .has_headers(opts.has_header)
        .flexible(false)
        .from_reader(input);
    let targets: Vec<&ColumnInfo> = if opts.has_header {
        let headers = rdr.headers()?.clone();
        headers
            .iter()
            .map(|h| {
                let h = h.trim().trim_start_matches('\u{feff}');
                columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(h))
                    .with_context(|| format!("CSV column \"{h}\" does not exist in {table}"))
            })
            .collect::<Result<_>>()?
    } else {
        columns.iter().collect()
    };
    let col_list = targets.iter().map(|c| dialect.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let target = dialect.qualified(schema, table);
    let mut statements = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut count = 0u64;
    let flush = |batch: &mut Vec<String>, statements: &mut Vec<String>| {
        if !batch.is_empty() {
            statements.push(format!("INSERT INTO {target} ({col_list}) VALUES {}", batch.join(", ")));
            batch.clear();
        }
    };
    for (line, record) in rdr.records().enumerate() {
        let record = record.with_context(|| format!("reading CSV record {}", line + 1))?;
        if record.len() > targets.len() {
            bail!("CSV record {} has {} fields but only {} columns are mapped", line + 1, record.len(), targets.len());
        }
        let values = record
            .iter()
            .zip(&targets)
            .map(|(field, col)| {
                let edit = if opts.empty_as_null && field.is_empty() {
                    CellEdit::Null
                } else {
                    CellEdit::Text(field.to_string())
                };
                match edit {
                    CellEdit::Text(t) => dialect.edit_literal(&col.data_type, &t),
                    _ => "NULL".to_string(),
                }
            })
            .collect::<Vec<_>>();
        let mut values = values;
        values.resize(targets.len(), "NULL".into());
        batch.push(format!("({})", values.join(", ")));
        count += 1;
        if batch.len() >= opts.rows_per_insert.max(1) {
            flush(&mut batch, &mut statements);
        }
    }
    flush(&mut batch, &mut statements);
    Ok((statements, count))
}

/// Splits an SQL script into individual statements for import.
pub fn sql_script_statements(script: &str, dialect: Dialect) -> Vec<String> {
    split_statements(script, dialect).iter().map(|s| s.text(script).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Row;

    fn sample() -> (Vec<String>, Vec<Row>) {
        (
            vec!["id".into(), "name".into(), "score".into(), "blob".into()],
            vec![
                vec![Value::Int(1), Value::text("Ada, \"the\" first"), Value::Decimal("10.50".into()), Value::Null]
                    .into(),
                vec![Value::Int(2), Value::Null, Value::Float(0.5), Value::Bytes(vec![1, 2].into())].into(),
            ],
        )
    }

    #[test]
    fn csv_export() {
        let (cols, rows) = sample();
        let out = export_to_vec(ExportOptions::default(), &cols, &rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "id,name,score,blob\n1,\"Ada, \"\"the\"\" first\",10.50,\n2,,0.5,0x0102\n"
        );
    }

    #[test]
    fn json_export() {
        let (cols, rows) = sample();
        let opts = ExportOptions { format: ExportFormat::Json, ..Default::default() };
        let out = String::from_utf8(export_to_vec(opts, &cols, &rows).unwrap()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed[0]["name"], "Ada, \"the\" first");
        assert_eq!(parsed[0]["score"], serde_json::json!(10.50));
        assert_eq!(parsed[1]["name"], serde_json::Value::Null);
        assert_eq!(parsed[1]["blob"], "0x0102");
        let empty =
            export_to_vec(ExportOptions { format: ExportFormat::Json, ..Default::default() }, &cols, &[]).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&empty).unwrap(), serde_json::json!([]));
    }

    #[test]
    fn sql_export() {
        let (cols, rows) = sample();
        let opts = ExportOptions {
            format: ExportFormat::SqlInsert,
            table: "`t`".into(),
            dialect: Dialect::MySql,
            rows_per_insert: 1,
            ..Default::default()
        };
        let out = String::from_utf8(export_to_vec(opts, &cols, &rows).unwrap()).unwrap();
        assert_eq!(
            out,
            "INSERT INTO `t` (`id`, `name`, `score`, `blob`) VALUES\n(1, 'Ada, \"the\" first', 10.50, NULL);\n\
             INSERT INTO `t` (`id`, `name`, `score`, `blob`) VALUES\n(2, NULL, 0.5, X'0102');\n"
        );
    }

    #[test]
    fn csv_import() {
        let cols = vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), ..Default::default() },
            ColumnInfo { name: "Name".into(), data_type: "text".into(), ..Default::default() },
        ];
        let csv = "name,ID\n\"O'Hara\",1\n,2\n";
        let (stmts, n) =
            csv_to_inserts(Dialect::Postgres, "public", "p", &cols, csv.as_bytes(), &CsvImportOptions::default())
                .unwrap();
        assert_eq!(n, 2);
        assert_eq!(stmts, vec!["INSERT INTO \"public\".\"p\" (\"Name\", \"id\") VALUES ('O''Hara', '1'), (NULL, '2')"]);
        let bad = csv_to_inserts(
            Dialect::Postgres,
            "public",
            "p",
            &cols,
            "nope\n1\n".as_bytes(),
            &CsvImportOptions::default(),
        );
        assert!(bad.unwrap_err().to_string().contains("nope"));
    }
}
