//! The DuckDB driver that `tableplus-duckdb` serves to the app. It lives in its own program so
//! the app doesn't carry DuckDB's 33 MB (see `docs/duckdb-research.md`, section 7).

mod catalog;
mod raw;
mod run;
mod values;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use tableplus_core::driver::{BatchSink, Connection, DynConnection, Explained};
use tableplus_core::duckdb::DuckDbTarget;
use tableplus_core::plan::{DUCKDB_ANALYZE, Explain, ExplainStatements};
use tableplus_core::{
    ConnectionConfig, Dialect, Flavor, QueryOutput, SchemaObject, ServerInfo, StatementResult, TableStructure,
};

use crate::raw::{Conn, Database, Interrupt, StatementType};

pub use crate::raw::library_version;

/// Settings every database opens with. Extensions DuckDB knows load when a query needs them, but
/// only from disk: nothing is downloaded without an explicit `INSTALL`.
const SETTINGS: &[(&str, &str)] = &[("autoinstall_known_extensions", "false"), ("autoload_known_extensions", "true")];

/// The database this helper serves, opened by the first session.
struct Opened {
    target: DuckDbTarget,
    read_only: bool,
    db: Arc<Database>,
}

/// Serves one database: every session the app opens in this process shares it.
#[derive(Default)]
pub struct DuckEngine {
    opened: Mutex<Option<Opened>>,
}

impl DuckEngine {
    fn database(&self, target: &DuckDbTarget, read_only: bool) -> Result<Arc<Database>> {
        let mut opened = self.opened.lock().unwrap();
        if let Some(o) = opened.as_ref() {
            if o.target != *target || o.read_only != read_only {
                bail!("this DuckDB helper already serves another database");
            }
            return Ok(o.db.clone());
        }
        let db = match target {
            DuckDbTarget::Memory => Database::open(None, SETTINGS)?,
            DuckDbTarget::Database(path) => {
                let mut settings = SETTINGS.to_vec();
                if read_only {
                    settings.push(("access_mode", "READ_ONLY"));
                }
                Database::open(Some(&path.to_string_lossy()), &settings)?
            }
            DuckDbTarget::DataFile { path, reader, view } => {
                let db = Database::open(None, SETTINGS)?;
                let conn = db.connect()?;
                let source = Dialect::DuckDb.quote_str(&path.to_string_lossy());
                conn.execute(&format!(
                    "CREATE VIEW {} AS SELECT * FROM {}({source})",
                    Dialect::DuckDb.quote_ident(view),
                    reader.function()
                ))?;
                db
            }
        };
        *opened = Some(Opened { target: target.clone(), read_only, db: db.clone() });
        Ok(db)
    }

    /// Opens a session in this process, without a helper. Used by the helper itself and by tests.
    pub fn open_session(&self, config: &ConnectionConfig) -> Result<DuckConnection> {
        let target = DuckDbTarget::of(config)?;
        let read_only = config.read_only && matches!(target, DuckDbTarget::Database(_));
        let db = self.database(&target, read_only)?;
        let conn = db.connect()?;
        let database = match &target {
            DuckDbTarget::Memory => tableplus_core::duckdb::MEMORY.to_string(),
            DuckDbTarget::Database(path) | DuckDbTarget::DataFile { path, .. } => {
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            }
        };
        Ok(DuckConnection {
            interrupt: conn.interrupt_handle(),
            conn: Arc::new(Mutex::new(conn)),
            database,
            version: library_version(),
        })
    }
}

#[async_trait]
impl driver_host::Engine for DuckEngine {
    fn name(&self) -> &'static str {
        tableplus_core::engines::DUCKDB.name
    }

    fn version(&self) -> String {
        library_version()
    }

    async fn open(&self, config: ConnectionConfig) -> Result<DynConnection> {
        Ok(Arc::new(self.open_session(&config)?))
    }
}

/// One session: a DuckDB connection on the helper's shared database.
pub struct DuckConnection {
    conn: Arc<Mutex<Conn>>,
    interrupt: Interrupt,
    database: String,
    version: String,
}

impl DuckConnection {
    async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&Conn) -> Result<T> + Send + 'static) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| anyhow!("DuckDB connection poisoned"))?;
            f(&guard)
        })
        .await?
    }
}

