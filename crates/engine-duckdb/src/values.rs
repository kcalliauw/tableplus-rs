//! How DuckDB values become cells. Numbers, text and blobs are read from DuckDB's vectors as they
//! are. Every other type is cast to VARCHAR by DuckDB itself, by wrapping the user's statement,
//! so the grid shows exactly what DuckDB's own clients print.

use libduckdb_sys as ffi;
use tableplus_core::{ColumnMeta, Row, Value};

use crate::raw::{Cells, Chunk, LogicalType};

/// How one result column reaches the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Varchar,
    Blob,
    /// Cast to VARCHAR, shown as an exact number (DECIMAL, HUGEINT, UHUGEINT, BIGNUM).
    CastNumber,
    /// Cast to VARCHAR, shown as text (dates, intervals, lists, structs, enums, UUIDs, …).
    CastText,
}

impl Shape {
    pub fn of(id: ffi::DUCKDB_TYPE) -> Self {
        match id {
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN => Shape::Bool,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT => Shape::I8,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT => Shape::I16,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER => Shape::I32,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT => Shape::I64,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT => Shape::U8,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT => Shape::U16,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER => Shape::U32,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT => Shape::U64,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT => Shape::F32,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE => Shape::F64,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR => Shape::Varchar,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB => Shape::Blob,
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL
            | ffi::DUCKDB_TYPE_DUCKDB_TYPE_HUGEINT
            | ffi::DUCKDB_TYPE_DUCKDB_TYPE_UHUGEINT
            | ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGNUM => Shape::CastNumber,
            _ => Shape::CastText,
        }
    }

    pub fn is_cast(self) -> bool {
        matches!(self, Shape::CastNumber | Shape::CastText)
    }
}

/// A result's columns: what the app sees, and how each is read.
pub struct Columns {
    pub meta: Vec<ColumnMeta>,
    pub shapes: Vec<Shape>,
}

impl Columns {
    pub fn new(columns: &[(String, LogicalType)]) -> Self {
        Self {
            meta: columns.iter().map(|(name, ty)| ColumnMeta::new(name.clone(), ty.name())).collect(),
            shapes: columns.iter().map(|(_, ty)| Shape::of(ty.id())).collect(),
        }
    }

    pub fn needs_cast(&self) -> bool {
        self.shapes.iter().any(|s| s.is_cast())
    }

    /// `sql` with every cast column turned into DuckDB's VARCHAR rendering. Positional
    /// references keep duplicate column names and the inner statement's row order.
    pub fn wrap(&self, sql: &str) -> String {
        let list: Vec<String> = self
            .meta
            .iter()
            .zip(&self.shapes)
            .enumerate()
            .map(|(i, (meta, shape))| {
                let name = tableplus_core::Dialect::DuckDb.quote_ident(&meta.name);
                if shape.is_cast() {
                    format!("CAST(#{} AS VARCHAR) AS {name}", i + 1)
                } else {
                    format!("#{} AS {name}", i + 1)
                }
            })
            .collect();
        format!("SELECT {} FROM (\n{sql}\n)", list.join(", "))
    }

    /// The rows of a chunk whose cast columns are already VARCHAR.
    pub fn read_chunk(&self, chunk: &Chunk<'_>, out: &mut Vec<Row>, limit: usize) {
        let columns: Vec<_> = (0..self.shapes.len()).map(|i| chunk.column(i)).collect();
        for row in 0..chunk.len().min(limit) {
            let cells = columns.iter().zip(&self.shapes).map(|(col, shape)| {
                if !col.is_valid(row) {
                    return Value::Null;
                }
                // SAFETY: each shape names the physical type DuckDB stores for that column, and
                // cast columns arrive as VARCHAR.
                unsafe {
                    match shape {
                        Shape::Bool => Value::Bool(col.get::<u8>(row) != 0),
                        Shape::I8 => Value::Int(col.get::<i8>(row).into()),
                        Shape::I16 => Value::Int(col.get::<i16>(row).into()),
                        Shape::I32 => Value::Int(col.get::<i32>(row).into()),
                        Shape::I64 => Value::Int(col.get::<i64>(row)),
                        Shape::U8 => Value::UInt(col.get::<u8>(row).into()),
                        Shape::U16 => Value::UInt(col.get::<u16>(row).into()),
                        Shape::U32 => Value::UInt(col.get::<u32>(row).into()),
                        Shape::U64 => Value::UInt(col.get::<u64>(row)),
                        Shape::F32 => Value::Float(col.get::<f32>(row).into()),
                        Shape::F64 => Value::Float(col.get::<f64>(row)),
                        Shape::Blob => Value::Bytes(col.bytes(row).into()),
                        Shape::Varchar | Shape::CastText => Value::Text(String::from_utf8_lossy(col.bytes(row)).into()),
                        Shape::CastNumber => Value::Decimal(String::from_utf8_lossy(col.bytes(row)).into()),
                    }
                }
            });
            out.push(cells.collect());
        }
    }

    /// The rows of a materialized result read cell by cell, for statements that can't be wrapped.
    /// A type the old value functions can't render shows its type name instead.
    pub fn read_cells(&self, cells: &mut Cells<'_>, out: &mut Vec<Row>, limit: usize) {
        let rows = cells.rows().min(limit as u64);
        for row in 0..rows {
            let values = (0..self.shapes.len()).map(|c| {
                let col = c as u64;
                if cells.is_null(col, row) {
                    return Value::Null;
                }
                if self.shapes[c] == Shape::Blob {
                    return Value::Bytes(cells.blob(col, row).into());
                }
                let Some(text) = cells.text(col, row) else {
                    return Value::Text(format!("({})", self.meta[c].type_name).into());
                };
                parse_cell(self.shapes[c], text)
            });
            out.push(values.collect());
        }
    }
}

fn parse_cell(shape: Shape, text: String) -> Value {
    let int = |t: &str| t.parse::<i64>().map(Value::Int).ok();
    let parsed = match shape {
        Shape::Bool => Some(Value::Bool(text == "true")),
        Shape::I8 | Shape::I16 | Shape::I32 | Shape::I64 => int(&text),
        Shape::U8 | Shape::U16 | Shape::U32 | Shape::U64 => text.parse::<u64>().map(Value::UInt).ok(),
        Shape::F32 | Shape::F64 => text.parse::<f64>().map(Value::Float).ok(),
        Shape::CastNumber => Some(Value::Decimal(text.clone().into())),
        Shape::Varchar | Shape::Blob | Shape::CastText => None,
    };
    parsed.unwrap_or_else(|| Value::Text(text.into()))
}
