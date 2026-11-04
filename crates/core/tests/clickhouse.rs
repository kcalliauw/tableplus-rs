//! Integration tests against the ClickHouse 25.8 container.

mod common;

use std::time::Duration;

use common::*;
use tableplus_core::config::SslMode;
use tableplus_core::driver::{self, DynConnection};
use tableplus_core::export::{ExportFormat, ExportOptions, Exporter, sql_script_statements};
use tableplus_core::model::ObjectKind;
use tableplus_core::plan::{self, ExplainMode};
use tableplus_core::sqlgen::{self, Filter, FilterOp, SortKey};
use tableplus_core::{Dialect, Flavor, StatementResult, Value};

async fn connect() -> Option<DynConnection> {
    connect_or_skip(&clickhouse_config(), &password()).await
}

fn filter(column: Option<&str>, op: FilterOp, value: &str) -> Filter {
    Filter { column: column.map(str::to_string), op, value: value.into(), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_metadata() {
    let Some(conn) = connect().await else {
        return;
    };
    assert!(conn.server_version().starts_with("ClickHouse 25.8"), "{}", conn.server_version());
    let info = conn.server_info();
    assert_eq!((info.flavor, info.version.major, info.version.minor), (Flavor::ClickHouse, 25, 8), "{info}");
    assert_eq!(
        (conn.dialect(), conn.database(), conn.default_schema()),
        (Dialect::ClickHouse, "shop".into(), "shop".into())
    );
    assert_eq!(conn.list_schemas().await.unwrap(), vec!["analytics", "default", "shop"]);
    assert_eq!(conn.list_databases().await.unwrap(), conn.list_schemas().await.unwrap());

    let objects = conn.list_objects().await.unwrap();
    let find = |schema: &str, name: &str| objects.iter().find(|o| o.schema == schema && o.name == name).map(|o| o.kind);
    assert_eq!(find("shop", "orders"), Some(ObjectKind::Table));
    assert_eq!(find("shop", "audit_log"), Some(ObjectKind::Table));
    assert_eq!(find("shop", "active_users"), Some(ObjectKind::View));
    assert_eq!(find("shop", "daily_revenue"), Some(ObjectKind::MaterializedView));
    assert_eq!(find("analytics", "events"), Some(ObjectKind::Table));
    assert!(objects.iter().all(|o| !o.name.starts_with(".inner") && o.schema != "system"), "{objects:?}");

    let orders = conn.table_structure("shop", "orders").await.unwrap();
    let col = |name: &str| orders.column(name).unwrap();
    assert_eq!(col("status").data_type, "LowCardinality(String)");
    assert_eq!(col("discount").data_type, "Nullable(Float64)");
    assert!(col("discount").nullable && col("note").nullable && !col("total").nullable);
    assert_eq!(orders.primary_key(), vec!["user_id", "created_at"]);
    assert_eq!(orders.sorting_key, vec!["user_id", "created_at", "id", "_part", "_part_offset"]);
    assert_eq!(orders.paging_key(), orders.sorting_key);
    let skip = orders.indexes.iter().find(|i| i.name == "idx_status").unwrap();
    assert_eq!(skip.definition.as_deref(), Some("INDEX `idx_status` status TYPE set(8) GRANULARITY 4"));
    let order_by = orders.indexes.iter().find(|i| i.name == "ORDER BY").unwrap();
    assert_eq!(order_by.definition.as_deref(), Some("ORDER BY (user_id, created_at, id)"));

    let users = conn.table_structure("shop", "users").await.unwrap();
    assert_eq!(users.comment.as_deref(), Some("Registered customers"));
    assert_eq!(users.column("country").unwrap().default.as_deref(), Some("'US'"));
    assert_eq!(users.column("nickname").unwrap().comment.as_deref(), Some("Shown instead of the full name"));
    let log = conn.table_structure("shop", "audit_log").await.unwrap();
    assert!(log.sorting_key.is_empty() && log.paging_key().is_empty());
    assert!(conn.table_structure("shop", "no_such_table").await.is_err());

    let table = objects.iter().find(|o| o.name == "orders").unwrap();
    let ddl = conn.object_definition(table).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE shop.orders") && ddl.contains("\n"), "{ddl}");
    let view = objects.iter().find(|o| o.name == "daily_revenue").unwrap();
    assert!(conn.object_definition(view).await.unwrap().starts_with("CREATE MATERIALIZED VIEW shop.daily_revenue"));
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_typed_values() {
    let Some(conn) = connect().await else {
        return;
    };
    let rs = rows(&conn, "SELECT * FROM type_zoo ORDER BY id").await;
    let col = |n: &str| rs.column_index(n).unwrap();
    let (r, r2) = (&rs.rows[0], &rs.rows[1]);
    assert_eq!(r[col("id")], Value::UInt(1));
    assert_eq!(r[col("c_int8")], Value::Int(-8));
    assert_eq!(r[col("c_uint64")], Value::UInt(u64::MAX));
    assert_eq!(r[col("c_int128")], Value::Decimal("-170141183460469231731687303715884105728".into()));
    assert_eq!(r[col("c_float64")], Value::Float(std::f64::consts::E));
    assert_eq!(r[col("c_decimal")], Value::Decimal("12345.6789".into()));
    assert_eq!(r[col("c_bool")], Value::Bool(true));
    assert_eq!(r[col("c_date")], Value::text("2024-02-29"));
    assert_eq!(r[col("c_dt64")], Value::text("2024-02-29 13:37:00.123"));
    assert_eq!(r[col("c_enum")], Value::text("b"));
    assert_eq!(r[col("c_map")], Value::text("{'k':1}"));
    assert_eq!(r[col("c_tuple")], Value::text("('x',-1)"));
    assert_eq!(r[col("c_ipv4")], Value::text("10.0.0.1"));
    assert!(r[col("c_nullable")].is_null() && r[col("c_lc_null")].is_null());
    assert_eq!(r2[col("c_float32")], Value::Float(f64::INFINITY));
    assert!(matches!(r2[col("c_float64")], Value::Float(f) if f.is_nan()));
    assert_eq!(r2[col("c_fixed")], Value::text("z\0\0\0"));
    assert_eq!(r2[col("c_nullable")], Value::Int(7));
    assert_eq!(r2[col("c_lc_null")], Value::text("seven"));
    assert_eq!(rs.columns[col("c_dt64")].type_name, "DateTime64(3, 'UTC')");
    assert_eq!(rs.columns[col("c_lc_null")].type_name, "LowCardinality(Nullable(String))");

    let rs = rows(&conn, "SELECT label, value, raw FROM escapes ORDER BY id").await;
    let value = |label: &str| rs.rows.iter().find(|r| r[0] == Value::text(label)).unwrap();
    assert_eq!(value("tab")[1], Value::text("a\tb"));
    assert_eq!(value("newline")[1], Value::text("line1\nline2"));
    assert_eq!(value("backslash")[1], Value::text("C:\\temp\\new"));
    assert!(value("null")[1].is_null());
    assert_eq!(value("backslash N")[1], Value::text("\\N"), "the text \\N is not NULL");
    assert_eq!(value("quote")[1], Value::text("it's"));
    assert_eq!(value("unicode")[1], Value::text("héllo wörld ✓"));
    assert_eq!(value("binary")[2], Value::Bytes(vec![0xff, 0x00, 0xfe].into()));
    assert_eq!(value("empty")[1], Value::text(""));

    // Literals the dialect writes read back as the same values.
    let d = conn.dialect();
    let tricky = "it's \\ a\ttab\nnewline \\N";
    let sql = format!(
        "SELECT {}, {}, {}",
        d.quote_str(tricky),
        d.literal(&Value::Bytes(vec![0, 0xff].into())),
        d.bool_literal(true)
    );
    let rs = rows(&conn, &sql).await;
    assert_eq!(rs.rows[0][..], [Value::text(tricky), Value::Bytes(vec![0, 0xff].into()), Value::Bool(true)]);
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 41 + 1").await.unwrap().as_deref(), Some("42"));
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_paging_sorting_and_filters() {
    let Some(conn) = connect().await else {
        return;
    };
    let d = conn.dialect();
    assert_eq!(count(&conn, "shop", "orders", None).await, 5000);
    let st = conn.table_structure("shop", "orders").await.unwrap();
    let names: Vec<String> = st.columns.iter().map(|c| c.name.clone()).collect();
    let sort = sqlgen::with_tiebreaker(&[SortKey { column: "total".into(), descending: true }], &st.paging_key());
    let page = |offset: u64| {
        let sql = sqlgen::select_page_sql(d, "shop", "orders", None, &sort, Some(100), offset);
        let conn = conn.clone();
        async move { rows(&conn, &sql).await }
    };
    let (first, second) = (page(0).await, page(100).await);
    assert_eq!((first.rows.len(), second.rows.len()), (100, 100));
    assert_eq!(first.rows[0][col_of(&first, "total")], Value::Decimal("1248.75".into()));
    let ids = |rs: &tableplus_core::ResultSet| rs.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    assert!(ids(&first).iter().all(|id| !ids(&second).contains(id)), "pages don't overlap");
    assert_eq!(page(4950).await.rows.len(), 50);

    let w = |f: Filter| sqlgen::where_clause(d, &[f], &names);
    let n = |f: Filter| {
        let w = w(f);
        let conn = conn.clone();
        async move { count(&conn, "shop", "orders", w.as_deref()).await }
    };
    assert_eq!(n(filter(Some("status"), FilterOp::Contains, "PAI")).await, 1250, "case-insensitive contains");
    assert_eq!(n(filter(Some("user_id"), FilterOp::Equal, "5")).await, 25);
    assert_eq!(n(filter(Some("user_id"), FilterOp::In, "1, 2, 3")).await, 75);
    assert_eq!(n(filter(Some("note"), FilterOp::IsNull, "")).await, 1000);
    assert_eq!(n(filter(Some("discount"), FilterOp::IsNotNull, "")).await, 3333);
    let first_day =
        Filter { value2: "2024-01-01 23:00:00".into(), ..filter(Some("created_at"), FilterOp::Between, "2024-01-01") };
    assert_eq!(n(first_day).await, 24);
    assert_eq!(n(filter(None, FilterOp::Contains, "note 4242")).await, 1, "any column");
    assert_eq!(n(filter(Some("items"), FilterOp::Contains, "sku-2")).await, 1250);
    assert_eq!(n(filter(Some("id"), FilterOp::Raw, "id % 1000 = 0")).await, 5);

    let rs = match conn.query("SELECT * FROM analytics.events", Some(10_000)).await.unwrap() {
        StatementResult::Rows(rs) => rs,
        other => panic!("{other:?}"),
    };
    assert!(rs.truncated);
    assert_eq!(rs.rows.len(), 10_000);
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 1").await.unwrap().as_deref(), Some("1"), "the session is free");
    // A statement still running when the row limit is reached is stopped, freeing the session.
    let started = std::time::Instant::now();
    let slow = "SELECT sleepEachRow(0.2), number FROM numbers(50) SETTINGS max_block_size = 1";
    match conn.query(slow, Some(2)).await.unwrap() {
        StatementResult::Rows(rs) => assert!(rs.truncated && rs.rows.len() == 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 2").await.unwrap().as_deref(), Some("2"));
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    let empty = rows(&conn, "SELECT id FROM orders WHERE id = 0").await;
    assert_eq!((empty.columns.len(), empty.rows.len()), (1, 0));
}

fn col_of(rs: &tableplus_core::ResultSet, name: &str) -> usize {
    rs.column_index(name).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_statements_and_session() {
    let Some(conn) = connect().await else {
        return;
    };
    conn.query("SET max_block_size = 4321", None).await.unwrap();
    assert_eq!(
        driver::scalar(conn.as_ref(), "SELECT getSetting('max_block_size')").await.unwrap().as_deref(),
        Some("4321")
    );
    conn.query("CREATE TEMPORARY TABLE tp_tmp (a UInt8)", None).await.unwrap();
    match conn.query("INSERT INTO tp_tmp VALUES (1), (2), (3)", None).await.unwrap() {
        StatementResult::Affected(n) => assert_eq!(n, 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT count() FROM tp_tmp").await.unwrap().as_deref(), Some("3"));

    let table = format!("shop.tp_scratch_{}", std::process::id());
    conn.query(&format!("DROP TABLE IF EXISTS {table}"), None).await.unwrap();
    conn.query(&format!("CREATE TABLE {table} (id UInt32, name String) ENGINE = MergeTree ORDER BY id"), None)
        .await
        .unwrap();
    match conn
        .query(&format!("INSERT INTO {table} SELECT number, toString(number) FROM numbers(7)"), None)
        .await
        .unwrap()
    {
        StatementResult::Affected(n) => assert_eq!(n, 7),
        other => panic!("{other:?}"),
    }
    // Statements run in order and stop at the first error, keeping the ones before it.
    let err = conn
        .execute_in_transaction(&[
            format!("INSERT INTO {table} VALUES (100, 'kept')"),
            format!("INSERT INTO {table} VALUES (101, 'kept too')"),
            format!("INSERT INTO {table} VALUES ('not a number', 'x')"),
            format!("INSERT INTO {table} VALUES (102, 'never runs')"),
        ])
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("statement 3 failed") && msg.contains("2 statements before it were not rolled back"), "{msg}");
    assert_eq!(count(&conn, "shop", &table["shop.".len()..], None).await, 9);
    let n = conn.execute_in_transaction(&[format!("INSERT INTO {table} VALUES (200, 'a'), (201, 'b')")]).await.unwrap();
    assert_eq!(n, 2);
    conn.query(&format!("DROP TABLE {table}"), None).await.unwrap();

    // A statement that names its own format comes back as that format's text, line by line.
    let rs = rows(&conn, "SELECT number AS n FROM numbers(2) FORMAT JSONEachRow").await;
    assert_eq!(rs.columns[0].name, "JSONEachRow");
    assert_eq!(rs.rows.iter().map(|r| r[0].to_text().unwrap()).collect::<Vec<_>>(), ["{\"n\":0}", "{\"n\":1}"]);

    let err = conn.query("SELECT * FROM no_such_table", None).await.unwrap_err().to_string();
    assert!(err.starts_with("Code: 60.") && err.contains("UNKNOWN_TABLE"), "{err}");
    // An error after some rows were already sent.
    let err = conn
        .query("SELECT throwIf(number = 50000, 'boom') FROM numbers(100000) SETTINGS max_block_size = 1000", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("Code: 395.") && err.contains("boom"), "{err}");
    assert!(conn.query("SELECT 1; SELECT 2", None).await.is_err(), "one statement per request");
    conn.ping().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_cancel_and_tls() {
    let Some(conn) = connect().await else {
        return;
    };
    conn.cancel().await.expect("nothing to cancel is fine");
    let c2 = conn.clone();
    let started = std::time::Instant::now();
    let task = tokio::spawn(async move {
        c2.query("SELECT sleepEachRow(0.5) FROM numbers(60) SETTINGS max_block_size = 1", None).await
    });
    tokio::time::sleep(Duration::from_millis(700)).await;
    conn.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel took effect").unwrap();
    let err = res.unwrap_err().to_string();
    assert!(err.contains("QUERY_WAS_CANCELLED") || err.contains("cancelled"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 1").await.unwrap().as_deref(), Some("1"));

    // The test server speaks plain HTTP: Prefer falls back, Require refuses.
    let mut cfg = clickhouse_config();
    cfg.tls.mode = SslMode::Disable;
    driver::connect(&cfg, &password()).await.unwrap();
    cfg.tls.mode = SslMode::Require;
    assert!(driver::connect(&cfg, &password()).await.is_err(), "no TLS on the plain HTTP port");
    let mut unicode = clickhouse_config();
    unicode.user = "tp_unicode".into();
    let creds = tableplus_core::Credentials { password: Some("pässwörd✓".into()), ..Default::default() };
    let other = driver::connect(&unicode, &creds).await.unwrap();
    assert_eq!(driver::scalar(other.as_ref(), "SELECT count() FROM users").await.unwrap().as_deref(), Some("200"));
    // A user with SELECT alone can't kill queries or read skip indexes.
    match other.query("SELECT number FROM numbers(3)", Some(2)).await.unwrap() {
        StatementResult::Rows(rs) => assert!(rs.rows.len() == 2 && rs.truncated),
        r => panic!("{r:?}"),
    }
    assert_eq!(driver::scalar(other.as_ref(), "SELECT 1").await.unwrap().as_deref(), Some("1"));
    assert!(!other.table_structure("shop", "orders").await.unwrap().columns.is_empty());
    let wrong = tableplus_core::Credentials { password: Some("wrong".into()), ..Default::default() };
    let err = format!("{:#}", driver::connect(&clickhouse_config(), &wrong).await.err().unwrap());
    assert!(err.contains("AUTHENTICATION_FAILED") || err.contains("Authentication failed"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_streaming() {
    let Some(conn) = connect().await else {
        return;
    };
    timed(
        "clickhouse streaming 250k rows",
        exercise_streaming(&conn, "SELECT * FROM analytics.events WHERE id <= 250000", 250_000),
    )
    .await;
    let err = conn.query_batches("SET max_threads = 2", 10, &mut |_, _| Ok(())).await.unwrap_err();
    assert!(err.to_string().contains("no rows"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_export_round_trip() {
    let Some(conn) = connect().await else {
        return;
    };
    let d = conn.dialect();
    let copy = format!("tp_escapes_{}", std::process::id());
    conn.query(&format!("DROP TABLE IF EXISTS shop.{copy}"), None).await.unwrap();
    conn.query(&format!("CREATE TABLE shop.{copy} AS shop.escapes"), None).await.unwrap();

    let export = |format: ExportFormat| {
        let conn = conn.clone();
        let copy = copy.clone();
        async move {
            let opts = ExportOptions { format, table: d.qualified("shop", &copy), dialect: d, ..Default::default() };
            let file = tempfile::NamedTempFile::new().unwrap();
            let mut writer = Some(file.reopen().unwrap());
            let mut exporter = None;
            let sql = sqlgen::select_page_sql(d, "shop", "escapes", None, &[], None, 0);
            conn.query_batches(&sql, 4, &mut |cols, rows| {
                if exporter.is_none() {
                    let names = cols.iter().map(|c| c.name.clone()).collect();
                    exporter = Some(Exporter::new(writer.take().unwrap(), opts.clone(), names)?);
                }
                exporter.as_mut().unwrap().write_rows(rows.iter().map(|r| &r[..]))
            })
            .await
            .unwrap();
            assert_eq!(exporter.unwrap().finish().unwrap(), 9);
            std::fs::read_to_string(file.path()).unwrap()
        }
    };
    let csv = export(ExportFormat::Csv).await;
    assert!(csv.starts_with("id,label,value,raw\n") && csv.contains("\"line1\nline2\""), "{csv}");

    let script = export(ExportFormat::SqlInsert).await;
    let statements = sql_script_statements(&script, d);
    assert_eq!(conn.execute_in_transaction(&statements).await.unwrap(), 9);
    let original = rows(&conn, "SELECT * FROM shop.escapes ORDER BY id").await.rows;
    let copied = rows(&conn, &format!("SELECT * FROM shop.{copy} ORDER BY id")).await.rows;
    assert_eq!(copied, original, "the INSERT export reads back as the same values");
    conn.query(&format!("DROP TABLE shop.{copy}"), None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_explain_text() {
    let Some(conn) = connect().await else {
        return;
    };
    let server = conn.server_info();
    let sql = "SELECT count() FROM orders WHERE user_id = 5";
    let req = plan::request(&server, sql, ExplainMode::Estimate).unwrap();
    assert_eq!(req.explain.sql(), format!("EXPLAIN indexes = 1 {sql}"));
    let explained = conn.explain(&req).await.unwrap();
    assert_eq!(explained.reset_error, None);
    let rs = explained.output.rows().unwrap();
    assert!(plan::parse(&server, rs).is_none(), "no parser reads ClickHouse plans; the Plan tab shows the text");
    let text = plan::raw_text(rs);
    assert!(text.contains("ReadFromMergeTree (shop.orders)") && text.contains("PrimaryKey"), "{text}");
    assert!(text.contains("\n  "), "indentation survives: {text}");

    let why = plan::request(&server, sql, ExplainMode::Analyze).unwrap_err();
    assert!(why.0.contains("no Explain Analyze"), "{why}");
    assert!(plan::asks_for_plan(&server, "EXPLAIN PIPELINE SELECT 1"));
    let typed = rows(&conn, "EXPLAIN SYNTAX SELECT 1").await;
    assert!(!typed.rows.is_empty());
    assert!(!plan::asks_for_plan(&server, "DESCRIBE TABLE orders"));
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_pages_dont_overlap_on_a_shared_sorting_key() {
    let Some(conn) = connect().await else {
        return;
    };
    let table = "shop.paging_dupes";
    conn.query(&format!("DROP TABLE IF EXISTS {table}"), None).await.unwrap();
    conn.query(&format!("CREATE TABLE {table} (k UInt8, id UInt32) ENGINE = MergeTree ORDER BY k"), None)
        .await
        .unwrap();
    for part in 0..4 {
        let sql = format!("INSERT INTO {table} SELECT number % 3, number + {} FROM numbers(2000)", part * 2000);
        conn.query(&sql, None).await.unwrap();
    }
    let st = conn.table_structure("shop", "paging_dupes").await.unwrap();
    let sort = sqlgen::with_tiebreaker(&[SortKey { column: "k".into(), descending: true }], &st.paging_key());
    let mut seen = std::collections::HashSet::new();
    for offset in (0..8000).step_by(1000) {
        let sql = sqlgen::select_page_sql(conn.dialect(), "shop", "paging_dupes", None, &sort, Some(1000), offset);
        for row in rows(&conn, &sql).await.rows {
            assert!(seen.insert(row[1].to_text()), "row {:?} on two pages", row[1]);
        }
    }
    assert_eq!(seen.len(), 8000);
    conn.query(&format!("DROP TABLE {table}"), None).await.unwrap();
}
