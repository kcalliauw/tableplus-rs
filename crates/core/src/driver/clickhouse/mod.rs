//! ClickHouse over its HTTP interface (port 8123, or 8443 with TLS). Results come back as
//! `TabSeparatedWithNamesAndTypes`, so every value is the server's own text rendering.
//!
//! All statements of a connection share one HTTP session, which keeps `SET` and temporary tables
//! between them. The server refuses a second request in a session while one runs, so requests
//! take turns through [`Session`].

pub mod tsv;

use std::net::IpAddr;
use std::ops::ControlFlow;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::FutureExt;
use reqwest::header::HeaderMap;
use reqwest::{Response, StatusCode, Url};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

use self::tsv::{TsvReader, ValueKind};
use super::{BatchSink, Connection, Explained, explain_with};
use crate::config::{ConnectionConfig, Credentials, SslMode};
use crate::dialect::Dialect;
use crate::model::{
    ColumnInfo, ColumnMeta, IndexInfo, ObjectKind, ResultSet, Row, SchemaObject, StatementResult, TableStructure,
};
use crate::plan::ExplainStatements;
use crate::server::{Flavor, ServerInfo};
use crate::tunnel::{SshTunnel, open_tunnel};

const D: Dialect = Dialect::ClickHouse;
const FORMAT: &str = "TabSeparatedWithNamesAndTypes";
const SYSTEM_DATABASES: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";
/// The longest the server lets a session idle (its default `max_session_timeout`).
const SESSION_TIMEOUT_SECS: &str = "3600";
/// `SESSION_IS_LOCKED`: the session still runs a statement whose reader went away.
const SESSION_IS_LOCKED: &str = "373";
/// `SESSION_NOT_FOUND`: the server ended the session after it sat idle too long.
const SESSION_NOT_FOUND: &str = "372";
const ROWS_PER_BATCH: usize = 1024;

pub struct ChConnection {
    http: reqwest::Client,
    url: Url,
    user: String,
    password: String,
    turn: Mutex<SessionState>,
    /// The `query_id` of the statement the session is running, for [`Connection::cancel`].
    running: std::sync::Mutex<Option<String>>,
    version: String,
    database: String,
    _tunnel: Option<SshTunnel>,
}

/// The HTTP session the connection's statements share.
struct SessionState {
    id: String,
    /// Whether the server has created it. From then on requests ask the server to refuse an
    /// unknown id instead of silently starting a fresh session in the default database.
    started: bool,
}

impl SessionState {
    fn new() -> Self {
        Self { id: Uuid::new_v4().to_string(), started: false }
    }
}

/// What a statement returned.
enum Output {
    /// Rows, which went to the sink.
    Rows,
    /// No result; the rows it wrote, from the `X-ClickHouse-Summary` header.
    Written(u64),
}

type RowSink<'a> = dyn FnMut(&[ColumnMeta], Vec<Row>) -> Result<ControlFlow<()>> + Send + 'a;

/// The response body: TabSeparated rows, or, when the statement names its own `FORMAT`, the
/// server's text line by line.
enum Body {
    Tsv(TsvReader),
    Lines { columns: Vec<ColumnMeta>, partial: Vec<u8> },
}

impl Body {
    fn for_format(format: &str) -> Self {
        if format == FORMAT {
            Body::Tsv(TsvReader::default())
        } else {
            Body::Lines { columns: vec![ColumnMeta::new(format, "String")], partial: Vec::new() }
        }
    }

    fn columns(&self) -> Option<&[ColumnMeta]> {
        match self {
            Body::Tsv(reader) => reader.columns(),
            Body::Lines { columns, .. } => Some(columns),
        }
    }

    fn push(&mut self, chunk: &[u8], rows: &mut Vec<Row>) {
        match self {
            Body::Tsv(reader) => reader.push(chunk, rows),
            Body::Lines { partial, .. } => {
                partial.extend_from_slice(chunk);
                if let Some(last) = partial.iter().rposition(|b| *b == b'\n') {
                    let complete: Vec<u8> = partial.drain(..=last).collect();
                    rows.extend(complete[..last].split(|b| *b == b'\n').map(line_row));
                }
            }
        }
    }