#[async_trait]
impl Connection for DuckConnection {
    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }

    fn server_version(&self) -> String {
        format!("DuckDB {}", self.version.trim_start_matches('v'))
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo::from_version_text(Flavor::DuckDb, &self.version)
    }

    fn database(&self) -> String {
        self.database.clone()
    }

    fn default_schema(&self) -> String {
        "main".into()
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.with_conn(catalog::databases).await
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        self.with_conn(catalog::schemas).await
    }

    async fn list_objects(&self) -> Result<Vec<SchemaObject>> {
        self.with_conn(catalog::objects).await
    }

    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let (schema, table) = (schema.to_string(), table.to_string());
        self.with_conn(move |c| catalog::structure(c, &schema, &table)).await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let object = object.clone();
        self.with_conn(move |c| catalog::definition(c, &object)).await
    }

    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        let sql = sql.to_string();
        self.with_conn(move |c| run::collect(c, &sql, max_rows)).await
    }

    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64> {
        // The blocking reader hands batches over a bounded channel, so memory stays at a few
        // batches whatever the result's size.
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let reader = tokio::task::spawn_blocking(move || -> Result<u64> {
            let guard = conn.lock().map_err(|_| anyhow!("DuckDB connection poisoned"))?;
            run::batches(&guard, &sql, batch, &mut |columns, rows| {
                Ok(tx.blocking_send((columns.to_vec(), rows)).is_ok())
            })
        });
        let mut sink_error = None;
        while let Some((columns, rows)) = rx.recv().await {
            if sink_error.is_none()
                && let Err(e) = sink(&columns, rows)
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
            c.execute("BEGIN TRANSACTION")?;
            let mut total = 0u64;
            for (i, stmt) in statements.iter().enumerate() {
                match run::collect(c, stmt, Some(0)) {
                    Ok(StatementResult::Affected(n)) => total += n,
                    Ok(StatementResult::Rows(_)) => {}
                    Err(e) => {
                        let _ = c.execute("ROLLBACK");
                        let e = e.context(format!("statement {} failed: {stmt}", i + 1));
                        return Err(if is_rebuild_drop(stmt, &e) {
                            e.context(
                                "DuckDB can't change this column while the table has an index, so the table is \
                                 rebuilt, and DuckDB won't drop a table that another table's foreign key references",
                            )
                        } else {
                            e
                        });
                    }
                }
            }
            c.execute("COMMIT")?;
            Ok(total)
        })
        .await
    }

    async fn explain(&self, req: &ExplainStatements) -> Result<Explained> {
        let req = req.clone();
        self.with_conn(move |c| explain(c, &req)).await
    }

    async fn cancel(&self) -> Result<()> {
        // SAFETY: `self.conn` keeps the connection open while `self` lives.
        unsafe { self.interrupt.interrupt() };
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        self.with_conn(|c| c.execute("SELECT 1")).await
    }
}

/// The structure editor's table rebuild (`tableplus_core::alter`) failed at dropping the old table.
fn is_rebuild_drop(stmt: &str, e: &anyhow::Error) -> bool {
    stmt.starts_with("DROP TABLE") && format!("{e:#}").contains("is main key table of")
}

fn explain(c: &Conn, req: &ExplainStatements) -> Result<Explained> {
    let Explain::Statement(sql) = &req.explain else {
        bail!("DuckDB measures plans through EXPLAIN statements");
    };
    // Counted by DuckDB's parser, because the app's statement check can miss a second statement.
    if run::statement_count(c, sql)? != 1 {
        bail!("Explain works on one statement at a time.");
    }
    // The statement check reads only the statement's shape; DuckDB's own type catches a write it
    // misread. SELECT also covers SHOW, DESCRIBE and SUMMARIZE, which read.
    if let Some(inner) = sql.strip_prefix(DUCKDB_ANALYZE)
        && run::statement_type(c, inner)? != StatementType::Select
    {
        bail!("Explain Analyze runs the statement, and DuckDB says this one is not a query.");
    }
    // The setup opens a read-only transaction. Inside the user's own, it would fail and leave
    // theirs aborted, so an open transaction is refused before anything runs.
    if req.needs_idle_session && run::in_transaction(c)? {
        bail!("{}", tableplus_core::driver::TRANSACTION_OPEN);
    }
    for setup in &req.setup {
        c.execute(setup)?;
    }
    let plan = (|| {
        let started = Instant::now();
        let result = run::collect(c, sql, None)?;
        Ok(QueryOutput { sql: sql.clone(), result, elapsed: started.elapsed() })
    })();
    let failed = req.reset.iter().filter_map(|r| c.execute(r).err().map(|e| format!("{r}: {e:#}"))).collect();
    tableplus_core::driver::explained(plan, failed)
}
