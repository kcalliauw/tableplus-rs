//! DuckDB through the real helper: every test connects with `driver::connect`, which starts the
//! `tableplus-duckdb` binary Cargo built for this test and talks to it over the pipe.

#![allow(clippy::approx_constant)]

#[path = "../../core/tests/common/mod.rs"]
mod common;

use std::sync::Once;
use std::time::{Duration, Instant};

use common::*;
use tableplus_core::config::{ConnectionConfig, Credentials, DriverKind};
use tableplus_core::driver::{self, DynConnection};
use tableplus_core::model::{ConstraintKind, ObjectKind};
use tableplus_core::sqlgen::{self, SortKey};
use tableplus_core::{Dialect, Flavor, StatementResult, Value};

fn use_built_helper() {
    static SET: Once = Once::new();
    // SAFETY: set once, before any test starts a helper or reads the environment.
    SET.call_once(|| unsafe { std::env::set_var("TABLEPLUS_DUCKDB_HELPER", env!("CARGO_BIN_EXE_tableplus-duckdb")) });
}

fn duckdb_config(path: &std::path::Path) -> ConnectionConfig {
    use_built_helper();
    let mut cfg = ConnectionConfig::new(DriverKind::DuckDb);
    cfg.name = "test duckdb".into();
    cfg.path = path.to_string_lossy().into_owned();
    cfg
}

async fn connect(cfg: &ConnectionConfig) -> DynConnection {
    driver::connect(cfg, &Credentials::default()).await.unwrap_or_else(|e| panic!("{}: {e:#}", cfg.path))
}

/// Creates the seeded shop database in `dir` through the helper and returns its config.
async fn duckdb_fixture(dir: &std::path::Path) -> ConnectionConfig {
    let cfg = duckdb_config(&dir.join("shop.duckdb"));
    let script = include_str!("../../../docker/seed/duckdb.sql");
    let stmts = tableplus_core::export::sql_script_statements(script, Dialect::DuckDb);
    connect(&cfg).await.execute_in_transaction(&stmts).await.unwrap();
    cfg
}

