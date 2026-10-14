//! The database abstraction used by the UI, plus the PostgreSQL, MySQL/MariaDB and SQLite
//! implementations.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{ConnectionConfig, Credentials, DriverKind};
use crate::dialect::Dialect;
use crate::model::{ColumnMeta, QueryOutput, Row, SchemaObject, StatementResult, TableStructure};

/// Receives rows in batches from [`Connection::query_batches`].
pub type BatchSink<'a> = dyn FnMut(&[ColumnMeta], Vec<Row>) -> Result<()> + Send + 'a;

pub mod mysql;
pub mod postgres;
pub mod sqlite;

/// An open connection. All methods take `&self`; implementations serialize access
/// internally so a connection can be shared between UI tasks.
#[async_trait]
pub trait Connection: Send + Sync {
    fn dialect(&self) -> Dialect;
    /// e.g. `PostgreSQL 17.6`.
    fn server_version(&self) -> String;
    /// The database this connection is bound to (file name for SQLite).
    fn database(&self) -> String;
    /// Schema selected by default in the sidebar.
    fn default_schema(&self) -> String;

    async fn list_databases(&self) -> Result<Vec<String>>;
    async fn list_schemas(&self) -> Result<Vec<String>>;
    async fn list_objects(&self) -> Result<Vec<SchemaObject>>;
    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure>;
    /// DDL for a table, view or routine.
    async fn object_definition(&self, object: &SchemaObject) -> Result<String>;
    /// Executes one statement. Row-returning statements stop reading after `max_rows`.
    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult>;
    /// Streams a row-returning statement to `sink` in batches of up to `batch` rows without
    /// holding the whole result in memory (used by exports). Returns the number of rows.
    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64>;
    /// Runs all statements in one transaction (rolled back on the first error) and returns
    /// the total number of affected rows.
    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64>;
    /// Best-effort cancellation of the statement currently running.
    async fn cancel(&self) -> Result<()>;
    async fn ping(&self) -> Result<()>;
}

pub type DynConnection = Arc<dyn Connection>;

/// Opens a connection (and SSH tunnel, if configured).
pub async fn connect(config: &ConnectionConfig, creds: &Credentials) -> Result<DynConnection> {
    Ok(match config.driver {
        DriverKind::Postgres => Arc::new(postgres::PgConnection::connect(config, creds).await?),
        DriverKind::MySql | DriverKind::MariaDb => Arc::new(mysql::MyConnection::connect(config, creds).await?),
        DriverKind::Sqlite => Arc::new(sqlite::SqliteConnection::open(config).await?),
    })
}

/// Executes `sql` and records how long it took.
pub async fn run(conn: &dyn Connection, sql: &str, max_rows: Option<usize>) -> Result<QueryOutput> {
    let started = Instant::now();
    let result = conn.query(sql, max_rows).await?;
    Ok(QueryOutput { sql: sql.to_string(), result, elapsed: started.elapsed() })
}

/// Reads a single scalar (first column of first row) as text.
pub async fn scalar(conn: &dyn Connection, sql: &str) -> Result<Option<String>> {
    match conn.query(sql, Some(1)).await? {
        StatementResult::Rows(rs) => Ok(rs.rows.first().and_then(|r| r.first()).and_then(|v| v.to_text())),
        StatementResult::Affected(_) => Ok(None),
    }
}

/// Opens and immediately closes a connection, returning the server version.
pub async fn test_connection(config: &ConnectionConfig, creds: &Credentials) -> Result<String> {
    let conn = connect(config, creds).await?;
    conn.ping().await?;
    Ok(conn.server_version())
}

/// Builds a readable error from driver error chains (`Error: db error: ERROR: …`).
pub(crate) fn describe(err: impl std::fmt::Display, source: Option<&(dyn std::error::Error + 'static)>) -> String {
    match source {
        Some(s) => format!("{err}: {s}"),
        None => err.to_string(),
    }
}