    fn exception(&self) -> Option<String> {
        match self {
            Body::Tsv(reader) => reader.exception(),
            Body::Lines { .. } => None,
        }
    }

    fn finish(self, rows: &mut Vec<Row>) -> Result<()> {
        match self {
            Body::Tsv(reader) => reader.finish().map_err(|e| anyhow!(e)),
            Body::Lines { partial, .. } => {
                if !partial.is_empty() {
                    rows.push(line_row(&partial));
                }
                Ok(())
            }
        }
    }
}

fn line_row(line: &[u8]) -> Row {
    Box::new([ValueKind::Text.value(Some(line.to_vec()))])
}

/// The rows a statement wrote, from the `X-ClickHouse-Summary` header.
fn written_rows(headers: &HeaderMap) -> u64 {
    headers
        .get("X-ClickHouse-Summary")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| serde_json::from_str::<serde_json::Value>(h).ok())
        .and_then(|summary| summary.get("written_rows")?.as_str()?.parse().ok())
        .unwrap_or(0)
}

/// The column names in a sorting or primary key such as `user_id, toDate(created_at), id`, in
/// key order. Expressions that aren't a plain column are left out.
fn key_columns(key: &str, columns: &[ColumnInfo]) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0usize, 0);
    for (i, c) in key.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&key[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&key[start..]);
    parts
        .into_iter()
        .map(|p| p.trim().trim_matches('`'))
        .filter(|p| columns.iter().any(|c| c.name == *p))
        .map(str::to_string)
        .collect()
}

impl ChConnection {
    pub async fn connect(config: &ConnectionConfig, creds: &Credentials) -> Result<Self> {
        let tunnel = if config.ssh.enabled {
            Some(open_tunnel(&config.ssh, creds, &config.host, config.port).await?)
        } else {
            None
        };
        let tls = config.tls.mode != SslMode::Disable;
        let conn = match Self::open(config, creds, tunnel.as_ref(), tls).await {
            // "Prefer" falls back to plain HTTP when the port doesn't speak TLS.
            Err(e)
                if tls
                    && config.tls.mode == SslMode::Prefer
                    && e.downcast_ref::<reqwest::Error>().is_some_and(reqwest::Error::is_connect) =>
            {
                tracing::info!("HTTPS connection failed ({e:#}), retrying over HTTP");
                Self::open(config, creds, tunnel.as_ref(), false).await?
            }
            other => other?,
        };
        Ok(Self { _tunnel: tunnel, ..conn })
    }

