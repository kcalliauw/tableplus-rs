//! Running statements and reading their rows.

use anyhow::Result;
use tableplus_core::sqltext::{split_statements, trim_statement};
use tableplus_core::{ColumnMeta, Dialect, ResultSet, Row, StatementResult};

use crate::raw::{Conn, Prepared, QueryResult, ResultKind, StatementType};
use crate::values::Columns;

/// Where a statement's rows come from.
enum Rows<'r, 's> {
    /// Read chunk by chunk, with cast columns already VARCHAR.
    Chunks(&'r mut QueryResult<'s>),
    /// A materialized result that still holds types needing text, read cell by cell.
    Cells(&'r mut QueryResult<'s>),
}

impl Rows<'_, '_> {
    /// Hands rows to `take` until it returns `false`, `max` rows were read, or the rows run out.
    fn each(&mut self, columns: &Columns, max: usize, mut take: impl FnMut(Vec<Row>) -> Result<bool>) -> Result<()> {
        let mut read = 0usize;
        match self {
            Rows::Chunks(result) => {
                while read < max {
                    let Some(chunk) = result.next_chunk()? else { break };
                    let mut rows = Vec::with_capacity(chunk.len());
                    columns.read_chunk(&chunk, &mut rows, max - read);
                    read += rows.len();
                    if !take(rows)? {
                        break;
                    }
                }
            }
            Rows::Cells(result) => {
                let mut rows = Vec::new();
                columns.read_cells(&mut result.cells(), &mut rows, max);
                take(rows)?;
            }
        }
        Ok(())
    }
}

/// What the last statement of a text produced.
enum Outcome<'r, 's> {
    Rows(Columns, Rows<'r, 's>),
    Affected(u64),
}

/// The last statement's own text, when the app's splitter and DuckDB agree on the count.
fn last_statement(sql: &str, count: usize) -> Option<&str> {
    let spans = split_statements(sql, Dialect::DuckDb);
    (spans.len() == count).then(|| spans.last().map(|s| trim_statement(s.text(sql)))).flatten()
}

/// Runs every statement in `sql` and hands the last one's outcome to `f`.
fn with_last<T>(conn: &Conn, sql: &str, f: impl FnOnce(Outcome<'_, '_>) -> Result<T>) -> Result<T> {
    let extracted = conn.extract(sql)?;
    let Some(last) = extracted.len().checked_sub(1) else { return f(Outcome::Affected(0)) };
    for i in 0..last {
        extracted.prepare(i)?.materialize()?;
    }
    let mut stmt = extracted.prepare(last)?;
    let columns = Columns::new(&stmt.columns());
    if stmt.statement_type() == StatementType::Select {
        if !columns.needs_cast() {
            let mut result = stmt.stream()?;
            return f(Outcome::Rows(columns, Rows::Chunks(&mut result)));
        }
        // PRAGMA and a few other SELECT-typed statements can't be a subquery; they fall through.
        let wrapped = last_statement(sql, extracted.len()).and_then(|text| conn.prepare(&columns.wrap(text)).ok());
        if let Some(mut wrapped) = wrapped {
            let mut result = wrapped.stream()?;
            return f(Outcome::Rows(columns, Rows::Chunks(&mut result)));
        }
    }
    materialized(&mut stmt, columns, f)
}

fn materialized<T>(
    stmt: &mut Prepared<'_>,
    columns: Columns,
    f: impl FnOnce(Outcome<'_, '_>) -> Result<T>,
) -> Result<T> {
    let mut result = stmt.materialize()?;
    match result.kind() {
        ResultKind::Changed(n) => f(Outcome::Affected(n)),
        ResultKind::Nothing => f(Outcome::Affected(0)),
        ResultKind::Rows => {
            let columns = if columns.meta.is_empty() { Columns::new(&result.columns()) } else { columns };
            let rows = if columns.needs_cast() { Rows::Cells(&mut result) } else { Rows::Chunks(&mut result) };
            f(Outcome::Rows(columns, rows))
        }
    }
}

/// Runs `sql` and reads at most `max_rows` rows of the last statement.
pub fn collect(conn: &Conn, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
    with_last(conn, sql, |outcome| match outcome {
        Outcome::Affected(n) => Ok(StatementResult::Affected(n)),
        Outcome::Rows(columns, mut rows) => {
            let limit = max_rows.unwrap_or(usize::MAX);
            let mut all: Vec<Row> = Vec::new();
            rows.each(&columns, limit.saturating_add(1), |mut chunk| {
                all.append(&mut chunk);
                Ok(true)
            })?;
            let truncated = all.len() > limit;
            all.truncate(limit);
            Ok(StatementResult::Rows(ResultSet { columns: columns.meta, rows: all, truncated }))
        }
    })
}

/// Receives a batch of rows; returns `false` to stop the statement.
pub type Emit<'a> = dyn FnMut(&[ColumnMeta], Vec<Row>) -> Result<bool> + 'a;

/// Runs `sql` and hands the last statement's rows to `emit` in batches of `batch`.
pub fn batches(conn: &Conn, sql: &str, batch: usize, emit: &mut Emit<'_>) -> Result<u64> {
    let batch = batch.max(1);
    with_last(conn, sql, |outcome| {
        let Outcome::Rows(columns, mut rows) = outcome else { return Ok(0) };
        let mut buf: Vec<Row> = Vec::with_capacity(batch);
        let mut total = 0u64;
        let mut open = true;
        rows.each(&columns, usize::MAX, |chunk| {
            total += chunk.len() as u64;
            for row in chunk {
                buf.push(row);
                if buf.len() >= batch {
                    open = emit(&columns.meta, std::mem::replace(&mut buf, Vec::with_capacity(batch)))?;
                    if !open {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        })?;
        if open {
            emit(&columns.meta, buf)?;
        }
        Ok(total)
    })
}

/// The number of statements in `sql`, counted by DuckDB's parser without running any.
pub fn statement_count(conn: &Conn, sql: &str) -> Result<usize> {
    Ok(conn.extract(sql)?.len())
}
