//! Explain Analyze on SQLite: the bundled library measures each plan step while the statement
//! runs, and the driver reads those measurements through `sqlite3_stmt_scanstatus_v2`.

mod common;

use common::*;
use tableplus_core::config::Credentials;
use tableplus_core::driver;

#[tokio::test(flavor = "multi_thread")]
async fn the_bundled_library_measures_plan_steps() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    let options = rows(&conn, "PRAGMA compile_options").await;
    let options: Vec<String> = options.rows.iter().filter_map(|r| r[0].to_text()).collect();
    assert!(
        options.iter().any(|o| o == "ENABLE_STMT_SCANSTATUS"),
        "built without SQLITE_ENABLE_STMT_SCANSTATUS; cargo reads LIBSQLITE3_FLAGS from .cargo/config.toml: {options:?}"
    );
}