    async fn open(
        config: &ConnectionConfig,
        creds: &Credentials,
        tunnel: Option<&SshTunnel>,
        tls: bool,
    ) -> Result<Self> {
        let mut http = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).no_proxy();
        if tls {
            http =
                http.tls_backend_preconfigured(super::postgres::tls_connector(config.tls.mode, &config.tls.ca_cert)?);
        }
        // Through a tunnel over TLS, the URL keeps the host name the certificate names and the
        // name resolves to the tunnel.
        let (host, port) = match tunnel {
            None => (config.host.clone(), config.port),
            Some(t) if !tls || config.host.parse::<IpAddr>().is_ok() => ("127.0.0.1".into(), t.local_addr.port()),
            Some(t) => {
                http = http.resolve(&config.host, t.local_addr);
                (config.host.clone(), t.local_addr.port())
            }
        };
        let host = if host.contains(':') { format!("[{host}]") } else { host };
        let scheme = if tls { "https" } else { "http" };
        let url = Url::parse(&format!("{scheme}://{host}:{port}/")).context("invalid host")?;
        let mut conn = Self {
            http: http.build()?,
            url,
            user: config.user.clone(),
            password: creds.password.clone().unwrap_or_default(),
            turn: Mutex::new(SessionState::new()),
            running: std::sync::Mutex::new(None),
            version: String::new(),
            database: String::new(),
            _tunnel: None,
        };
        let probe = async {
            let mut session = conn.session().await;
            // The session keeps a USE; a `database` parameter would only last one request.
            if !config.database.is_empty() {
                session.query(&format!("USE {}", D.quote_ident(&config.database)), None).await?;
            }
            session.rows("SELECT version(), currentDatabase()").await
        };
        let rows = match tokio::time::timeout(Duration::from_secs(20), probe).await {
            Err(_) => bail!("connection to {}:{} timed out", config.host, config.port),
            Ok(rows) => rows?,
        };
        let text = |i: usize| rows.first().and_then(|r| r.get(i)).and_then(|v| v.to_text()).unwrap_or_default();
        conn.version = text(0);
        conn.database = text(1);
        Ok(conn)
    }

    async fn session(&self) -> Session<'_> {
        Session { conn: self, state: self.turn.lock().await }
    }

    fn post(&self, sql: &str, query_id: &str, session: Option<&SessionState>) -> reqwest::RequestBuilder {
        let mut url = self.url.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("query_id", query_id).append_pair("default_format", FORMAT);
            if let Some(session) = session {
                q.append_pair("session_id", &session.id).append_pair("session_timeout", SESSION_TIMEOUT_SECS);
                if session.started {
                    q.append_pair("session_check", "1");
                }
            }
        }
        self.http
            .post(url)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql.to_string())
    }

    /// Stops a statement from outside its session. `wait` returns only once it has stopped.
    async fn kill(&self, query_id: &str, wait: bool) -> Result<()> {
        let mode = if wait { "SYNC" } else { "ASYNC" };
        let sql = format!("KILL QUERY WHERE query_id = {} {mode}", D.quote_str(query_id));
        let response = self.post(&sql, &Uuid::new_v4().to_string(), None).send().await?;
        checked(response).await?.bytes().await?;
        Ok(())
    }

    /// Reads one metadata query as text cells.
    async fn meta(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let rows = self.session().await.rows(sql).await?;
        Ok(rows.into_iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect())
    }
}

/// The response, or the server's error when the status isn't 200.
async fn checked(response: Response) -> Result<Response> {
    if response.status() == StatusCode::OK {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let text = text.trim();
    if text.is_empty() { bail!("HTTP {status}") } else { bail!("{text}") }
}

/// The connection's turn at its HTTP session.
struct Session<'c> {
    conn: &'c ChConnection,
    state: MutexGuard<'c, SessionState>,
}