fn text(rs: &tableplus_core::ResultSet, row: usize, column: &str) -> Value {
    rs.rows[row][rs.column_index(column).unwrap_or_else(|| panic!("no column {column}"))].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_browses_the_shop() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = duckdb_fixture(dir.path()).await;
    let conn = connect(&cfg).await;
    assert!(conn.server_version().starts_with("DuckDB 1.5"), "{}", conn.server_version());
    assert_eq!((conn.server_info().flavor, conn.server_info().version.major), (Flavor::DuckDb, 1));
    assert_eq!(conn.dialect(), Dialect::DuckDb);
    assert_eq!(conn.database(), "shop.duckdb");
    assert_eq!(conn.list_schemas().await.unwrap(), vec!["main"]);
    assert_eq!(conn.list_databases().await.unwrap(), vec!["shop"]);

    let objects = conn.list_objects().await.unwrap();
    let kind = |n: &str| objects.iter().find(|o| o.name == n).map(|o| o.kind);
    assert_eq!(kind("users"), Some(ObjectKind::Table));
    assert_eq!(kind("order_summary"), Some(ObjectKind::View));
    assert_eq!(kind("order_value"), Some(ObjectKind::Function));
    assert_eq!(kind("orders_of"), Some(ObjectKind::Function));
    assert_eq!(kind("duckdb_tables"), None, "internal objects are hidden");

    let users = conn.table_structure("main", "users").await.unwrap();
    assert_eq!(users.primary_key(), vec!["id"]);
    assert_eq!(users.column("id").unwrap().default.as_deref(), Some("nextval('users_id_seq')"));
    assert_eq!(users.column("country").unwrap().default.as_deref(), Some("'US'"));
    assert_eq!(users.column("balance").unwrap().data_type, "DECIMAL(12,2)");
    assert!(!users.column("email").unwrap().nullable);
    assert_eq!(users.comment.as_deref(), Some("Shop customers"));
    let idx = users.indexes.iter().find(|i| i.name == "users_country_idx").expect("secondary index");
    assert_eq!(idx.columns, ["country"]);
    assert!(idx.definition.as_deref().unwrap().starts_with("CREATE INDEX users_country_idx"));
    assert!(users.constraints.iter().any(|c| c.kind == ConstraintKind::Unique && c.columns == ["email"]));

    let items = conn.table_structure("main", "order_items").await.unwrap();
    assert_eq!(items.primary_key(), vec!["order_id", "product_id"]);
    let fks: Vec<_> = items.constraints.iter().filter(|c| c.kind == ConstraintKind::ForeignKey).collect();
    assert_eq!(fks.len(), 2);
    assert!(fks.iter().any(|f| f.columns == ["order_id"] && f.definition == "REFERENCES \"orders\" (\"id\")"));
    assert!(items.constraints.iter().any(|c| c.kind == ConstraintKind::Check && c.definition.contains("quantity > 0")));

    let categories = objects.iter().find(|o| o.name == "categories").unwrap();
    let ddl = conn.object_definition(categories).await.unwrap();
    assert!(ddl.contains("FOREIGN KEY (\"parent_id\") REFERENCES \"categories\" (\"id\")"), "{ddl}");
    let view = objects.iter().find(|o| o.name == "active_users").unwrap();
    assert!(conn.object_definition(view).await.unwrap().starts_with("CREATE VIEW active_users"));
    let mac = objects.iter().find(|o| o.name == "order_value").unwrap();
    let mac = conn.object_definition(mac).await.unwrap();
    assert!(mac.starts_with("CREATE MACRO order_value(qty, price) AS round"), "{mac}");

    // Row for row the SQLite shop (docs/duckdb-research.md, section 6).
    let sums = rows(
        &conn,
        "SELECT count(*), sum(total), (SELECT sum(balance) FROM users), \
                           (SELECT count(*) FROM order_items), (SELECT full_name FROM users WHERE id = 123), \
                           (SELECT max(placed_at) FROM orders) FROM orders",
    )
    .await;
    assert_eq!(sums.rows[0][0], Value::Int(20_000));
    assert!(matches!(sums.rows[0][1], Value::Float(f) if (f - 49_923_728.75).abs() < 0.01), "{:?}", sums.rows[0][1]);
    assert_eq!(sums.rows[0][2], Value::Decimal("2499975.00".into()));
    assert_eq!(sums.rows[0][3], Value::Int(40_001));
    assert_eq!(sums.rows[0][4], Value::text("Linus Turing"));
    assert_eq!(sums.rows[0][5], Value::text("2024-05-05 00:00:00"));

    assert_eq!(count(&conn, "main", "orders", None).await, 20_000);
    let page = rows(
        &conn,
        &sqlgen::select_page_sql(
            conn.dialect(),
            "main",
            "orders",
            None,
            &[SortKey { column: "id".into(), descending: true }],
            Some(100),
            100,
        ),
    )
    .await;
    assert_eq!(page.rows[0][0], Value::Int(19_900));
    assert_eq!(text(&page, 0, "status"), Value::text("pending"));
    assert_eq!(page.columns[2].type_name, "ENUM");
    match conn.query("SELECT * FROM orders", Some(10)).await.unwrap() {
        StatementResult::Rows(rs) => assert!(rs.truncated && rs.rows.len() == 10),
        other => panic!("{other:?}"),
    }
    match conn.query("FROM orders LIMIT 10", Some(10)).await.unwrap() {
        StatementResult::Rows(rs) => assert!(!rs.truncated && rs.rows.len() == 10, "FROM-first SELECT"),
        other => panic!("{other:?}"),
    }
    let err = conn.query("SELECT * FROM nope", None).await.unwrap_err().to_string();
    assert!(err.contains("Table with name nope does not exist"), "{err}");

    timed("duckdb streaming 20k rows", exercise_streaming(&conn, "SELECT * FROM orders", 20_000)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_renders_every_type_as_duckdb_prints_it() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = duckdb_fixture(dir.path()).await;
    let conn = connect(&cfg).await;
    let rs = rows(&conn, "SELECT * FROM type_zoo ORDER BY id").await;
    let expect = [
        ("c_int", Value::Int(42), "INTEGER"),
        ("c_real", Value::Float(3.14), "DOUBLE"),
        ("c_text", Value::text("héllo wörld ✓"), "VARCHAR"),
        ("c_blob", Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef].into()), "BLOB"),
        ("c_bool", Value::Bool(true), "BOOLEAN"),
        ("c_date", Value::text("2024-02-29"), "DATE"),
        ("c_hugeint", Value::Decimal("170141183460469231731687303715884105727".into()), "HUGEINT"),
        ("c_ubigint", Value::UInt(u64::MAX), "UBIGINT"),
        ("c_decimal", Value::Decimal("12345678901234567890.0123456789".into()), "DECIMAL(30,10)"),
        ("c_uuid", Value::text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"), "UUID"),
        ("c_time", Value::text("13:37:00.123456"), "TIME"),
        ("c_timetz", Value::text("13:37:00+02"), "TIME WITH TIME ZONE"),
        ("c_ts_ns", Value::text("2024-02-29 13:37:00.123456789"), "TIMESTAMP_NS"),
        ("c_interval", Value::text("1 year 2 months 3 days 04:05:06.789"), "INTERVAL"),
        ("c_list", Value::text("[1, 2, NULL]"), "INTEGER[]"),
        ("c_struct", Value::text("{'a': 1, 'b': x}"), "STRUCT(a INTEGER, b VARCHAR)"),
        ("c_map", Value::text("{k1=1, k2=2}"), "MAP(VARCHAR, INTEGER)"),
        ("c_enum", Value::text("paid"), "ENUM"),
        ("c_json", Value::text("{\"tags\": [\"a\", \"b\"], \"n\": 1}"), "JSON"),
        ("c_bit", Value::text("10110"), "BIT"),
    ];
    for (column, value, ty) in expect {
        let i = rs.column_index(column).unwrap();
        assert_eq!((column, &rs.rows[0][i]), (column, &value));
        assert_eq!((column, rs.columns[i].type_name.as_str()), (column, ty));
    }
    assert!(rs.rows[1][1..].iter().all(Value::is_null), "{:?}", rs.rows[1]);
    assert_eq!(text(&rs, 2, "c_text"), Value::text("multi\nline"));
    assert_eq!(text(&rs, 2, "c_struct"), Value::text(r"{'a': NULL, 'b': 'it\'s'}"));
    assert_eq!(text(&rs, 2, "c_list"), Value::text("[]"));
    assert!(rs.columns.iter().find(|c| c.name == "c_hugeint").unwrap().is_numeric());

    // Duplicate names and order survive the cast to text.
    let dup =
        rows(&conn, "SELECT a.id, b.c_date AS id FROM type_zoo a JOIN type_zoo b USING (id) ORDER BY 1 DESC").await;
    assert_eq!(dup.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "id"]);
    assert_eq!(
        dup.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        [Value::Int(3), Value::Int(2), Value::Int(1)]
    );
    // Statements that can't be wrapped still read every cell.
    let pragma = rows(&conn, "PRAGMA table_info('type_zoo')").await;
    assert_eq!(pragma.rows.len(), 21);
    let returning = rows(
        &conn,
        "INSERT INTO type_zoo (id, c_date, c_list) VALUES (9, '2020-01-01', [7]) RETURNING id, c_date, c_list",
    )
    .await;
    assert_eq!(returning.rows[0][..2], [Value::Int(9), Value::text("2020-01-01")]);
    let desc = rows(&conn, "DESCRIBE type_zoo").await;
    assert_eq!(desc.rows.len(), 21);
    let summarize = rows(&conn, "SUMMARIZE users").await;
    assert_eq!(text(&summarize, 0, "column_name"), Value::text("id"));
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_statement_results() {
    use_built_helper();
    let conn = connect(&duckdb_config(std::path::Path::new(":memory:"))).await;
    assert_eq!(conn.database(), ":memory:");
    let affected = |r: StatementResult| match r {
        StatementResult::Affected(n) => n,
        StatementResult::Rows(rs) => panic!("expected a count, got {:?}", rs.columns),
    };
    assert_eq!(affected(conn.query("CREATE TABLE t (a INTEGER)", None).await.unwrap()), 0);
    assert_eq!(affected(conn.query("INSERT INTO t SELECT * FROM range(5)", None).await.unwrap()), 5);
    assert_eq!(affected(conn.query("UPDATE t SET a = a + 1 WHERE a > 2", None).await.unwrap()), 2);
    assert_eq!(affected(conn.query("SET threads = 2", None).await.unwrap()), 0);
    assert_eq!(affected(conn.query("BEGIN", None).await.unwrap()), 0);
    assert_eq!(affected(conn.query("DELETE FROM t", None).await.unwrap()), 5);
    assert_eq!(affected(conn.query("ROLLBACK", None).await.unwrap()), 0);
    // Several statements run in order; the last one's result comes back.
    let last = rows(&conn, "INSERT INTO t VALUES (10); SELECT count(*) AS n FROM t").await;
    assert_eq!(last.rows[0][0], Value::Int(6));
    assert_eq!(
        conn.execute_in_transaction(&["INSERT INTO t VALUES (1), (2)".into(), "DELETE FROM t WHERE a = 10".into()])
            .await
            .unwrap(),
        3
    );
    let err = conn
        .execute_in_transaction(&["INSERT INTO t VALUES (1)".into(), "INSERT INTO nope VALUES (1)".into()])
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("statement 2 failed"), "{err:#}");
    assert_eq!(
        rows(&conn, "SELECT count(*) FROM t").await.rows[0][0],
        Value::Int(7),
        "the failed transaction rolled back"
    );
    // No extension is downloaded: ICU isn't built in, and installing it is the user's call.
    let err =
        conn.query("SELECT now()::TIMESTAMPTZ AT TIME ZONE 'Europe/Brussels'", None).await.unwrap_err().to_string();
    assert!(err.contains("INSTALL icu"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_cancel_interrupts_a_running_query() {
    use_built_helper();
    let conn = connect(&duckdb_config(std::path::Path::new(":memory:"))).await;
    let slow = {
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.query("SELECT count(*) FROM range(1000000000000) a, range(10) b WHERE a.range % 7 = b.range", None)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked = Instant::now();
    conn.cancel().await.unwrap();
    let err = slow.await.unwrap().unwrap_err().to_string();
    assert!(err.contains("Interrupted"), "{err}");
    assert!(asked.elapsed() < Duration::from_secs(2), "stopped {:?} after the cancel", asked.elapsed());
    assert_eq!(rows(&conn, "SELECT 42").await.rows[0][0], Value::Int(42), "the session still works");
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_sessions_on_one_file_share_a_helper() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = duckdb_fixture(dir.path()).await;
    let first = connect(&cfg).await;
    // A second process could not open the file while the first holds it read-write.
    let second = connect(&cfg).await;
    first.query("CREATE TABLE shared_probe AS SELECT 1 AS x", None).await.unwrap();
    assert_eq!(rows(&second, "SELECT x FROM shared_probe").await.rows[0][0], Value::Int(1));
    let version = driver::test_connection(&cfg, &Credentials::default()).await.unwrap();
    assert!(version.starts_with("DuckDB "), "the Test button reuses the open helper: {version}");
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_read_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = duckdb_fixture(dir.path()).await;
    cfg.read_only = true;
    let conn = connect(&cfg).await;
    assert_eq!(count(&conn, "main", "users", None).await, 5_000);
    let err = conn.query("DELETE FROM users", None).await.unwrap_err().to_string();
    assert!(err.contains("read-only"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_opens_data_files_as_views() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = duckdb_fixture(dir.path()).await;
    let conn = connect(&cfg).await;
    let parquet = dir.path().join("orders.parquet");
    let csv = dir.path().join("Order Items.csv");
    let json = dir.path().join("users.json");
    let quote = |p: &std::path::Path| Dialect::DuckDb.quote_str(&p.to_string_lossy());
    conn.query(&format!("COPY orders TO {} (FORMAT parquet)", quote(&parquet)), None).await.unwrap();
    conn.query(&format!("COPY (FROM order_items LIMIT 100) TO {} (HEADER)", quote(&csv)), None).await.unwrap();
    conn.query(&format!("COPY (FROM users LIMIT 10) TO {} (FORMAT json)", quote(&json)), None).await.unwrap();

    for (path, view, expected) in [(&parquet, "orders", 20_000), (&csv, "Order Items", 100), (&json, "users", 10)] {
        let file = connect(&duckdb_config(path)).await;
        assert_eq!(file.database(), path.file_name().unwrap().to_string_lossy());
        let objects = file.list_objects().await.unwrap();
        assert_eq!(objects.iter().map(|o| (o.name.as_str(), o.kind)).collect::<Vec<_>>(), [(view, ObjectKind::View)]);
        assert_eq!(count(&file, "main", view, None).await, expected, "{view}");
        let st = file.table_structure("main", view).await.unwrap();
        assert!(!st.columns.is_empty());
    }
    let orders = connect(&duckdb_config(&parquet)).await;
    let page = rows(&orders, "SELECT id, status, placed_at FROM orders ORDER BY id LIMIT 1").await;
    assert_eq!(page.rows[0], vec![Value::Int(1), Value::text("paid"), Value::text("2024-01-01 00:09:00")].into());
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_missing_helper_says_so() {
    let mut cfg = ConnectionConfig::new(DriverKind::DuckDb);
    cfg.path = ":memory:".into();
    let program = std::env::temp_dir().join("tableplus-duckdb-does-not-exist");
    // SAFETY: only this test reads this variable.
    unsafe { std::env::set_var("TABLEPLUS_TEST_NO_HELPER", &program) };
    let engine = Box::leak(Box::new(tableplus_core::engines::HelperEngine {
        env_var: "TABLEPLUS_TEST_NO_HELPER",
        ..tableplus_core::engines::DUCKDB
    }));
    let err =
        tableplus_core::driver::remote::RemoteConnection::open(engine, &cfg).await.err().expect("no helper there");
    assert!(format!("{err:#}").contains("tableplus-duckdb-does-not-exist"), "{err:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_editing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = duckdb_fixture(dir.path()).await;
    let conn = connect(&cfg).await;
    exercise_editing(&conn, "main").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_edits_duckdb_values() {
    use tableplus_core::alter::{ColumnDef, StructureChange, alter_statements};
    use tableplus_core::sqlgen::{CellEdit, RowIdentity};
    const D: Dialect = Dialect::DuckDb;
    let dir = tempfile::tempdir().unwrap();
    let cfg = duckdb_fixture(dir.path()).await;
    let conn = connect(&cfg).await;
    let affected = |r: StatementResult| match r {
        StatementResult::Affected(n) => n,
        StatementResult::Rows(rs) => panic!("expected a count, got {:?}", rs.columns),
    };

    // A row without a primary key is found through the text the grid shows, timestamps included.
    let log = conn.table_structure("main", "audit_log").await.unwrap();
    let first = rows(&conn, "SELECT * FROM audit_log ORDER BY happened_at LIMIT 1").await;
    let identity = RowIdentity::new(&log.columns, &first.rows[0]);
    let sql = sqlgen::update_sql(
        D,
        "main",
        "audit_log",
        &log.columns,
        &identity,
        &[("actor".into(), CellEdit::Text("koen".into()))],
    );
    assert_eq!(affected(conn.query(&sql, None).await.unwrap()), 1, "{sql}");
    assert_eq!(count(&conn, "main", "audit_log", Some("actor = 'koen'")).await, 1);

    // Lists, structs, maps, enums and blobs typed as text are coerced by DuckDB.
    let zoo = conn.table_structure("main", "type_zoo").await.unwrap();
    let one = rows(&conn, "SELECT * FROM type_zoo WHERE id = 1").await;
    let edits = [
        ("c_list", "[4, 5]"),
        ("c_struct", "{'a': 7, 'b': y}"),
        ("c_map", "{k3=3}"),
        ("c_enum", "shipped"),
        ("c_blob", "0xCAFE"),
        ("c_interval", "2 days"),
    ];
    let changes: Vec<(String, CellEdit)> =
        edits.iter().map(|(c, v)| (c.to_string(), CellEdit::Text(v.to_string()))).collect();
    let sql = sqlgen::update_sql(
        D,
        "main",
        "type_zoo",
        &zoo.columns,
        &RowIdentity::new(&zoo.columns, &one.rows[0]),
        &changes,
    );
    assert_eq!(affected(conn.query(&sql, None).await.unwrap()), 1, "{sql}");
    let back =
        rows(&conn, "SELECT c_list, c_struct, c_map, c_enum, c_blob, c_interval FROM type_zoo WHERE id = 1").await;
    assert_eq!(
        back.rows[0],
        vec![
            Value::text("[4, 5]"),
            Value::text("{'a': 7, 'b': y}"),
            Value::text("{k3=3}"),
            Value::text("shipped"),
            Value::Bytes(vec![0xCA, 0xFE].into()),
            Value::text("2 days"),
        ]
        .into()
    );

    // Renaming a column of an indexed table rebuilds it, keeping its comment, indexes and keys.
    let products = conn.table_structure("main", "products").await.unwrap();
    conn.query("CREATE INDEX products_name_idx ON products (name)", None).await.unwrap();
    conn.query("COMMENT ON COLUMN products.stock IS 'units on hand'", None).await.unwrap();
    let products = conn.table_structure("main", "products").await.map(|p| (p, products)).unwrap().0;
    let mut renamed = ColumnDef::from(products.column("name").unwrap());
    renamed.name = "title".into();
    let ddl = alter_statements(
        D,
        "main",
        "products",
        &products,
        &[StructureChange::ModifyColumn { original: "name".into(), new: renamed }],
    )
    .unwrap();
    assert!(ddl[0].starts_with("CREATE TABLE \"main\".\"_tableplus_rebuild_products\""), "{ddl:#?}");
    // products is referenced by order_items, so DuckDB refuses to drop it and nothing changes.
    let err = format!("{:#}", conn.execute_in_transaction(&ddl).await.unwrap_err());
    assert!(err.contains("another table's foreign key") && err.contains("order_items"), "{err}");
    let unchanged = conn.table_structure("main", "products").await.unwrap();
    assert!(unchanged.column("name").is_some() && unchanged.column("title").is_none(), "rolled back");
    assert_eq!(count(&conn, "main", "products", None).await, 300);

    // A table nothing references is rebuilt.
    conn.query(
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body VARCHAR NOT NULL UNIQUE, n INTEGER CHECK (n > 0))",
        None,
    )
    .await
    .unwrap();
    conn.query("INSERT INTO notes VALUES (1, 'a', 1), (2, 'b', 2)", None).await.unwrap();
    conn.query("CREATE INDEX notes_n_idx ON notes (n)", None).await.unwrap();
    conn.query("COMMENT ON TABLE notes IS 'scratch'", None).await.unwrap();
    let notes = conn.table_structure("main", "notes").await.unwrap();
    let mut text = ColumnDef::from(notes.column("body").unwrap());
    text.name = "text".into();
    text.nullable = true;
    let ddl = alter_statements(
        D,
        "main",
        "notes",
        &notes,
        &[StructureChange::ModifyColumn { original: "body".into(), new: text }],
    )
    .unwrap();
    conn.execute_in_transaction(&ddl).await.unwrap_or_else(|e| panic!("{ddl:#?}: {e:#}"));
    let notes = conn.table_structure("main", "notes").await.unwrap();
    assert!(notes.column("text").unwrap().nullable);
    assert_eq!(notes.primary_key(), vec!["id"]);
    assert_eq!(notes.comment.as_deref(), Some("scratch"));
    assert!(notes.indexes.iter().any(|i| i.name == "notes_n_idx"));
    assert!(
        notes.constraints.iter().any(|c| c.kind == ConstraintKind::Unique && c.columns == ["text"]),
        "{:?}",
        notes.constraints
    );
    assert!(notes.constraints.iter().any(|c| c.kind == ConstraintKind::Check));
    assert_eq!(
        rows(&conn, "SELECT text FROM notes ORDER BY id").await.rows,
        vec![vec![Value::text("a")].into(), vec![Value::text("b")].into()]
    );

    // Defaults and comments change in place, and constraints can't be dropped.
    let mut default = ColumnDef::from(notes.column("n").unwrap());
    default.default = Some("5".into());
    default.comment = Some("count".into());
    let ddl = alter_statements(
        D,
        "main",
        "notes",
        &notes,
        &[StructureChange::ModifyColumn { original: "n".into(), new: default }],
    )
    .unwrap();
    assert!(ddl.iter().all(|s| !s.contains("_tableplus_rebuild_")), "{ddl:#?}");
    conn.execute_in_transaction(&ddl).await.unwrap();
    let n = conn.table_structure("main", "notes").await.unwrap();
    assert_eq!(
        (n.column("n").unwrap().default.as_deref(), n.column("n").unwrap().comment.as_deref()),
        (Some("5"), Some("count"))
    );
    let drop = StructureChange::DropConstraint { name: "x".into(), kind: ConstraintKind::Unique };
    assert!(alter_statements(D, "main", "notes", &n, &[drop]).is_err());
    assert!(!D.drops_constraints());
}
