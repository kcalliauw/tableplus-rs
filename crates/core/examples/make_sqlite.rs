//! Creates the SQLite demo/test database from `docker/seed/sqlite.sql`.
//!
//! cargo run -p tableplus-core --example make_sqlite -- testdata/shop.sqlite

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "testdata/shop.sqlite".into());
    let path = std::path::Path::new(&path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(include_str!("../../../docker/seed/sqlite.sql"))?;
    let users: i64 = conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
    println!("created {} ({users} users)", path.display());
    Ok(())
}