impl Session<'_> {
    /// Runs `sql`, handing its rows to `sink` in batches of up to `batch`. The sink always hears
    /// of the columns, with an empty batch when there are no rows. When the sink stops or fails,
    /// the statement is killed so the session is free for the next one. A user without the right
    /// to kill it still gets its rows; the server stops it once it can't write to the closed
    /// connection.
    async fn run(&mut self, sql: &str, batch: usize, sink: &mut RowSink<'_>) -> Result<Output> {
        let query_id = Uuid::new_v4().to_string();
        *self.conn.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(query_id.clone());
        let mut finished = false;
        let outcome = self.stream(sql, &query_id, batch, sink, &mut finished).await;
        *self.conn.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if !finished && let Err(e) = self.conn.kill(&query_id, true).await {
            tracing::debug!("could not kill ClickHouse query {query_id}: {e:#}");
        }
        outcome
    }

    async fn stream(
        &mut self,
        sql: &str,
        query_id: &str,
        batch: usize,
        sink: &mut RowSink<'_>,
        finished: &mut bool,
    ) -> Result<Output> {
        let response = self.send(sql, query_id).await;
        // A statement the server refused never started.
        *finished = response.is_err();
        let mut response = response?;
        let headers = response.headers().clone();
        let Some(first) = response.chunk().await? else {
            *finished = true;
            return Ok(Output::Written(written_rows(&headers)));
        };
        let format = headers.get("X-ClickHouse-Format").and_then(|f| f.to_str().ok()).unwrap_or(FORMAT);
        let mut body = Body::for_format(format);
        let mut rows = Vec::new();
        let mut sent_any = false;
        let mut chunk = Some(first);
        while let Some(bytes) = chunk {
            body.push(&bytes, &mut rows);
            if let Some(columns) = body.columns() {
                while rows.len() >= batch {
                    let rest = rows.split_off(batch);
                    sent_any = true;
                    if sink(columns, std::mem::replace(&mut rows, rest))?.is_break() {
                        return Ok(Output::Rows);
                    }
                }
            }
            chunk = match response.chunk().await {
                Ok(chunk) => chunk,
                // After a cancel or a failure the server writes its exception and closes the
                // connection without ending the body.
                Err(e) => match body.exception() {
                    Some(exception) => {
                        *finished = true;
                        bail!(exception)
                    }
                    None => return Err(e.into()),
                },
            };
        }
        *finished = true;
        let columns = body.columns().map(<[ColumnMeta]>::to_vec);
        body.finish(&mut rows)?;
        let columns = columns.context("the response has no columns")?;
        if !rows.is_empty() || !sent_any {
            // The last batch: nothing follows it to stop.
            let _ = sink(&columns, rows)?;
        }
        Ok(Output::Rows)
    }

    /// Sends `sql`, waiting out a statement the session still runs after its reader went away,
    /// and starting a new session when the server ended the idle one.
    async fn send(&mut self, sql: &str, query_id: &str) -> Result<Response> {
        let mut renewed = false;
        for _ in 0..30 {
            let response = self.conn.post(sql, query_id, Some(&self.state)).send().await?;
            let code = response.headers().get("X-ClickHouse-Exception-Code").and_then(|c| c.to_str().ok());
            match code {
                Some(SESSION_IS_LOCKED) => tokio::time::sleep(Duration::from_millis(100)).await,
                Some(SESSION_NOT_FOUND) if !renewed => {
                    renewed = true;
                    self.renew().await?;
                }
                _ => {
                    self.state.started = true;
                    return checked(response).await;
                }
            }
        }
        bail!("The session is still busy with an earlier statement. Try again in a moment.")
    }

    /// Starts a new session in the connection's database. Settings and temporary tables of the
    /// old session are gone.
    async fn renew(&mut self) -> Result<()> {
        tracing::info!("the ClickHouse session expired; starting a new one");
        *self.state = SessionState::new();
        if !self.conn.database.is_empty() {
            let sql = format!("USE {}", D.quote_ident(&self.conn.database));
            let response = self.conn.post(&sql, &Uuid::new_v4().to_string(), Some(&self.state)).send().await?;
            checked(response).await?.bytes().await?;
        }
        self.state.started = true;
        Ok(())
    }

    async fn query(&mut self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        let limit = max_rows.unwrap_or(usize::MAX);
        let mut result = ResultSet::default();
        let mut sink = |columns: &[ColumnMeta], batch: Vec<Row>| {
            if result.columns.is_empty() {
                result.columns = columns.to_vec();
            }
            result.rows.extend(batch);
            if result.rows.len() > limit {
                result.rows.truncate(limit);
                result.truncated = true;
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        };
        // One row past the limit is enough to know the result was cut.
        let batch = limit.saturating_add(1).min(ROWS_PER_BATCH);
        Ok(match self.run(sql, batch, &mut sink).await? {
            Output::Rows => StatementResult::Rows(result),
            Output::Written(n) => StatementResult::Affected(n),
        })
    }

    async fn rows(&mut self, sql: &str) -> Result<Vec<Row>> {
        match self.query(sql, None).await? {
            StatementResult::Rows(rs) => Ok(rs.rows),
            StatementResult::Affected(_) => bail!("expected rows from: {sql}"),
        }
    }
}

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

#[async_trait]
impl Connection for ChConnection {
    fn dialect(&self) -> Dialect {
        D
    }

    fn server_version(&self) -> String {
        format!("ClickHouse {}", self.version)
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo::from_version_text(Flavor::ClickHouse, &self.version)
    }

    fn database(&self) -> String {
        self.database.clone()
    }

    fn default_schema(&self) -> String {
        self.database.clone()
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.list_schemas().await
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let sql = format!("SELECT name FROM system.databases WHERE name NOT IN {SYSTEM_DATABASES} ORDER BY name");
        Ok(self.meta(&sql).await?.iter().map(|r| s(&r[0])).collect())
    }

