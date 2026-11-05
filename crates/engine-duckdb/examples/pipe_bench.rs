//! What the pipe to the helper costs: the same reads in-process and through `tableplus-duckdb`.
//! Run one mode per process so peak memory is per mode:
//!
//! cargo build --release -p engine-duckdb --examples --bins
//! target/release/examples/pipe_bench <shop.duckdb> inprocess|pipe

use std::time::{Duration, Instant};

use tableplus_core::config::{ConnectionConfig, DriverKind};
use tableplus_core::driver::DynConnection;
use tableplus_core::{Credentials, StatementResult};

const RUNS: usize = 5;
const FIFTY_MILLION: &str = "SELECT range AS id, range * 2 AS v, 'row ' || range AS label FROM range(50000000)";

fn peak_rss_mb() -> f64 {
    // SAFETY: getrusage fills the struct it is given.
    let usage = unsafe {
        let mut u: libc_rusage = std::mem::zeroed();
        getrusage(0, &mut u);
        u
    };
    // macOS reports bytes, Linux kilobytes.
    if cfg!(target_os = "macos") { usage.ru_maxrss as f64 / 1e6 } else { usage.ru_maxrss as f64 / 1e3 }
}

#[repr(C)]
struct libc_rusage {
    ru_utime: [i64; 2],
    ru_stime: [i64; 2],
    ru_maxrss: i64,
    rest: [i64; 13],
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut libc_rusage) -> i32;
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

async fn grid(conn: &DynConnection, sql: &str, limit: Option<usize>) -> (Duration, usize) {
    let mut times = Vec::new();
    let mut n = 0;
    for _ in 0..RUNS {
        let t = Instant::now();
        let StatementResult::Rows(rs) = conn.query(sql, limit).await.unwrap() else { panic!("{sql}") };
        times.push(t.elapsed());
        n = rs.rows.len();
    }
    (median(times), n)
}

async fn export(conn: &DynConnection, sql: &str, runs: usize) -> (Duration, Duration, u64) {
    let mut times = Vec::new();
    let mut firsts = Vec::new();
    let mut total = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let mut first = None;
        total = conn
            .query_batches(sql, 5_000, &mut |_, rows| {
                first.get_or_insert_with(|| t.elapsed());
                std::hint::black_box(rows);
                Ok(())
            })
            .await
            .unwrap();
        times.push(t.elapsed());
        firsts.push(first.unwrap());
    }
    (median(firsts), median(times), total)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let shop = args.next().expect("path to shop.duckdb");
    let mode = args.next().unwrap_or_else(|| "pipe".into());
    let mut config = ConnectionConfig::new(DriverKind::DuckDb);
    config.path = shop;
    config.read_only = true;
    let engine = engine_duckdb::DuckEngine::default();
    let conn: DynConnection = match mode.as_str() {
        "inprocess" => std::sync::Arc::new(engine.open_session(&config)?),
        _ => {
            // SAFETY: set before the helper starts.
            unsafe {
                if std::env::var_os("TABLEPLUS_DUCKDB_HELPER").is_none() {
                    let helper = std::env::current_exe()?.parent().unwrap().parent().unwrap().join("tableplus-duckdb");
                    std::env::set_var("TABLEPLUS_DUCKDB_HELPER", helper);
                }
            }
            tableplus_core::driver::connect(&config, &Credentials::default()).await?
        }
    };
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    for (label, sql) in [
        ("order_items, 40,001 rows of numbers", "SELECT * FROM order_items"),
        ("orders, 20,000 rows with enum and timestamp", "SELECT * FROM orders"),
        ("users, 5,000 rows with decimal and text", "SELECT * FROM users"),
    ] {
        let (t, n) = grid(&conn, sql, Some(100_000)).await;
        println!("{mode:9} grid    {label:45} {n:>9} rows {:8.1} ms", ms(t));
    }
    let (t, n) = grid(&conn, FIFTY_MILLION, Some(100_000)).await;
    println!("{mode:9} grid    {:45} {n:>9} rows {:8.1} ms", "50M-row query, first 100,000 (grid limit)", ms(t));
    let (first, t, n) = export(&conn, "SELECT * FROM orders", RUNS).await;
    println!("{mode:9} export  {:45} {n:>9} rows {:8.1} ms, first batch {:.2} ms", "orders", ms(t), ms(first));
    let (first, t, n) = export(&conn, FIFTY_MILLION, 1).await;
    println!("{mode:9} export  {:45} {n:>9} rows {:8.1} ms, first batch {:.2} ms", "50M rows", ms(t), ms(first));
    println!("{mode:9} peak RSS of this process: {:.0} MB", peak_rss_mb());
    Ok(())
}
