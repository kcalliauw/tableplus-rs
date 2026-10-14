//! SQLite via `rusqlite`; blocking calls run on tokio's blocking pool.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{InterruptHandle, OpenFlags};

use super::{BatchSink, Connection};
use crate::alter::{ColumnDef, create_table_sql};
use crate::config::ConnectionConfig;
use crate::dialect::Dialect;
use crate::model::{
    ColumnInfo, ColumnMeta, ConstraintInfo, ConstraintKind, IndexInfo, ObjectKind, ResultSet, Row, SchemaObject,
    StatementResult, TableStructure,
};
use crate::value::Value;

const D: Dialect = Dialect::Sqlite;

pub struct SqliteConnection {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: InterruptHandle,
    path: String,
    version: String,
}

fn value_from(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into()),
        ValueRef::Blob(b) => Value::Bytes(b.into()),
    }
}

type Cells = Vec<Vec<Option<String>>>;

fn meta_rows(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<Cells> {
    let mut stmt = conn.prepare(sql)?;
    let n = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push((0..n).map(|i| value_from(r.get_ref(i).unwrap_or(ValueRef::Null)).to_text()).collect());
    }
    Ok(out)
}

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

impl SqliteConnection {
    pub async fn open(config: &ConnectionConfig) -> Result<Self> {
        let path = crate::tunnel::expand_tilde(config.path.trim());
        if config.path.trim().is_empty() {
            bail!("No SQLite database file selected");
        }
        let display = path.display().to_string();
        tokio::task::spawn_blocking(move || {
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI;
            let conn = rusqlite::Connection::open_with_flags(&path, flags)
                .with_context(|| format!("opening {}", path.display()))?;
            conn.busy_timeout(Duration::from_secs(5))?;
            let version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
            Ok(Self {
                interrupt: conn.get_interrupt_handle(),
                conn: Arc::new(Mutex::new(conn)),
                path: display,
                version,
            })
        })
        .await?
    }

    async fn with_conn<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| anyhow!("SQLite connection poisoned"))?;
            f(&mut guard)
        })
        .await?
    }

    async fn meta(&self, sql: String) -> Result<Cells> {
        self.with_conn(move |c| Ok(meta_rows(c, &sql)?)).await
    }
}

#[async_trait]
impl Connection for SqliteConnection {
    fn dialect(&self) -> Dialect {
        D
    }

    fn server_version(&self) -> String {
        format!("SQLite {}", self.version)
    }