    async fn list_objects(&self) -> Result<Vec<SchemaObject>> {
        // A materialized view without TO keeps its rows in a hidden `.inner` table.
        let rows = self
            .meta(&format!(
                "SELECT database, name, engine FROM system.tables \
                 WHERE database NOT IN {SYSTEM_DATABASES} AND NOT is_temporary AND NOT startsWith(name, '.inner') \
                 ORDER BY database, name"
            ))
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let kind = match r[2].as_deref() {
                    Some("View" | "LiveView" | "WindowView") => ObjectKind::View,
                    Some("MaterializedView") => ObjectKind::MaterializedView,
                    _ => ObjectKind::Table,
                };
                SchemaObject::new(s(&r[0]), s(&r[1]), kind)
            })
            .collect())
    }

    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let (sq, tq) = (D.quote_str(schema), D.quote_str(table));
        let cols = self
            .meta(&format!(
                "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key, compression_codec \
                 FROM system.columns WHERE database = {sq} AND table = {tq} ORDER BY position"
            ))
            .await?;
        if cols.is_empty() {
            bail!("{schema}.{table} has no columns or does not exist");
        }
        let columns: Vec<ColumnInfo> = cols
            .into_iter()
            .map(|r| {
                let data_type = s(&r[1]);
                let (kind, expr) = (s(&r[2]), s(&r[3]));
                // MATERIALIZED, ALIAS and EPHEMERAL columns aren't defaults a client can rely on.
                let generated = (!kind.is_empty() && kind != "DEFAULT").then(|| format!("{kind} {expr}"));
                let extra = [generated, r[6].clone().filter(|c| !c.is_empty())]
                    .into_iter()
                    .flatten()
                    .map(|e| e.trim().to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                ColumnInfo {
                    name: s(&r[0]),
                    nullable: data_type.starts_with("Nullable(") || data_type.starts_with("LowCardinality(Nullable("),
                    data_type,
                    default: (kind == "DEFAULT").then_some(expr),
                    primary_key: r[5].as_deref() == Some("1"),
                    extra: (!extra.is_empty()).then_some(extra),
                    comment: r[4].clone().filter(|c| !c.is_empty()),
                }
            })
            .collect();
        let table_row = self
            .meta(&format!(
                "SELECT engine, sorting_key, primary_key, comment FROM system.tables WHERE database = {sq} AND name = {tq}"
            ))
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let field = |i: usize| table_row.get(i).cloned().flatten().unwrap_or_default();
        let (engine, sorting_key, primary_key) = (field(0), field(1), field(2));
        let mut indexes = Vec::new();
        if !primary_key.is_empty() {
            indexes.push(IndexInfo {
                name: "PRIMARY KEY".into(),
                columns: key_columns(&primary_key, &columns),
                unique: false,
                primary: true,
                method: Some(engine.clone()),
                definition: Some(format!("PRIMARY KEY ({primary_key})")),
            });
        }
        if !sorting_key.is_empty() && sorting_key != primary_key {
            indexes.push(IndexInfo {
                name: "ORDER BY".into(),
                columns: key_columns(&sorting_key, &columns),
                unique: false,
                primary: false,
                method: Some(engine.clone()),
                definition: Some(format!("ORDER BY ({sorting_key})")),
            });
        }
        // Reading skip indexes needs a grant on the system table that SELECT on the table lacks.
        let skipping = self
            .meta(&format!(
                "SELECT name, expr, type_full, granularity FROM system.data_skipping_indices \
                 WHERE database = {sq} AND table = {tq} ORDER BY name"
            ))
            .await
            .unwrap_or_else(|e| {
                tracing::debug!("skip indexes of {schema}.{table} unavailable: {e:#}");
                Vec::new()
            });
        for r in skipping {
            let (name, expr, kind) = (s(&r[0]), s(&r[1]), s(&r[2]));
            indexes.push(IndexInfo {
                definition: Some(format!("INDEX {} {expr} TYPE {kind} GRANULARITY {}", D.quote_ident(&name), s(&r[3]))),
                name,
                columns: vec![expr],
                unique: false,
                primary: false,
                method: Some(kind),
            });
        }
        let mut paging = key_columns(&sorting_key, &columns);
        // A sorting key needn't be unique; a MergeTree row's part and offset are.
        if engine.ends_with("MergeTree") {
            paging.extend(["_part".to_string(), "_part_offset".to_string()]);
        }
        Ok(TableStructure {
            sorting_key: paging,
            columns,
            indexes,
            constraints: Vec::new(),
            comment: Some(field(3)).filter(|c| !c.is_empty()),
        })
    }

    async fn object_definition(&self, o: &SchemaObject) -> Result<String> {
        let rows = self.meta(&format!("SHOW CREATE TABLE {}", D.qualified(&o.schema, &o.name))).await?;
        rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()).context("definition not available")
    }

    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        self.session().await.query(sql, max_rows).await
    }

    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64> {
        let mut total = 0u64;
        let mut forward = |columns: &[ColumnMeta], rows: Vec<Row>| {
            total += rows.len() as u64;
            sink(columns, rows).map(|()| ControlFlow::Continue(()))
        };
        match self.session().await.run(sql, batch.max(1), &mut forward).await? {
            Output::Rows => Ok(total),
            Output::Written(_) => bail!("statement returned no rows"),
        }
    }

    /// ClickHouse has no transactions: the statements run in order and stop at the first error,
    /// and the ones before it stay applied.
    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64> {
        let mut session = self.session().await;
        let mut total = 0;
        for (i, stmt) in statements.iter().enumerate() {
            match session.run(stmt, ROWS_PER_BATCH, &mut |_, _| Ok(ControlFlow::Continue(()))).await {
                Ok(Output::Written(n)) => total += n,
                Ok(Output::Rows) => {}
                Err(e) => {
                    let earlier = match i {
                        0 => String::new(),
                        1 => " ClickHouse has no transactions, so the statement before it was not rolled back.".into(),
                        n => format!(
                            " ClickHouse has no transactions, so the {n} statements before it were not rolled back."
                        ),
                    };
                    return Err(e.context(format!("statement {} failed: {stmt}.{earlier}", i + 1)));
                }
            }
        }
        Ok(total)
    }

    async fn explain(&self, req: &ExplainStatements) -> Result<Explained> {
        explain_with(
            &mut self.session().await,
            req,
            |session, sql| session.query(sql, None).boxed(),
            // The server refuses a second statement in one request, so there is nothing to check.
            |_, _| async { Ok(()) }.boxed(),
            |_| async { Ok(false) }.boxed(),
        )
        .await
    }

    async fn cancel(&self) -> Result<()> {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match running {
            Some(query_id) => self.kill(&query_id, false).await,
            None => Ok(()),
        }
    }

    async fn ping(&self) -> Result<()> {
        self.session().await.rows("SELECT 1").await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_plain_columns_in_key_order() {
        let columns: Vec<ColumnInfo> = ["id", "user_id", "created_at"]
            .into_iter()
            .map(|n| ColumnInfo { name: n.into(), ..Default::default() })
            .collect();
        assert_eq!(key_columns("user_id, created_at, id", &columns), ["user_id", "created_at", "id"]);
        assert_eq!(key_columns("toDate(created_at), cityHash64(id, user_id), `id`", &columns), ["id"]);
        assert!(key_columns("", &columns).is_empty());
    }

    #[test]
    fn user_formats_read_line_by_line() {
        let mut body = Body::for_format("JSONEachRow");
        let mut rows = Vec::new();
        body.push(b"{\"a\":1}\n{\"a\"", &mut rows);
        body.push(b":2}\n{\"a\":3}", &mut rows);
        assert_eq!(rows.len(), 2);
        body.finish(&mut rows).unwrap();
        let text: Vec<_> = rows.iter().map(|r| r[0].to_text().unwrap()).collect();
        assert_eq!(text, ["{\"a\":1}", "{\"a\":2}", "{\"a\":3}"]);
        assert_eq!(Body::for_format("JSON").columns().unwrap()[0].name, "JSON");
    }

    #[test]
    fn summaries_give_written_rows() {
        let mut headers = HeaderMap::new();
        assert_eq!(written_rows(&headers), 0);
        headers.insert("X-ClickHouse-Summary", r#"{"read_rows":"7","written_rows":"7"}"#.parse().unwrap());
        assert_eq!(written_rows(&headers), 7);
    }
}
