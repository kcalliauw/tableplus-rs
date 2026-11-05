//! `tableplus-duckdb`: serves DuckDB to the tableplus-rs app over standard input and output.

fn main() -> anyhow::Result<()> {
    driver_host::serve(engine_duckdb::DuckEngine::default())
}