    fn database(&self) -> String {
        std::path::Path::new(&self.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn default_schema(&self) -> String {
        "main".into()
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.list_schemas().await
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let rows = self.meta("PRAGMA database_list".into()).await?;
        Ok(rows.iter().map(|r| s(&r[1])).filter(|n| n != "temp").collect())
    }

    async fn list_objects(&self) -> Result<Vec<SchemaObject>> {
        let mut out = Vec::new();
        for schema in self.list_schemas().await? {
            let rows = self
                .meta(format!(
                    "SELECT name, type FROM {}.sqlite_master WHERE type IN ('table','view') \
                     AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
                    D.quote_ident(&schema)
                ))
                .await?;
            for r in rows {
                let kind = if r[1].as_deref() == Some("view") { ObjectKind::View } else { ObjectKind::Table };
                out.push(SchemaObject::new(schema.clone(), s(&r[0]), kind));
            }
        }
        Ok(out)
    }

    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let sch = D.quote_ident(if schema.is_empty() { "main" } else { schema });
        let t = D.quote_str(table);
        let table_name = table.to_string();
        let (cols, create_sql, idx_list, fks) = {
            let sch2 = sch.clone();
            let t2 = t.clone();
            self.with_conn(move |c| {
                let cols = meta_rows(c, &format!("SELECT * FROM pragma_table_info({t2}, {})", D.quote_str(sch2.trim_matches('"'))))?;
                let create = meta_rows(c, &format!("SELECT sql FROM {sch2}.sqlite_master WHERE name = {t2}"))?;
                let idx = meta_rows(c, &format!("SELECT name, \"unique\", origin FROM pragma_index_list({t2}, {})", D.quote_str(sch2.trim_matches('"'))))?;
                let fks = meta_rows(
                    c,
                    &format!(
                        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list({t2}, {}) ORDER BY id, seq",
                        D.quote_str(sch2.trim_matches('"'))
                    ),
                )?;
                Ok((cols, create, idx, fks))
            })
            .await?
        };
        if cols.is_empty() {
            bail!("{table_name} has no columns or does not exist");
        }
        let create_sql = create_sql.first().and_then(|r| r[0].clone()).unwrap_or_default();
        let autoincrement = create_sql.to_ascii_uppercase().contains("AUTOINCREMENT");
        // cid, name, type, notnull, dflt_value, pk
        let pk_count = cols.iter().filter(|r| r[5].as_deref().unwrap_or("0") != "0").count();
        let columns: Vec<ColumnInfo> = cols
            .iter()
            .map(|r| {
                let pk = r[5].as_deref().unwrap_or("0") != "0";
                let ty = s(&r[2]);
                let rowid_alias = pk && pk_count == 1 && ty.eq_ignore_ascii_case("INTEGER");
                ColumnInfo {
                    name: s(&r[1]),
                    data_type: ty,
                    nullable: r[3].as_deref() == Some("0") && !pk,
                    default: r[4].clone(),
                    primary_key: pk,
                    extra: rowid_alias.then(|| if autoincrement { "AUTOINCREMENT" } else { "ROWID" }.to_string()),
                    comment: None,
                }
            })
            .collect();
        let mut indexes = Vec::new();
        let mut constraints = Vec::new();
        let pk_cols: Vec<String> = {
            let mut v: Vec<(i64, String)> = cols
                .iter()
                .filter_map(|r| {
                    let n: i64 = r[5].as_deref().unwrap_or("0").parse().unwrap_or(0);
                    (n > 0).then(|| (n, s(&r[1])))
                })
                .collect();
            v.sort();
            v.into_iter().map(|(_, n)| n).collect()
        };
        if !pk_cols.is_empty() {
            let list = pk_cols.iter().map(|c| D.quote_ident(c)).collect::<Vec<_>>().join(", ");
            constraints.push(ConstraintInfo {
                name: "PRIMARY".into(),
                kind: ConstraintKind::PrimaryKey,
                columns: pk_cols.clone(),
                definition: format!("PRIMARY KEY ({list})"),
            });
        }
        for r in idx_list {
            let name = s(&r[0]);
            let origin = s(&r[2]);
            let n2 = D.quote_str(&name);
            let sch2 = sch.clone();
            let (icols, def) = self
                .with_conn(move |c| {
                    let icols = meta_rows(
                        c,
                        &format!(
                            "SELECT name FROM pragma_index_info({n2}, {}) ORDER BY seqno",
                            D.quote_str(sch2.trim_matches('"'))
                        ),
                    )?;
                    let def = meta_rows(
                        c,
                        &format!("SELECT sql FROM {sch2}.sqlite_master WHERE type = 'index' AND name = {n2}"),
                    )?;
                    Ok((icols, def))
                })
                .await?;
            let columns: Vec<String> =
                icols.iter().map(|r| r[0].clone().unwrap_or_else(|| "(expression)".into())).collect();
            let unique = r[1].as_deref() == Some("1");
            if origin == "u" {
                let list = columns.iter().map(|c| D.quote_ident(c)).collect::<Vec<_>>().join(", ");
                constraints.push(ConstraintInfo {
                    name: name.clone(),
                    kind: ConstraintKind::Unique,
                    columns: columns.clone(),
                    definition: format!("UNIQUE ({list})"),
                });
            }
            indexes.push(IndexInfo {
                primary: origin == "pk",
                unique,
                method: None,
                definition: def.first().and_then(|r| r[0].clone()),
                columns,
                name,
            });
        }
        let mut fk_groups: Vec<(String, ConstraintInfo, Vec<String>, String, String, String)> = Vec::new();
        for r in fks {
            let id = s(&r[0]);
            match fk_groups.iter_mut().find(|g| g.0 == id) {
                Some(g) => {
                    g.1.columns.push(s(&r[2]));
                    g.2.push(s(&r[3]));
                }
                None => fk_groups.push((
                    id.clone(),
                    ConstraintInfo {
                        name: format!("fk_{table}_{id}"),
                        kind: ConstraintKind::ForeignKey,
                        columns: vec![s(&r[2])],
                        definition: String::new(),
                    },
                    vec![s(&r[3])],
                    s(&r[1]),
                    s(&r[4]),
                    s(&r[5]),
                )),
            }
        }
        for (_, mut c, to, parent, on_update, on_delete) in fk_groups {
            let refs = to.iter().map(|x| D.quote_ident(x)).collect::<Vec<_>>().join(", ");
            let mut d = format!("REFERENCES {} ({refs})", D.quote_ident(&parent));
            if on_update != "NO ACTION" && !on_update.is_empty() {
                d.push_str(&format!(" ON UPDATE {on_update}"));
            }
            if on_delete != "NO ACTION" && !on_delete.is_empty() {
                d.push_str(&format!(" ON DELETE {on_delete}"));
            }
            c.definition = d;
            constraints.push(c);
        }
        // CHECK constraints are only available in the CREATE statement.
        for (i, check) in extract_checks(&create_sql).into_iter().enumerate() {
            constraints.push(ConstraintInfo {
                name: format!("check_{}", i + 1),
                kind: ConstraintKind::Check,
                columns: Vec::new(),
                definition: check,
            });
        }
        Ok(TableStructure { columns, indexes, constraints, comment: None })
    }

    async fn object_definition(&self, o: &SchemaObject) -> Result<String> {
        let sch = D.quote_ident(if o.schema.is_empty() { "main" } else { &o.schema });
        let rows =
            self.meta(format!("SELECT sql FROM {sch}.sqlite_master WHERE name = {}", D.quote_str(&o.name))).await?;
        match rows.into_iter().next().and_then(|r| r[0].clone()) {
            Some(sql) => Ok(sql),
            None if o.kind == ObjectKind::Table => {
                let st = self.table_structure(&o.schema, &o.name).await?;
                let cols: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();
                Ok(create_table_sql(D, &o.schema, &o.name, &cols, &[]))
            }
            None => Err(anyhow!("definition not found")),
        }
    }

    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        let sql = sql.to_string();
        self.with_conn(move |c| {
            let mut stmt = c.prepare(&sql)?;
            if stmt.column_count() == 0 {
                let n = stmt.execute([])?;
                return Ok(StatementResult::Affected(n as u64));
            }
            let columns: Vec<ColumnMeta> =
                stmt.columns().iter().map(|c| ColumnMeta::new(c.name(), c.decl_type().unwrap_or_default())).collect();
            let n = columns.len();
            let limit = max_rows.unwrap_or(usize::MAX);
            let mut rows: Vec<Row> = Vec::new();
            let mut truncated = false;
            let mut q = stmt.query([])?;
            while let Some(r) = q.next()? {
                if rows.len() >= limit {
                    truncated = true;
                    break;
                }
                rows.push((0..n).map(|i| value_from(r.get_ref(i).unwrap_or(ValueRef::Null))).collect());
            }
            Ok(StatementResult::Rows(ResultSet { columns, rows, truncated }))
        })
        .await
    }

    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64> {
        // The blocking reader hands batches to the async side over a bounded channel, so
        // memory stays at a few batches regardless of result size.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(Vec<ColumnMeta>, Vec<Row>)>(2);
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let reader = tokio::task::spawn_blocking(move || -> Result<u64> {
            let guard = conn.lock().map_err(|_| anyhow!("SQLite connection poisoned"))?;
            let mut stmt = guard.prepare(&sql)?;
            let columns: Vec<ColumnMeta> =
                stmt.columns().iter().map(|c| ColumnMeta::new(c.name(), c.decl_type().unwrap_or_default())).collect();
            let n = columns.len();
            let mut q = stmt.query([])?;
            let mut buf: Vec<Row> = Vec::with_capacity(batch);
            let mut total = 0;
            while let Some(r) = q.next()? {
                buf.push((0..n).map(|i| value_from(r.get_ref(i).unwrap_or(ValueRef::Null))).collect());
                total += 1;
                if buf.len() >= batch && tx.blocking_send((columns.clone(), std::mem::take(&mut buf))).is_err() {
                    return Ok(total); // receiver gone (sink failed)
                }
            }
            let _ = tx.blocking_send((columns, buf));
            Ok(total)
        });
        let mut sink_error = None;
        while let Some((cols, rows)) = rx.recv().await {
            if sink_error.is_none()
                && let Err(e) = sink(&cols, rows)
            {
                sink_error = Some(e);
                rx.close();
            }
        }
        let total = reader.await??;
        match sink_error {
            Some(e) => Err(e),
            None => Ok(total),
        }
    }

    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |c| {
            let tx = c.transaction()?;
            let mut total = 0u64;
            for (i, stmt) in statements.iter().enumerate() {
                let before = tx.total_changes();
                tx.execute_batch(stmt).with_context(|| format!("statement {} failed: {stmt}", i + 1))?;
                total += tx.total_changes().saturating_sub(before);
            }
            tx.commit()?;
            Ok(total)
        })
        .await
    }

    async fn cancel(&self) -> Result<()> {
        self.interrupt.interrupt();
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        self.meta("SELECT 1".into()).await.map(|_| ())
    }
}

/// Pulls `CHECK (…)` clauses out of a CREATE TABLE statement (balanced parentheses).
fn extract_checks(sql: &str) -> Vec<String> {
    let upper = sql.to_ascii_uppercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = upper[from..].find("CHECK") {
        let start = from + pos;
        from = start + 5;
        let Some(open) = sql[from..].find('(').map(|p| from + p) else {
            break;
        };
        if !sql[from..open].trim().is_empty() {
            continue;
        }
        let mut depth = 0;
        let mut end = None;
        for (i, ch) in sql[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end {
            out.push(format!("CHECK {}", &sql[open..end]));
            from = end;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks() {
        assert_eq!(
            extract_checks("CREATE TABLE t (a int CHECK (a > 0), b int, CONSTRAINT c CHECK ((b + 1) < 10))"),
            vec!["CHECK (a > 0)", "CHECK ((b + 1) < 10)"]
        );
        assert!(extract_checks("CREATE TABLE checks (x int)").is_empty());
    }
}
