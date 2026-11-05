//! Creates the DuckDB demo and test database from `docker/seed/duckdb.sql`, through the same
//! driver the helper serves, and `orders.parquet` beside it for opening a data file directly.
//!
//! cargo run -p engine-duckdb --example make_duckdb -- testdata/shop.duckdb

use tableplus_core::config::{ConnectionConfig, DriverKind};
use tableplus_core::driver::Connection;
use tableplus_core::export::sql_script_statements;
use tableplus_core::{Dialect, StatementResult, Value};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "testdata/shop.duckdb".into());
    let path = std::path::Path::new(&path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    for stale in [path.to_path_buf(), path.with_extension("duckdb.wal")] {
        if stale.exists() {
            std::fs::remove_file(stale)?;
        }
    }
    let mut config = ConnectionConfig::new(DriverKind::DuckDb);
    config.path = path.to_string_lossy().into_owned();
    let engine = engine_duckdb::DuckEngine::default();
    let conn = engine.open_session(&config)?;
    let script = include_str!("../../../docker/seed/duckdb.sql");
    conn.execute_in_transaction(&sql_script_statements(script, Dialect::DuckDb)).await?;
    let parquet = path.with_file_name("orders.parquet");
    let target = Dialect::DuckDb.quote_str(&parquet.to_string_lossy());
    conn.query(&format!("COPY orders TO {target} (FORMAT parquet)"), None).await?;
    let StatementResult::Rows(rs) = conn.query("SELECT count(*) FROM users", None).await? else { unreachable!() };
    let users = rs.rows[0].first().cloned().unwrap_or(Value::Null);
    println!("created {} ({} users) and {}", path.display(), users.display(), parquet.display());
    Ok(())
}
