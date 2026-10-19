# DuckDB driver research

Research for a full-featured DuckDB driver in tableplus-rs, on the `duckdb` branch (built on `visualizer`). Nothing in the app was changed. Scratch files, captured plans and scripts are in `/tmp/duckdb-research/`.

Versions in this document:

- DuckDB **1.5.6** is the current stable release, published 2026-09-28 ([release](https://github.com/duckdb/duckdb/releases/tag/v1.5.6)).
- The Rust crate `duckdb` **1.10506.1** (2026-10-16, MSRV 1.88) bundles exactly DuckDB 1.5.6. The version `1.MMmmpp.x` encodes DuckDB `MM.mm.pp` ([README](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/README.md#L169-L173)), and its submodule pins commit `069cc9f9b5`, the `v1.5.6` tag.
- Every result marked **[ran]** was run on an Apple M5 Max (18 cores), either with the Homebrew CLI `duckdb v1.5.6 (Variegata) 069cc9f9b5` or with a Rust probe built against `duckdb = "=1.10506.1"` with the `bundled` feature (`/tmp/duckdb-research/probe/`). The probe printed `v1.5.6`, so both are the version the app would ship.

## 1. Summary

**Short answer.** A DuckDB driver fits the existing design: one more `Connection` implementation, modelled on the SQLite driver (embedded, file-based, a blocking library behind `spawn_blocking`), plus a `Dialect`, a `Flavor`, a plan parser and a handful of UI branches. DuckDB's `EXPLAIN (ANALYZE, FORMAT JSON)` returns a real operator tree with estimated rows, actual rows and per-operator time, so the tree, graph and flame chart views work without engine-specific UI code.

**Recommended approach.**

1. Use the official `duckdb` crate with the `bundled`, `json` and `parquet` features. Call the raw C API (`duckdb::ffi`) for the four things the crate hides: the statement count before running, the statement type, one shared instance per file, and query progress.
2. Treat a DuckDB file like a SQLite file in the UI, but keep one DuckDB instance per file path for the whole app, and open each session with `try_clone()`.
3. Ask for plans with `EXPLAIN (FORMAT JSON)` and `EXPLAIN (ANALYZE, FORMAT JSON)`, run Explain Analyze inside `BEGIN TRANSACTION READ ONLY` … `ROLLBACK`, and label times as thread time scaled to the statement's latency, as SQLite's cycle shares are labelled today.
4. Keep the seed as `docker/seed/duckdb.sql` and build `testdata/shop.duckdb` with an example binary, exactly as `shop.sqlite` is built today. A working draft seed is in `/tmp/duckdb-research/duckdb.sql`; it reproduces the SQLite shop row for row.

**Size of the work.** About 2,500 to 3,000 lines including tests, in four phases (section 7): connect and browse, then editing, then plans, then DuckDB-only features. For comparison, `driver/sqlite.rs` is 651 lines and `plan/postgres_json.rs` is 486. The binary grows by about 31 to 34 MB: the release app is 20.4 MB today (`target/release/tableplus-rs`, built 2026-10-18), and a stripped hello-world with bundled DuckDB is 31.8 MiB, 34.4 MiB with `json` and `parquet`. A cold release build of libduckdb-sys takes 64 to 87 s wall time and about 784 s of CPU on 18 cores.

**Findings that shape the design**, each detailed below:

- The crate's `prepare("A; B")` **runs A** before returning B's statement ([inner_connection.rs#L135-L155](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/inner_connection.rs#L135-L155)). The existing "prepare first to refuse a second statement" guard would therefore execute the first statement. **[ran]**
- `EXPLAIN ANALYZE INSERT …` executes and commits the insert. `BEGIN TRANSACTION READ ONLY` blocks it, blocks `nextval`, and covers attached databases, but still lets `COPY … TO 'file'` write a file. **[ran]**
- Per-operator `operator_timing` is **exclusive** of children and **summed over threads**; `cpu_time` is the inclusive sum. On the window query, the operators add up to 14.9 to 18.3 ms of thread time for a statement that takes 2.4 to 2.6 ms. **[ran]**
- Several crate type conversions are lossy: `TIMETZ` drops its offset, `TIME_NS` panics, `BIT` arrives as internal bytes, `VARIANT` errors. **[ran]**
- The existing literal and LIKE rules produce wrong data on DuckDB: `X'DEAD'` is the text `xDEAD`, `'\xdead'::bytea` is 3 bytes, and `ILIKE '%\%%'` without `ESCAPE` doesn't escape. **[ran]**
- A second `Connection::open` of the same file in one process creates an independent instance and breaks file locking. Only one process may open a file read-write.
- The bundled library downloads extensions at runtime into `~/.duckdb/extensions/`. During this research it fetched `icu` and `parquet` over HTTP the first time a query needed them. **[ran]**

## 2. How drivers are built here

### The pattern

A driver is a struct that implements the `Connection` trait (`crates/core/src/driver/mod.rs:26-58`). Every method takes `&self`; the driver serializes access internally so tabs can share one connection. The UI never branches on the engine for data, plans or editing. It asks `conn.dialect()` for SQL spelling and `conn.server_info()` for version-dependent behaviour. Everything engine-specific lives in core:

- `driver/<engine>.rs` implements the trait.
- `dialect.rs` spells identifiers, literals and common types.
- `sqlgen.rs` and `alter.rs` generate DML and DDL from `Dialect`.
- `plan/request.rs` picks the EXPLAIN statement by `Flavor` and version; `plan/<format>.rs` parses its output into the one `Plan` model.

`DECISIONS.md` ("Database drivers: native async crates, text values") explains why values are kept as the server's own text where Rust has no natural type, and "Query plans" why every parser fills one model.

### The SQLite driver, and what a DuckDB driver mirrors

`driver/sqlite.rs` is the closest analogue: an embedded library with a blocking API.

| SQLite driver | DuckDB driver |
|---|---|
| `Arc<Mutex<rusqlite::Connection>>`, and `spawn_blocking` for every call (`sqlite.rs:28-35`, `111-121`) | Same. `duckdb::Connection` is `Send` but not `Sync` ([lib.rs#L270-L277](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/lib.rs#L270-L277)), and `Statement` is not `Send`, so all statement work stays inside the blocking closure. |
| `InterruptHandle` for `cancel()` (`sqlite.rs:101`, `436-439`) | `Connection::interrupt_handle()` returns an `Arc<InterruptHandle>` that is `Send + Sync` ([lib.rs#L651-L673](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/lib.rs#L651-L673)). Measured: the query failed with `INTERRUPT Error: Interrupted!` 0.2 s after the call. |
| Opens with `OpenFlags` and a 5 s busy timeout (`sqlite.rs:88-109`) | Opens through a per-path registry (section 3) with `Config::access_mode`, `threads` and `max_memory`. There is no busy timeout: a locked file fails at once with `IO Error: Could not set lock on file … Conflicting lock is held in <exe> (PID n)`. |
| `query_on`: `column_count() == 0` means "affected rows" (`sqlite.rs:60-81`) | Must differ. DuckDB returns a `Count` column for INSERT, UPDATE, DELETE and CREATE, and a `Success` column for SET, BEGIN, COMMIT and ATTACH **[ran]**. Column names exist only after execution ([column.rs](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/column.rs#L28-L165)). The driver needs the statement type from `duckdb_prepared_statement_type` through `ffi` to map these to `StatementResult::Affected`. |
| `query_batches` reads rows on a blocking thread and sends batches over a bounded channel (`sqlite.rs:354-393`) | Same shape, but `Statement::query()` materializes the **whole** result inside DuckDB before the first row. On 50 million rows the first row took 529 ms and 1,189 MB, against 0.5 ms and 22 MB through `stream_arrow()` (crate probe in `/tmp/duckdb-research/crate/`). Use the streaming path both for `query` with `max_rows` and for `query_batches`. |
| `execute_in_transaction` with `total_changes()` (`sqlite.rs:395-409`) | Same transaction shape. Sum each statement's `Count`; `execute()` returns it. |
| `list_schemas` from `PRAGMA database_list`, objects from `sqlite_master` (`sqlite.rs:150-175`) | `duckdb_schemas()`, `duckdb_tables()`, `duckdb_views()` and `duckdb_functions()` (section 4). |
| `table_structure` from `pragma_table_info`, `pragma_index_list` and `pragma_foreign_key_list`, with CHECK clauses parsed out of the CREATE text (`sqlite.rs:177-332`, `603-637`) | Simpler: `duckdb_columns()`, `duckdb_constraints()` (CHECK expressions, foreign-key columns and referenced tables as lists) and `duckdb_indexes()`. No text parsing. |
| `explain` runs setup, plan and reset in one blocking closure; `Explain::Measure` runs the statement under scan status through a raw statement wrapper (`sqlite.rs:411-434`, `456-600`) | Same closure shape with `Explain::Statement` only. The plan comes back as a result cell, so no timing wrapper is needed, but the driver needs `ffi` to count statements before running anything. |
| `scan_status_available` adds a capability flag to `ServerInfo` (`sqlite.rs:449-454`, `server.rs:82-84`) | Not needed. |

### Every place a new engine must touch

Core crate:

| Place | What changes |
|---|---|
| `config.rs:8-14` `enum DriverKind` | Add `DuckDb`, then `ALL` (`:17`), `label` (`:19-26`), `badge` (`:29-36`, for example `Dk`), `default_port` (`:38-44`, 0), `default_user` (`:46-52`, empty) and `dialect` (`:54-60`). |
| `config.rs:62-64` `is_file_based` | Include `DuckDb`. That switches the dialog, `display_name` and `summary` (`:262-283`) to the file path. `display_name` falls back to the literal `"SQLite"` (`:270`), which must become per driver. |
| `config.rs:216-234` `ConnectionConfig` | `path` is documented as the SQLite file. A DuckDB connection also needs an access mode, and maybe config options and attach statements (section 4), as new fields with `#[serde(default)]`. |
| `config.rs:308-321` `from_url` | Add `duckdb://`, and possibly `md:`. |
| `dialect.rs:8-12` `enum Dialect` | Add `DuckDb`. Then `quote_ident` (`:15-20`, double quotes as in Postgres), `qualified` (`:23-29`), `bytes_literal` (`:47-53`, must emit `from_hex('DEAD')`, section 3), `bool_literal` (`:55-62`, `TRUE`/`FALSE`), `name` (`:99-105`) and `common_types` (`:108-160`). `is_binary_type` (`:163-166`) already matches `BLOB`. |
| `server.rs:11-35` `enum Flavor` | Add `DuckDb` with `name` and `dialect`. `Version::parse` (`:52-68`) returns `None` for DuckDB's `v1.5.6` because the leading `v` is not a digit, so the driver must strip it. |
| `driver/mod.rs:21-23`, `63-69` | `pub mod duckdb;` and the `connect` arm. |
| `model.rs:149-176` `ColumnMeta::is_numeric` | Add `hugeint`, `uhugeint`, `utinyint`, `usmallint`, `uinteger`, `ubigint` and `bignum`/`varint`. |
| `sqlgen.rs:104-109` `text_cast` | `CAST(x AS TEXT)` works on DuckDB **[ran]**. |
| `sqlgen.rs:122-130` `like_expr` | DuckDB needs `ILIKE … ESCAPE '\'`. `'a%b' ILIKE '%\%%'` is `false` without the clause and `true` with it **[ran]**, so the Postgres branch would silently stop matching a literal `%` or `_`. |
| `sqlgen.rs:264-271` `select_page_sql` | `OFFSET n` without `LIMIT` works **[ran]**. |
| `sqlgen.rs:350-361` `where_one` | Tables without a primary key use `rowid = (SELECT rowid FROM t WHERE … LIMIT 1)`, as SQLite does. DuckDB's `rowid` is "stable within a transaction" ([docs](https://duckdb.org/docs/current/sql/statements/select#row-ids)), and staged edits commit in one transaction. |
| `sqlgen.rs:400-404` `insert_sql` | `INSERT … DEFAULT VALUES` works **[ran]**. |
| `alter.rs:91-160`, `162-209` | New `Dialect::DuckDb` arms. The Postgres arm's `RENAME COLUMN`, `ALTER COLUMN … TYPE … USING`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT` and `COMMENT ON COLUMN` are valid DuckDB, but most of them fail when the table has any index (section 4), and `DROP CONSTRAINT` is unsupported. |
| `sqltext.rs:249-362` tokenizer | DuckDB lexes like Postgres: nested block comments, `$$` strings and `e'…'` escapes all work, and backticks are a syntax error **[ran]**. Each `dialect == Dialect::Postgres` test in the tokenizer (`:272`, `:278`, `:281`, `:347`) must also accept `DuckDb`. |
| `sqltext.rs:37`, `:190` `KEYWORDS`, `FUNCTIONS` | DuckDB words such as `QUALIFY`, `PIVOT`, `UNPIVOT`, `SUMMARIZE` and `EXCLUDE` are missing. `duckdb_keywords()` returns 489 keywords and `duckdb_functions()` every function, so completion could read both from the database. |
| `completion.rs:148-154` `quote_if_needed` | DuckDB identifiers are case-insensitive even when quoted ([docs](https://duckdb.org/docs/current/sql/dialect/keywords_and_identifiers)): `SELECT * FROM users` finds a table created as `"Users"` **[ran]**. Unlike Postgres, uppercase names need no quotes. |
| `plan/mod.rs:36-65` `PlanFormat`, `:321-329` `PARSERS` | A `DuckDbJson` format and its parser module (section 5). |
| `plan/mod.rs:232-235` `unestimated` | DuckDB prints `"Estimated Cardinality": "0"` on the projection right above an `ORDER_BY`, which has no estimate of its own (`explain_json_agg.json`, `modern.explain.json`). Treat it like MySQL's `rows=0`. |
| `plan/request.rs:101-127`, `:138-157` | A `read_only(Flavor::DuckDb)` pair and the DuckDB rules (section 5). |
| `plan/request.rs:225-381` `StatementShape` | DuckDB-only forms: `FROM orders` (FROM-first SELECT) now reads as verb `FROM` and would be refused as a write; `DESCRIBE`, `SHOW` and `SUMMARIZE` are not plans. |
| `plan/access.rs:143-191` `access`, `extras` | DuckDB operator names to `Access` (section 5). |
| `plan/facts.rs:89-102` | `latency`, and in detailed mode `planner` and `physical_planner`, as plan-wide facts. |
| `export.rs` | CSV, JSON and SQL exports work unchanged. Parquet would be new (section 4). |
| `crates/core/examples/make_sqlite.rs`, `tests/common/mod.rs:73-82` | A `make_duckdb` example and a `duckdb_fixture` (section 6). |

App crate:

| Place | What changes |
|---|---|
| `conn_dialog.rs:105`, `:113-138` | The driver picker lays 132 px tiles in a 580 px dialog. Five tiles need 660 px plus spacing, so the row needs smaller tiles or a second row. |
| `conn_dialog.rs:178-190` | The file row's hint (`/path/to/database.sqlite`) and the Browse filter (`"SQLite"`: `sqlite`, `sqlite3`, `db`, `db3`) are SQLite-specific. DuckDB files usually end in `.duckdb` or `.ddb`. Read-only and `:memory:` options go here too. |
| `conn_dialog.rs:93`, `:356` | Validation follows `is_file_based` once it includes DuckDB. |
| `welcome.rs:21-28`, `:154` | Badge colour, and the empty-state copy that lists the engines. |
| `structure.rs:290`, `:532` | Comment editing and "drop constraint" are hidden for SQLite. DuckDB supports `COMMENT ON COLUMN` but not `DROP CONSTRAINT`, so the second must also hide for DuckDB. |
| `session.rs:122-134`, `sidebar.rs:73-74` | The sidebar has one level of schemas; DuckDB has `database.schema` (section 4). |
| `plan_view.rs`, `plan_graph.rs`, `plan_flame.rs` | No engine branches; only tests construct formats. |
| `scripts/demo-home.sh`, `scripts/screenshots.sh` | A saved DuckDB connection and the welcome-row offsets (section 6). |

## 3. The Rust crate and type mapping

### Crate facts (duckdb 1.10506.1)

- **Features** ([Cargo.toml](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/Cargo.toml#L19-L46), [docs.rs](https://docs.rs/crate/duckdb/1.10506.1/features)). `bundled` compiles DuckDB's C++ with the `cc` crate from 257 compilation units. `json` and `parquet` are separate features; without them the bundled library has only `core_functions` built in. `icu` and `autocomplete` need `bundled-cmake`, which only works from a git checkout, so they are unusable from crates.io. The README says ICU is left out because of crates.io's 10 MB package limit ([README#L296-L304](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/README.md#L296-L304)).
- **Build cost**, measured cold on 18 cores with rustc 1.99.1 and `strip = true`: `bundled` took 64.1 s wall (86.5 s on a second run) and 784 s CPU, for a 31.8 MiB binary; with `parquet` and `json`, 68.7 s and 34.4 MiB; a debug build 67.2 s and 126 MB. The libduckdb-sys build script is the critical path (78 s of 86.5 s). This repo builds dependencies at `opt-level = 2` even in dev (`Cargo.toml`, `[profile.dev.package."*"]`), so debug builds pay the full C++ optimisation cost.
- **Extensions at runtime.** Auto-install and auto-load are on in the bundled build ([build_bundled_cc.rs#L96-L97](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/libduckdb-sys/build_bundled_cc.rs#L96-L97)). During this research, the Rust probe downloaded `icu` (15 MB) and `parquet` (13 MB) from `extensions.duckdb.org` into `~/.duckdb/extensions/v1.5.6/osx_arm64/` the first time a query needed them, and the CLI's `INSTALL sqlite` added `sqlite_scanner` (27 MB). The bundled library and the CLI share that directory.
- **Without ICU**, `TIMESTAMPTZ` renders in UTC (`2024-02-29 12:37:00+00`) and `now() - INTERVAL 1 DAY` fails to bind. With ICU and `TimeZone = Europe/Brussels`, the CLI rendered the same value as `2024-02-29 13:37:00+01` **[ran]**.
- **Concurrency.** `try_clone()` opens another connection on the same database instance ([lib.rs#L699-L706](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/lib.rs#L699-L706)). The crate never uses DuckDB's instance cache, which became stable in 1.5.6 and "is necessary if a client/program (re)opens multiple databases to the same file within the same process" ([duckdb.h#L4487-L4518](https://github.com/duckdb/duckdb/blob/v1.5.6/src/include/duckdb.h#L4487-L4518)). Measured: a second in-process `Connection::open` of the same file succeeded as a separate instance, a table created through the first was invisible to the second, and after the second was dropped another process could open the file read-write while the first was still writing (macOS file locks are per process). The app needs one instance per canonical path, shared by every session and by the Test button.
- **Locking across processes.** One process may open a file read-write, or many may open it read-only ([concurrency](https://duckdb.org/docs/current/connect/concurrency)). While the DuckDB CLI or a notebook holds a file read-write, the app can't open it at all, not even read-only.
- **Multi-statement prepare.** `prepare()` calls `duckdb_extract_statements`, **executes every statement but the last**, and prepares the last ([inner_connection.rs#L135-L155](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/inner_connection.rs#L135-L155)). **[ran]** `prepare("CREATE TABLE prepared_side_effect(a INT); SELECT 1 AS x")` created the table. The driver must count statements with `ffi::duckdb_extract_statements` before calling `prepare`.
- **No statement type, no `readonly()`.** The crate uses `duckdb_prepared_statement_type` only privately ([raw_statement.rs#L230-L237](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/raw_statement.rs#L230-L237)). Through `ffi`, SELECT reports 1, INSERT 2, CREATE 7 and EXPLAIN 4, while PRAGMA, SHOW and DESCRIBE also report 1 ([statement types](https://github.com/duckdb/duckdb/blob/v1.5.6/src/include/duckdb.h#L319-L351)). No C function says whether a statement writes.
- **Column metadata.** Names and Arrow types are available only after execution. `column_logical_type(i)` works before execution and gives DuckDB's logical type, which the driver should use for `ColumnMeta.type_name`: Arrow reports `HUGEINT` as `Decimal128(38, 0)` and `JSON` as `Utf8` **[ran]**.
- **Progress.** The crate has no wrapper for `duckdb_query_progress` ([duckdb.h#L3700-L3711](https://github.com/duckdb/duckdb/blob/v1.5.6/src/include/duckdb.h#L3700-L3711)) and keeps the raw connection handle private. Progress also needs `SET enable_progress_bar = true` on the connection; otherwise it returns -1.
- **Alternatives.** The raw C API (`duckdb::ffi`, 546 bound functions) gives statement type, progress, the instance cache, streaming chunks and logical types, at the cost of `unsafe` code and hand-written decoders. ADBC (`adbc_driver_manager`) is Arrow-only. `async-duckdb` 0.3.1 is a thin thread-and-channel wrapper, and `quackdb` was last released in 2023. The official crate plus targeted `ffi` calls is the practical choice, the same way the SQLite driver uses `rusqlite::ffi` (`sqlite.rs:481-600`).

### Type mapping

Observed with the probe on the draft seed's `type_zoo` row **[ran]**. "Crate value" is `duckdb::types::Value`. "DuckDB text" is `CAST(x AS VARCHAR)`, the rendering the app's design calls for.

| DuckDB type | Crate value | DuckDB text | App `Value` | Notes |
|---|---|---|---|---|
| BOOLEAN | `Boolean(true)` | `true` | `Bool` | |
| TINYINT … BIGINT | `TinyInt` … `BigInt` | | `Int` | |
| UTINYINT … UBIGINT | `UTinyInt` … `UBigInt(18446744073709551615)` | | `UInt` | |
| HUGEINT | `HugeInt(i128)` | `170141183460469231731687303715884105727` | `Decimal(text)` | `Value` has no 128-bit variant; `Decimal` keeps it exact and right-aligned. |
| UHUGEINT | `UHugeInt(u128)` | | `Decimal(text)` | Inside a LIST it becomes `HugeInt(-1)`, a documented crate bug. |
| FLOAT, DOUBLE | `Float`, `Double` | `3.14` | `Float` | DuckDB's `REAL` is a 4-byte `FLOAT`, unlike SQLite's. |
| DECIMAL(w,s) | `Decimal { width, scale, value: i128 }` | `12345678901234567890.0123456789` | `Decimal(text)` | The crate's `Decimal` implements `Display`. A bare `DECIMAL` is `DECIMAL(18,3)` ([docs](https://duckdb.org/docs/current/sql/data_types/numeric)). |
| VARCHAR, JSON | `Text` | | `Text` | JSON needs the `json` extension; without the feature, reading a JSON column auto-installs it. |
| UUID | `Text` | | `Text` | |
| BLOB | `Blob(bytes)` | `\xDE\xAD\xBE\xEF` | `Bytes` | |
| BIT | `Blob([3, 246])` for `10110` | `10110` | `Text` after decoding | The crate hands over DuckDB's internal bytes; the first byte is the padding count. |
| BIGNUM (VARINT) | `Blob` | | `Decimal(text)` after decoding | 1.4.0 renamed `VARINT` to `BIGNUM` and kept the alias. |
| ENUM | `Enum("paid")` | `paid` | `Text` | |
| DATE | `Date32(days)` | `2024-02-29` | `Text` | `infinity` arrives as `2147483647`. |
| TIME | `Time64(Microsecond, µs)` | `13:37:00.123456` | `Text` | |
| TIME_NS | **panic** at [row.rs#L598](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/row.rs#L598) | | `Text` via cast | Must never reach the crate's converter. |
| TIMETZ | `Time64(Microsecond, …)` | `13:37:00+02` | `Text` via cast | **Offset silently dropped.** |
| TIMESTAMP, _S, _MS, _NS | `Timestamp(unit, i64)` | `2024-02-29 13:37:00.123456789` for _NS | `Text` | Format per unit; `infinity` exists. |
| TIMESTAMPTZ | `Timestamp(Microsecond, UTC instant)` | `2024-02-29 12:37:00+00` without ICU | `Text` | The zone comes from the `TimeZone` setting, which needs ICU. |
| INTERVAL | `Interval { months: 14, days: 3, nanos }` | `1 year 2 months 3 days 04:05:06.789` | `Text` | |
| LIST, ARRAY[n] | `List(..)`, `Array(..)` | `[1, 2, NULL]` | `Text` | |
| STRUCT | `Struct(OrderedMap)` | `{'a': 1, 'b': x}` | `Text` | |
| MAP | `Map(OrderedMap)` | `{k1=1, k2=2}` | `Text` | |
| UNION | `Union(Box<Value>)` | `hi` | `Text` | |
| VARIANT (1.5.0+) | error: "decoding Variant columns is not supported" | | `Text` via cast | |
| GEOMETRY (built in from 1.5) | `Geometry(WKB)` | | `Text` via cast, or `Bytes` | |

`Value` has no `Display` ([types/value.rs](https://github.com/duckdb/duckdb-rs/blob/v1.10506.1/crates/duckdb/src/types/value.rs#L12-L98)), so the driver needs its own formatter for every non-scalar variant, written to match DuckDB's cast output above. A user can also run `SET arrow_lossless_conversion = true`, after which UUID, TIMETZ and HUGEINT arrive as `Blob`, so the driver should decide by `column_logical_type`, not by the Arrow type.

For the lossy or failing types (TIMETZ, TIME_NS, BIT, BIGNUM, VARIANT) there are two ways out. One is to wrap a SELECT-shaped statement as `SELECT * REPLACE (CAST(c AS VARCHAR) AS c) FROM (…)` when `column_logical_type` shows one of them. The other is to read those columns through the C API's chunk interface, which is more code but covers every statement. `duckdb_value_to_string` became stable in 1.5.6 ([duckdb.h#L9465-L9476](https://github.com/duckdb/duckdb/blob/v1.5.6/src/include/duckdb.h#L9465-L9476)) but works on a `duckdb_value`, not on a result cell; the per-cell `duckdb_value_varchar(result, col, row)` has been deprecated since 1.0.0.

### Literals that need a DuckDB arm **[ran]**

| Value | Today | On DuckDB | DuckDB spelling |
|---|---|---|---|
| bytes, Postgres arm | `'\xdead'::bytea` | a 3-byte BLOB: `0xDE`, `a`, `d` | `from_hex('DEAD')` or `'\xDE\xAD'::BLOB` |
| bytes, MySQL and SQLite arm | `X'DEAD'` | the VARCHAR `xDEAD` | as above |
| bool | `TRUE` | works | `TRUE` |
| LIKE, Postgres arm | `ILIKE '%\%%'` | doesn't escape | `ILIKE '%\%%' ESCAPE '\'` |
| edited cell | `'text'`, coerced by the server | works, including `'[1, 2]'` against an `INTEGER[]` column | unchanged |

## 4. Feature checklist

"Existing" means the app already has the feature and DuckDB plugs into it. "New" means the app has no equivalent yet.

### Connection

| Feature | DuckDB mechanism | Status |
|---|---|---|
| Open a file | `Connection::open_with_flags(path, Config)` | Existing (SQLite's file row). |
| `:memory:` | path `:memory:` ([docs](https://duckdb.org/docs/current/connect/overview)) | New: a checkbox. The database disappears when the session closes. |
| Read-only | `Config::access_mode(AccessMode::ReadOnly)`; `access_mode` can't change at runtime **[ran]** | New: a checkbox. Several sessions or processes can then share the file. |
| Config options | `threads`, `memory_limit`, `temp_directory`, `max_temp_directory_size`, `enable_external_access`, `autoload_known_extensions`, `autoinstall_known_extensions`, `allow_unsigned_extensions`, `lock_configuration` ([overview](https://duckdb.org/docs/current/configuration/overview), [securing](https://duckdb.org/docs/current/operations_manual/securing_duckdb/overview)) | New: a key and value list. `enable_external_access` and `allow_unsigned_extensions` can't be re-enabled at runtime **[ran]**. |
| ATTACH more databases | `ATTACH 'f.duckdb' AS x (READ_ONLY)`, also `TYPE sqlite`, `TYPE postgres` and `TYPE mysql`, and Parquet or CSV files as views. Attachments are not persisted ([attach](https://duckdb.org/docs/current/sql/statements/attach)). | New: a list of attach statements run at connect. One transaction can write to only one attached database. |
| MotherDuck | `md:` path or `ATTACH 'md:'`; the token comes from the `motherduck_token` environment variable or `md:db?motherduck_token=…` ([MotherDuck docs](https://motherduck.com/docs/key-tasks/authenticating-and-connecting-to-motherduck/authenticating-to-motherduck/)); needs the `motherduck` extension, downloaded at runtime ([core extension](https://duckdb.org/docs/current/core_extensions/motherduck)) | New: a remote mode with the token in the Keychain. Not available on windows_arm64. |
| Extensions | `INSTALL x; LOAD x;`, `duckdb_extensions()` ([docs](https://duckdb.org/docs/current/extensions/overview)) | New: an extensions panel, or at least a policy (section 7). |
| Test connection | `test_connection` opens, pings and closes (`driver/mod.rs:153-157`) | Existing. It must go through the per-path instance registry, or testing a file that is open breaks its lock. |
| Cancel | `interrupt_handle()` | Existing. |
| Progress | `duckdb_query_progress` through `ffi`, with `enable_progress_bar` on | New: the app has no progress UI. |
| SSH tunnel, TLS | Not applicable | Existing UI hides them for file-based drivers. |

### Catalog

| Feature | DuckDB source ([table functions](https://duckdb.org/docs/current/sql/meta/duckdb_table_functions)) | Status |
|---|---|---|
| Databases | `duckdb_databases()`, minus `system` and `temp` | Existing method `list_databases`, unused by the UI today. |
| Schemas | `duckdb_schemas() WHERE database_name = current_database() AND NOT internal` | Existing (sidebar). |
| Tables, views | `duckdb_tables()`, `duckdb_views()` with `NOT internal` | Existing (`ObjectKind::Table`, `View`). |
| Macros | `duckdb_functions() WHERE function_type IN ('macro', 'table_macro') AND NOT internal`; body in `macro_definition` | Existing `ObjectKind::Function`. |
| Sequences, types | `duckdb_sequences()`, `duckdb_types()` | New object kinds, or leave out. |
| Columns | `duckdb_columns()`: `column_name`, `data_type`, `is_nullable`, `column_default`, `comment` | Existing (`ColumnInfo`). |
| Primary, unique, check and foreign keys | `duckdb_constraints()`: `constraint_type`, `constraint_name`, `constraint_column_names`, `expression`, `referenced_table`, `referenced_column_names` | Existing (`ConstraintInfo`). Build the foreign-key definition from the lists, not from `constraint_text` (quirk below). |
| Indexes | `duckdb_indexes()`: `index_name`, `is_unique`, `expressions`, `sql`. Lists secondary indexes only; key indexes appear as constraints. | Existing (`IndexInfo`). The docs say `expressions` is always NULL; 1.5.6 returns `[country]` **[ran]**. |
| DDL | `duckdb_tables().sql`, `duckdb_views().sql`, `duckdb_indexes().sql` | Existing (`object_definition`). |
| Table comment | `duckdb_tables().comment`, `COMMENT ON TABLE` ([docs](https://duckdb.org/docs/current/sql/statements/comment_on)) | Existing (`TableStructure.comment`). |
| Row count estimate | `duckdb_tables().estimated_size` | New, optional. |

Queries the driver can use, run against the draft seed **[ran]**:

```sql
SELECT schema_name, table_name AS name, 'table' AS kind FROM duckdb_tables()
 WHERE database_name = current_database() AND NOT internal
UNION ALL SELECT schema_name, view_name, 'view' FROM duckdb_views()
 WHERE database_name = current_database() AND NOT internal
UNION ALL SELECT schema_name, function_name, function_type FROM duckdb_functions()
 WHERE database_name = current_database() AND NOT internal AND function_type IN ('macro', 'table_macro');

SELECT column_name, data_type, is_nullable, column_default, comment FROM duckdb_columns()
 WHERE database_name = current_database() AND schema_name = 'main' AND table_name = 'users'
 ORDER BY column_index;
```

Two DuckDB 1.5.6 quirks the driver must work around **[ran]**:

- **A self-referencing foreign key is missing from the DDL.** For `CREATE TABLE c(id INT PRIMARY KEY, p INT REFERENCES c(id))`, `duckdb_tables().sql` returns `CREATE TABLE c(id INTEGER PRIMARY KEY, p INTEGER, );`, which is not valid SQL, and the constraint's `constraint_text` is empty. The structured columns are right: `[parent_id]`, `categories`, `[id]` for the seed's categories.
- **A named ENUM type is inlined.** `orders.status`, declared as `order_status`, reads back as `ENUM('pending', 'paid', …)` in both `duckdb_columns()` and `duckdb_tables().sql`. `duckdb_types()` still lists `order_status`.

**The sidebar has one level, DuckDB has two.** DuckDB names objects `database.schema.table`. `SchemaObject` has one `schema` string, and `Dialect::qualified` quotes it as one identifier (`dialect.rs:23-29`). Three options:

1. Show only `current_database()`. Attached databases are then reachable only from the SQL editor.
2. Flatten to `db.schema` labels, and teach `qualified` to quote `"db"."schema"."t"` for DuckDB. Every place that builds SQL from a `SchemaObject` already goes through `qualified`.
3. Add a `database` field to `SchemaObject`. Cleanest, widest change.

Option 2 is the smallest change that makes ATTACH useful; this is a decision for Koen (section 7).

### Editing

| Feature | DuckDB behaviour | Status |
|---|---|---|
| Staged UPDATE, INSERT and DELETE in one transaction | `BEGIN`/`COMMIT` with snapshot isolation; two connections writing the same row fail with `Transaction conflict` ([transactions](https://duckdb.org/docs/current/sql/statements/transactions)) | Existing. |
| Rows without a primary key | `rowid` pseudo-column; `UPDATE … WHERE rowid = …` works | Existing (the SQLite branch of `where_one`). |
| UPDATE of a key or indexed column | Rewritten as DELETE plus INSERT in chunks of 2,048 rows; `UPDATE t SET pk = pk + 1` over 3,000 rows fails with `Duplicate key` ([indexes](https://duckdb.org/docs/current/sql/indexes#constraint-checking-in-update-statements)) | Existing feature; one row at a time is fine. |
| Auto-increment | No `SERIAL`, no `AUTOINCREMENT`, no `GENERATED … AS IDENTITY`; the idiom is `DEFAULT nextval('seq')` ([sequences](https://duckdb.org/docs/current/sql/statements/create_sequence)) **[ran]** | Existing UI (`ColumnInfo.extra`); show a `nextval(…)` default as the hint. |
| SAVEPOINT | Syntax error | Not used by the app. |
| ALTER: add column, set or drop default | Works, even with indexes | Existing. |
| ALTER: rename, drop, retype, set or drop NOT NULL | Fails with `Dependency Error` when the table has **any** secondary index or is referenced by a foreign key ([alter docs](https://duckdb.org/docs/current/sql/statements/alter_table); tested on 1.5.6) | Existing feature with a DuckDB limit. Needs either a rebuild like SQLite's (`alter.rs:229-327`), which itself can't drop a table that another table references, or a clear error. |
| ADD PRIMARY KEY | Since 1.2.0 ([announcement](https://duckdb.org/2025/02/05/announcing-duckdb-120)) | Existing. |
| ADD or DROP UNIQUE, FOREIGN KEY, CHECK | `Not implemented Error: No support for that ALTER TABLE option yet!` **[ran]** | Existing feature; hide it for DuckDB as for SQLite (`structure.rs:532`). |
| Foreign key actions | `ON DELETE CASCADE` and `SET NULL` are rejected at CREATE time **[ran]** | The structure view shows plain `REFERENCES`. |
| Triggers | `CREATE TRIGGER` is a syntax error | Nothing to show. |
| Comments | `COMMENT ON TABLE`, `COLUMN`, `VIEW`, `INDEX`, `SEQUENCE`, `TYPE`, `MACRO`; worked on an indexed table in 1.5.6 although the docs say it doesn't | Existing (Postgres-style comment editing). |

### Import and export

| Feature | DuckDB mechanism | Status |
|---|---|---|
| Export CSV, JSON or SQL from a grid or table | App-side exporter over `query_batches` | Existing. |
| Export Parquet | `COPY (query) TO 'f.parquet' (FORMAT parquet)` ([COPY](https://duckdb.org/docs/current/sql/statements/copy)) | New format in the export dialog; needs the `parquet` feature. |
| Fast CSV export | `COPY … TO 'f.csv' (HEADER, DELIM ',')` | New, optional; much faster than exporting row by row. |
| Import CSV | App-side `csv_to_inserts` (`export.rs:201-271`) | Existing. `INSERT INTO t SELECT * FROM read_csv('f.csv')` with type sniffing ([auto detection](https://duckdb.org/docs/current/data/csv/auto_detection)) would be a faster new path. |
| Import Parquet or JSON | `read_parquet`, `read_json` | New. |
| Query files directly | `SELECT * FROM 'orders.parquet'` | Works in the SQL editor as is. |
| Dump the database | `EXPORT DATABASE 'dir' (FORMAT parquet)` writes `schema.sql`, `load.sql` and data; `IMPORT DATABASE` needs a fresh database ([export](https://duckdb.org/docs/current/sql/statements/export)) | New. |
| Copy a database | `COPY FROM DATABASE a TO b` | New. From the SQLite shop it kept primary keys, NOT NULL, defaults and indexes, and dropped UNIQUE, CHECK, foreign keys and both views **[ran]**. |

### Completion and dialect

| Feature | DuckDB mechanism | Status |
|---|---|---|
| Table and column completion | Catalog as above | Existing. |
| Keywords | `duckdb_keywords()`: 75 reserved, 330 unreserved, 49 column-name and 35 type-function keywords | Existing static list; DuckDB could load its own. |
| Functions | `duckdb_functions()` | New: the `Catalog` trait (`completion.rs:47-51`) has no functions. |
| Server-side completion | `sql_auto_complete(query)` in the `autocomplete` extension ([docs](https://duckdb.org/docs/current/core_extensions/autocomplete)) | Not bundled from crates.io; skip. |
| Statement splitting | Postgres rules **[ran]** | Existing tokenizer with DuckDB added to the Postgres branches. |
| FROM-first, `SUMMARIZE`, `PIVOT`, `QUALIFY`, `SELECT * EXCLUDE` | Valid DuckDB | Runs as is; highlighting and the Explain rules need the words. |

## 5. Explain and Explain Analyze

### Statements and settings

| Statement | What it returns (1.5.6) | Since |
|---|---|---|
| `EXPLAIN q` | Two VARCHAR columns: `explain_key` = `physical_plan` and `explain_value` = box-drawing text with `~N rows` estimates | before 1.0 |
| `EXPLAIN (FORMAT JSON) q` | Same columns; `explain_value` is a JSON array of root operators with `name`, `children` and `extra_info` | 1.1.0 ([PR #12967](https://github.com/duckdb/duckdb/pull/12967)) |
| `EXPLAIN (FORMAT html / graphviz / mermaid / yaml / text) q` | Same columns, other renderings. `FORMAT XML` is refused with the list `mermaid, default, graphviz, yaml, json, html, text` **[ran]** | html and graphviz 1.1.0, yaml 1.4.0 ([PR #17572](https://github.com/duckdb/duckdb/pull/17572)), mermaid 1.5.0 ([PR #19581](https://github.com/duckdb/duckdb/pull/19581)) |
| `EXPLAIN ANALYZE q` | `explain_key` = `analyzed_plan`; the value is the profile as box text | before 1.0 |
| `EXPLAIN (ANALYZE, FORMAT JSON) q` | Same columns; the value is the full JSON profile | the option syntax since 1.1.0; HTML and graphviz with ANALYZE since 1.2.0 ([PR #13942](https://github.com/duckdb/duckdb/pull/13942)). `FORMAT YAML` with ANALYZE fails with `No mapping from ExplainFormat::YAML to ProfilerPrintFormat` **[ran]** |
| `SET explain_output = 'physical_only' / 'optimized_only' / 'all'` | `all` returns three rows: `logical_plan`, `logical_opt`, `physical_plan` **[ran]** | [explain guide](https://duckdb.org/docs/current/guides/meta/explain.html) |
| `PRAGMA enable_profiling = 'json'` with `SET profiling_output = 'f.json'` | Profiles every following statement into the file | before 1.0 ([profiling](https://duckdb.org/docs/current/dev/profiling.html)) |
| `SET profiling_mode = 'detailed'` | Adds `planner`, `physical_planner` and one timing per optimizer at the root **[ran]** | 1.1.0 rework ([PR #11101](https://github.com/duckdb/duckdb/pull/11101)) |
| `SET custom_profiling_settings = '{"CPU_TIME": "true", …}'` | Chooses metrics; the 1.5.6 default enables every non-detailed metric **[ran]** | 1.1.0 |
| `CALL enable_profiling(format := 'json', metrics := […])` | Table-function form | 1.5.0 ([PR #20151](https://github.com/duckdb/duckdb/pull/20151)) |
| C API `duckdb_get_profiling_info` | The same tree as `duckdb_value` maps | 1.1.0 ([duckdb.h#L3597-L3654](https://github.com/duckdb/duckdb/blob/v1.5.6/src/include/duckdb.h#L3597-L3654)); not wrapped by the crate |

Other version changes in the JSON: `operator_name` beside `operator_type` arrived in 1.2.0 ([PR #14744](https://github.com/duckdb/duckdb/pull/14744)); `system_peak_buffer_memory` and `system_peak_temp_dir_size` became default in 1.3.0; 1.5.0 added the attach, checkpoint and WAL metrics and `total_memory_allocated`, put the catalog and schema into table names, and fixed `rows_scanned` ([PR #20161](https://github.com/duckdb/duckdb/pull/20161)). Release notes: [1.1.0](https://github.com/duckdb/duckdb/releases/tag/v1.1.0), [1.2.0](https://github.com/duckdb/duckdb/releases/tag/v1.2.0), [1.3.0](https://github.com/duckdb/duckdb/releases/tag/v1.3.0), [1.4.0](https://github.com/duckdb/duckdb/releases/tag/v1.4.0), [1.5.0](https://github.com/duckdb/duckdb/releases/tag/v1.5.0).

DuckDB is embedded, so the app only ever talks to the version it bundles, and the request rules need no version table, unlike MySQL's. MotherDuck is the exception: it runs part of a query on its own servers, and this research didn't capture its output.

### Real output (trimmed)

Shop queries are in `/tmp/duckdb-research/plans/` (`q_join`, `q_agg`, `q_cte`, `q_window`, `q_sub`); the lab's Northwind `portable` and `modern` queries, which ran on DuckDB unchanged, are in `/tmp/duckdb-research/lab/`. Each `*.json` file is the `explain_value` cell exactly as the Rust crate returns it; `profile_*.json` came from `PRAGMA enable_profiling = 'json'` in the CLI. `scripts/tree.py` prints any of them as a tree.

`EXPLAIN (FORMAT JSON)` for the join query (`explain_json_join.json`):

```json
[{"name": "TOP_N",
  "extra_info": {"Top": "10", "Order By": "o.total DESC"},
  "children": [{"name": "PROJECTION",
    "extra_info": {"Projections": ["full_name", "id", "total"], "Estimated Cardinality": "418"},
    "children": [{"name": "HASH_JOIN",
      "extra_info": {"Join Type": "INNER", "Conditions": "user_id = id", "Estimated Cardinality": "418"},
      "children": [
        {"name": "SEQ_SCAN", "children": [],
         "extra_info": {"Table": "shop.main.orders", "Type": "Sequential Scan",
                        "Projections": ["user_id", "id", "total"],
                        "Filters": "(CAST(status AS VARCHAR) = 'delivered')", "Estimated Cardinality": "4000"}},
        {"name": "SEQ_SCAN", "children": [],
         "extra_info": {"Table": "shop.main.users", "Type": "Sequential Scan",
                        "Projections": ["id", "full_name"], "Filters": "country='GB'",
                        "Estimated Cardinality": "1000"}}]}]}]}]
```

`EXPLAIN (ANALYZE, FORMAT JSON)` for the same query (`analyze_json_join.json`), with the root metrics and a few keys per operator kept:

```json
{"latency": 0.002601875, "cpu_time": 0.001458501, "rows_returned": 0,
 "cumulative_cardinality": 13010, "cumulative_rows_scanned": 21000, "blocked_thread_time": 0.0,
 "query_name": "EXPLAIN (ANALYZE, FORMAT JSON) SELECT u.full_name, …",
 "children": [{"operator_name": "EXPLAIN_ANALYZE", "operator_cardinality": 0, "cpu_time": 0.00145896,
  "children": [{"operator_name": "TOP_N", "operator_timing": 0.0000591, "cpu_time": 0.0014583,
                "operator_cardinality": 10,
   "children": [{"operator_name": "PROJECTION", "operator_cardinality": 4000,
    "children": [{"operator_name": "HASH_JOIN", "operator_timing": 0.000194208, "cpu_time": 0.0013985,
                  "operator_cardinality": 4000,
                  "extra_info": {"Join Type": "INNER", "Conditions": "user_id = id",
                                 "Estimated Cardinality": "418"},
     "children": [
       {"operator_name": "SEQ_SCAN", "operator_type": "TABLE_SCAN", "operator_timing": 0.000397542,
        "operator_cardinality": 4000, "operator_rows_scanned": 20000,
        "extra_info": {"Table": "shop_probe.main.orders", "Type": "Sequential Scan",
                       "Estimated Cardinality": "4000",
                       "Dynamic Filters": "optional: user_id>=2 AND optional: user_id<=4997"}},
       {"operator_name": "SEQ_SCAN", "operator_type": "TABLE_SCAN", "operator_timing": 0.000806791,
        "operator_cardinality": 1000, "operator_rows_scanned": 1000,
        "extra_info": {"Table": "shop_probe.main.users", "Type": "Index Scan",
                       "Filters": "country='GB'", "Estimated Cardinality": "1000"}}]}]}]}]}]}
```

The hash join's estimate (418) is 9.6 times below its actual rows (4,000), just under the app's misestimate factor of 10 (`mod.rs:142`).

In 1.5.6 each operator carries `operator_name`, `operator_type`, `operator_timing`, `cpu_time`, `operator_cardinality`, `cumulative_cardinality`, `operator_rows_scanned`, `cumulative_rows_scanned`, `result_set_size`, `system_peak_buffer_memory`, `system_peak_temp_dir_size`, `extra_info` and `children`. The `extra_info` keys across all captures:

| Operator | `extra_info` keys |
|---|---|
| `SEQ_SCAN` (type `TABLE_SCAN`) | `Table`, `Type` (`Sequential Scan` or `Index Scan`), `Projections`, `Filters`, `Dynamic Filters`, `Estimated Cardinality` |
| `PARQUET_SCAN` | `Function`, `Projections`, `Filters`, `Estimated Cardinality` |
| `HASH_JOIN`, `LEFT_DELIM_JOIN`, `RIGHT_DELIM_JOIN` | `Join Type`, `Conditions`, `Delim Index`, `Estimated Cardinality` |
| `FILTER` | `Expression`, `Estimated Cardinality` |
| `HASH_GROUP_BY` | `Groups`, `Aggregates`, `Estimated Cardinality` |
| `PROJECTION` | `Projections`, `Estimated Cardinality` |
| `TOP_N` | `Top`, `Order By`; no estimate |
| `ORDER_BY` | `Order By`; no estimate |
| `WINDOW` | `Projections`; no estimate |
| `DELIM_SCAN`, `COLUMN_DATA_SCAN` | `Delim Index`, `Estimated Cardinality` |
| `REC_CTE` (type `RECURSIVE_CTE`), `REC_CTE_SCAN` | `Estimated Cardinality` |
| `UNGROUPED_AGGREGATE`, `PERFECT_HASH_GROUP_BY`, `UNION`, `STREAMING_LIMIT`, `DUMMY_SCAN` | no estimate in these captures |

### What the times mean

The statement docs say EXPLAIN ANALYZE shows "the cumulative wall-clock time spent in each operator" ([profiling statement](https://duckdb.org/docs/current/sql/statements/profiling.html)); the metrics page calls `OPERATOR_TIMING` "Time spent in the operator" and `CPU_TIME` "the cumulative operator timings" ([metrics](https://duckdb.org/docs/current/dev/metrics)). The source and the captures settle what the numbers are:

- **`operator_timing` is the operator's own time, exclusive of its children, summed over every thread that ran it.** Each thread's `OperatorProfiler` adds the time from `StartOperator` to `EndOperator` around the operator's own work ([query_profiler.cpp#L402-L414](https://github.com/duckdb/duckdb/blob/v1.5.6/src/main/query_profiler.cpp#L402-L414)), and `QueryProfiler::Flush` adds each thread's total into the shared tree node ([#L530-L544](https://github.com/duckdb/duckdb/blob/v1.5.6/src/main/query_profiler.cpp#L530-L544)).
- **`cpu_time` is inclusive.** Enabling it enables `OPERATOR_TIMING` ([profiling_info.cpp#L61-L62](https://github.com/duckdb/duckdb/blob/v1.5.6/src/main/profiling_info.cpp#L61-L62)), and the metrics page defines a cumulative metric as the operator's value plus all of its children's. Verified by summing: in all 14 captured profiles, every operator's `cpu_time` equals the sum of `operator_timing` over its subtree to within 1e-9 s (`scripts/tree.py`). For example, TOP_N spent 0.208 ms itself and 2.911 ms with a hash join (0.444 ms) and two scans (1.365 and 0.892 ms) below it.
- **Thread time, not wall time.** The window query at the default 18 threads, four runs: `latency` 2.38 to 2.64 ms, `cpu_time` 14.9 to 18.3 ms. With `SET threads = 1`: `latency` 2.67 ms, `cpu_time` 2.25 ms **[ran]**. The guide says as much: with parallel threads "the total processing time of the query may be lower than the sum of all the times spent on the individual operators" ([EXPLAIN ANALYZE guide](https://duckdb.org/docs/current/guides/meta/explain_analyze.html)).
- `latency` at the root is the whole statement's wall time, including planning; `cpu_time` leaves out parsing and planning.

### Mapping onto the plan model

`crates/core/src/plan/mod.rs` holds one tree per plan, where `Actual.time` is **inclusive** and per loop, and inclusive time is divided by `processes` (`mod.rs:95-111`, `:262-268`). A DuckDB parser maps as follows:

| Plan model | From DuckDB | Notes |
|---|---|---|
| Recognizing the result | Two columns named `explain_key` and `explain_value`, with key `physical_plan` and a value starting with `[`, or key `analyzed_plan` and a value starting with `{` | With `explain_output = 'all'` there are three rows; read the `physical_plan` one. |
| Root node | The statement: a node named `Query` holding the root metrics | Drop `EXPLAIN_ANALYZE` (`operator_cardinality` 0) and attach its child to the root, as SQLite's root is always the statement (`docs/query-plans.md:88`). An estimate-only plan with several roots also gets a `Query` root, as in PostgreSQL's multi-statement case (`postgres_json.rs:52-63`). |
| `operation` | `operator_name` (analyze) or `name` (estimate): `SEQ_SCAN`, `HASH_JOIN`, … | `operator_type` (`TABLE_SCAN`) is less specific; keep it as an attribute. |
| `object` | `extra_info.Table` (`shop.main.orders`), or `Function` for table functions | |
| `index` | Not reported | `Type: Index Scan` says an index was used, not which. |
| `access_type` | `extra_info.Type` | Feeds `Access` below. |
| `condition` | `Conditions` (joins), `Filters` (scans), `Expression` (filters) | `Dynamic Filters` stays an attribute. |
| `estimate.rows` | `extra_info["Estimated Cardinality"]`, a string | `cost` is `None`: DuckDB reports no cost, so cost bars stay empty. A `"0"` right above an operator without an estimate is a placeholder (`unestimated`). |
| `actual.rows` | `operator_cardinality` | `None` for `EXPLAIN_ANALYZE`, `DELIM_SCAN`, `LEFT_DELIM_JOIN` and `RIGHT_DELIM_JOIN`, which report 0 while rows flow through their children. A 0 against an estimate of 27 would read as a misestimate. |
| `actual.loops` | Always 1 | DuckDB counts no loops, not even `REC_CTE` iterations. |
| `actual.time.last_row_ms` | `cpu_time × 1000`, scaled (below) | Already inclusive. |
| `actual.time.first_row_ms` | None | No equivalent. |
| `processes` | 1 | Per-operator thread counts are not reported. |
| `attributes` | every `extra_info` key, plus `operator_type`, `operator_rows_scanned`, `result_set_size`, `system_peak_buffer_memory` | Rows scanned against rows returned is a useful selectivity hint for the detail panel. |
| Plan-wide attributes and facts | root `latency`, `cpu_time`, `rows_returned`, `total_bytes_read`, `total_memory_allocated`, `blocked_thread_time`; in detailed mode `planner`, `physical_planner`, `all_optimizers` | In `facts.rs`, `latency` becomes Execution and `planner` plus `physical_planner` becomes Planning. |

**Time scaling.** Summed thread time makes the tree's bars and the flame chart's widths disagree with the statement's wall time: the window query's root would claim 17.7 ms for a 2.6 ms statement. Two honest options:

1. Scale every inclusive time by `latency / root cpu_time`, so the root equals the measured statement time and each step keeps its share, and label the format "DuckDB profile (times are shares of thread time)". This is the SQLite precedent: SQLite's cycle counts are scaled to the time spent in `sqlite3_step` (`DECISIONS.md`, "Explain Analyze on SQLite").
2. Show raw thread time with a label, and let the root exceed the statement.

Option 1 is recommended; `docs/query-plans.md` would get a DuckDB section that says so.

**Access.** `SEQ_SCAN` with `Type: Sequential Scan` is `FullScan`; with `Type: Index Scan` it is `Range` or `Lookup`, which the plan doesn't distinguish. `READ_CSV`, `PARQUET_SCAN` and other table functions are a `FullScan` of a file. `COLUMN_DATA_SCAN`, `DELIM_SCAN`, `CTE_SCAN` and `REC_CTE_SCAN` are `InternalResult`, and `DUMMY_SCAN` is `Constant`. `HASH_JOIN` lists the probe side first and the build side second ([profiling docs](https://duckdb.org/docs/current/dev/profiling.html)), so the graph view could label the second child "build".

**No equivalent** in DuckDB: per-loop numbers, first-row time, costs, index names and buffer hits. **DuckDB-only**: rows scanned, peak memory and bytes read per operator, dynamic filters, and optimizer timings in detailed mode.

**Determinism.** Estimated plans were byte-identical at 1 and 18 threads for the `portable` query, and analysed operator names, rows and rows scanned matched between two runs **[ran]**. Only times change.

**Hand-typed EXPLAIN.** `EXPLAIN q` without a format returns box-drawing text in `explain_value`. `raw_text` (`mod.rs:358-385`) would print it under an `explain_key  explain_value` header, so the DuckDB parser should pass only the value to the raw view. `asks_for_plan` must treat DuckDB's `DESCRIBE` and `SHOW` as tables, as it does MySQL's `DESCRIBE orders` (`request.rs:347-365`); `EXPLAIN (ANALYZE, FORMAT JSON) DESCRIBE orders` returns `{"result": "error"}` **[ran]**.

### The read-only guard

DuckDB executes the statement under EXPLAIN ANALYZE. Against copies of the shop seed **[ran]**:

| Run | Result |
|---|---|
| `EXPLAIN ANALYZE INSERT INTO t VALUES (2)` | The row is inserted and committed. |
| The same inside `BEGIN` … `ROLLBACK` | Undone. |
| `BEGIN TRANSACTION READ ONLY`, then INSERT or EXPLAIN ANALYZE INSERT | `TransactionContext Error: Cannot write to database "…" - transaction is launched in read-only mode` |
| `BEGIN TRANSACTION READ ONLY`, then `SELECT nextval('users_id_seq')` | Refused with the same error. Outside a read-only transaction, `ROLLBACK` does **not** undo `nextval`: 5001, then 5002 rolled back, then 5003. |
| `BEGIN TRANSACTION READ ONLY`, then INSERT into an attached database | Refused. |
| `BEGIN TRANSACTION READ ONLY`, then INSERT into a `TEMP` table | Allowed, and undone by the ROLLBACK. |
| `BEGIN TRANSACTION READ ONLY`, then `COPY (SELECT 1) TO 'f.csv'` or `EXPLAIN ANALYZE COPY …` | **Allowed: the file was written.** |
| `BEGIN TRANSACTION READ ONLY` while a transaction is open | `cannot start a transaction within a transaction` |

So the recipe is PostgreSQL's: setup `BEGIN TRANSACTION READ ONLY`, plan, reset `ROLLBACK`, with `needs_idle_session = true`. The open-transaction check comes free: the setup statement fails with that error, which the driver turns into the existing "commit or roll back first" message. The crate can't answer the question otherwise, because its `is_autocommit()` returns `true` even inside `BEGIN`.

The statement-shape check in `request.rs` stays the first guard, because the read-only transaction doesn't stop `COPY … TO`. It needs DuckDB rules:

- Allow `FROM …` (FROM-first SELECT) and `SUMMARIZE` as queries. `EXPLAIN (ANALYZE, FORMAT JSON) FROM orders LIMIT 1` and `… SUMMARIZE orders` both return profiles **[ran]**.
- Refuse `PIVOT` and `UNPIVOT` for Analyze: the `PIVOT` capture returned no plan cell **[ran]**.
- Refuse `COPY`, `EXPORT`, `ATTACH`, `INSTALL`, `LOAD`, `CALL`, `PRAGMA` and `SET`, like any non-query.

The second guard, SQLite's `sqlite3_stmt_readonly`, has no DuckDB equivalent: `duckdb_prepared_statement_type` reports SELECT for PRAGMA, SHOW and DESCRIBE, and nothing reports writes. It still catches a COPY, INSERT, UPDATE or DELETE the tokenizer misread, so the driver should require `DUCKDB_STATEMENT_TYPE_SELECT` (1) on the inner statement before running Explain Analyze.

The one-statement guard (`driver/mod.rs:116-117`) can't use `prepare` (section 3). The driver counts statements with `ffi::duckdb_extract_statements` and refuses more than one before running anything.

### Lab goldens

The lab (`/Users/koen/code/tableplus-rs-explain-lab`) captures through the app's own drivers. `capture/src/main.rs` lists targets (`:18-43`), queries (`:68-197`) and variants (`:200-245`), and writes `goldens/<target>/<query>.<variant>.txt`; `capture/tests/goldens.rs` parses every golden with the app's parsers. SQLite runs as `sqlite-bundled`, loaded from `northwind/sqlite.sql` into a temp file (`main.rs:309-321`). DuckDB slots in the same way:

1. Add `Target { id: "duckdb-bundled", engine: DuckDb, port: None }`, loaded from a new `northwind/duckdb.sql`. The capture runs `tableplus_core::driver::connect`, so this needs the app's DuckDB driver first.
2. Generate `northwind/duckdb.sql` in `convert.py`. Neither existing script loads as is **[ran]**: `northwind/postgres.sql` loads its data (830 orders, 2,155 order details) but its 13 `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` lines fail, and `northwind/sqlite.sql` fails on the self-referencing `employees` insert. The DuckDB script needs inline foreign keys and the employees inserted managers first.
3. Variants: `plain` (`EXPLAIN`), `json` (`EXPLAIN (FORMAT JSON)`), `all` (setup `SET explain_output = 'all'`), `analyze` (`EXPLAIN ANALYZE`) and `analyze-json` (`EXPLAIN (ANALYZE, FORMAT JSON)`), plus `html`, `graphviz`, `mermaid` and `yaml` for the matrix. The `dml` query gets only the variants that don't execute, as today.
4. `Feature::RecursiveCteAndWindows` is always available for DuckDB. The `portable` and `modern` queries ran unchanged **[ran]**; their outputs are in `/tmp/duckdb-research/lab/`.
5. One binary can link only one libduckdb, so the lab covers only the bundled version. That matches the app, which only ever runs the bundled version.
6. "Timings and determinism" in the lab README gains a DuckDB paragraph: analyze goldens change in their times only, and thread time depends on the machine's core count, so the corpus test must never assert times.

## 6. Shop data in DuckDB

### How the shop data is built today

- `docker/seed/sqlite.sql` (123 lines) seeds 5,000 users, 300 products, 20,000 orders, 40,001 order items, 100 audit rows, 3 `type_zoo` rows and two views.
- `crates/core/examples/make_sqlite.rs` writes `testdata/shop.sqlite` from it. The file is **not** in git (`.gitignore`: `/testdata/*.sqlite`); `scripts/demo-home.sh:9-12` builds it when it's missing.
- Tests build their own copy in a temp directory through the app's driver: `sqlite_fixture` splits the seed with `sql_script_statements` and runs it with `execute_in_transaction` (`crates/core/tests/common/mod.rs:73-82`).
- `scripts/screenshots.sh:31-33` copies `testdata/shop.sqlite` into a throwaway home, and `demo-home.sh` saves a connection to it.

### What I ran

1. **ATTACH the SQLite file** with `INSTALL sqlite; LOAD sqlite; ATTACH 'shop.sqlite' AS src (TYPE sqlite, READ_ONLY)`:
   - Every table and both views are readable. Columns become `BIGINT`, `DOUBLE`, `VARCHAR`, `TIMESTAMP`, `DATE` or `BLOB`: `BOOLEAN` becomes `BIGINT` and `NUMERIC` becomes `DOUBLE`. `sqlite_sequence` shows up as a table of `BLOB`s.
   - The `order_summary` view comes back with **every column as VARCHAR**, because a SQLite view has no declared types.
   - `CREATE TABLE … AS SELECT * FROM src.users` keeps no constraints at all.
   - `COPY FROM DATABASE src TO shop_copy` keeps primary keys, NOT NULL, defaults and indexes, and drops UNIQUE, CHECK, foreign keys and both views.
   - `INSTALL sqlite` downloads the 27 MB `sqlite_scanner` extension, so this path needs the network.
2. **Run `docker/seed/sqlite.sql` directly.** It fails on:
   - `PRAGMA foreign_keys = ON` (syntax error),
   - `INTEGER PRIMARY KEY AUTOINCREMENT` (syntax error at `AUTOINCREMENT`),
   - `ON DELETE SET NULL` and `ON DELETE CASCADE` (`FOREIGN KEY constraints cannot use CASCADE, SET NULL or SET DEFAULT`),
   - `datetime('2023-01-01 00:00:00', '+N minutes')` (no such function),
   - `type_zoo`'s `INTEGER PRIMARY KEY`, which SQLite fills in from the rowid (`NOT NULL constraint failed: type_zoo.id`),
   - and every statement that depends on those tables.
3. **A DuckDB seed**, `/tmp/duckdb-research/duckdb.sql`, written to mirror the SQLite seed. It loads in 0.12 s into a 9.0 MiB file, and also loads inside one `BEGIN TRANSACTION` … `COMMIT`, which is how a test fixture would run it. Its changes:
   - `CREATE SEQUENCE users_id_seq` and `id BIGINT PRIMARY KEY DEFAULT nextval('users_id_seq')` instead of `AUTOINCREMENT`.
   - Foreign keys without `ON DELETE` actions.
   - `range(1, 5001)` instead of recursive CTEs, `TIMESTAMP '…' + to_minutes(n * 47)` instead of `datetime()`, and `lpad()` instead of `substr('00000' || n, -5)`.
   - `n // 10` instead of `n / 10`, because `/` is float division in DuckDB.
   - SQLite's `REAL` becomes `DOUBLE`; `BOOLEAN` and `DECIMAL(12,2)` are real types; `orders.status` is an `ENUM`.
   - The categories are inserted in two statements, parents first. In one multi-row insert, a child row that references a parent from the same statement fails with `Violates foreign key constraint because key "id: 1" does not exist in the referenced table`.
   - `order_summary` lists every non-aggregated column in `GROUP BY`. DuckDB rejects SQLite's `GROUP BY o.id` with `column "email" must appear in the GROUP BY clause or must be part of an aggregate function`.
   - `type_zoo` holds DuckDB's types (HUGEINT through JSON, section 3) instead of SQLite's six.
   - A `COMMENT ON TABLE` and a `CREATE MACRO`, so the structure view and the Functions folder have something to show.

   Checked against `shop.sqlite` attached read-only: `users`, `orders` and `order_items` match row for row (`EXCEPT` returns 0 rows), and the order total sum (49,923,728.75), the balance sum (2,499,975.00), user 123's name and the latest order time match.

Neither seed has triggers.

### Recommended seed approach

Keep DuckDB exactly parallel to SQLite:

- Check in `docker/seed/duckdb.sql`, adapted from the draft.
- `crates/core/examples/make_duckdb.rs` writes `testdata/shop.duckdb`; add `/testdata/*.duckdb` and `/testdata/*.duckdb.wal` to `.gitignore`. Don't check the file in: it is 9 MiB, and a file written by a newer DuckDB can be unreadable by an older bundled one.
- `duckdb_fixture(dir)` in `tests/common/mod.rs` creates the database through the app's driver from the seed text, as `sqlite_fixture` does. That also tests `execute_in_transaction` with DDL, sequences, enums and macros.
- `crates/core/tests/duckdb.rs` mirrors `tests/sqlite.rs`: introspection, `type_zoo` rendering, editing with and without a primary key, streaming, cancel, and Explain and Explain Analyze through `exercise_explain` and `exercise_read_only_explain`.
- `plan_fixtures.rs` gets one captured DuckDB estimate and one profile, so parser tests run without building a database.

**Screenshots.** `demo-home.sh` gains a `Shop (DuckDB)` connection and builds `testdata/shop.duckdb` when it's missing; `screenshots.sh` copies it to `~/Databases/shop.duckdb`. The welcome list is sorted by name, so `Shop (DuckDB)` lands between `Prod replica (via SSH)` and `Shop (MySQL 8)`. `ROW_MYSQL`, `ROW_POSTGRES` and `ROW_SQLITE` (`screenshots.sh:100-104`) each move down one 58 px row, and `ROW_DUCKDB=353` is added. A new `scene_duckdb` could show the `type_zoo` grid with nested values and Explain Analyze of the existing screenshot query, which runs on DuckDB unchanged.

## 7. Proposed build order and open questions

### Phases

1. **Connect and browse.** `DriverKind::DuckDb`, `Dialect::DuckDb`, `Flavor::DuckDb` and their match arms. The driver with the per-path instance registry, `try_clone` per session, statement counting through `ffi`, streaming reads, `Count` and `Success` results, and value formatting with the cast fallback for TIMETZ, TIME_NS, BIT, BIGNUM and VARIANT. Catalog for the current database, cancel, the connection dialog (file, read-only, `:memory:`), the seed, `make_duckdb`, the fixture and `tests/duckdb.rs`. About 1,200 lines.
2. **Edit.** DuckDB arms in `dialect.rs` (the `from_hex` literal), `sqlgen.rs` (`ESCAPE '\'`) and `alter.rs` (Postgres-like, with the index and foreign-key limits surfaced or a rebuild), structure-view gating, and `exercise_editing` against DuckDB. About 400 lines.
3. **Explain.** Request rules and the read-only recipe, the JSON parser with time scaling, `unestimated` and access rules, sections in `docs/query-plans.md` and `DECISIONS.md`, `plan_fixtures`, and the lab target. About 800 lines.
4. **DuckDB extras.** The ATTACH list and two-level sidebar, config options, Parquet export and `read_csv` import, progress, an extensions policy, and MotherDuck. Each is optional and separate.

### Open questions for Koen

1. **Network extensions.** The bundled library downloads extensions on first use: ICU for time zones, the SQLite, Postgres and MySQL scanners, MotherDuck. Allow that, or turn auto-install off and show an "Install extension" action? Recommendation: auto-load on, auto-install off, and an explicit install action, so opening a file never reaches the network silently.
2. **Binary size.** DuckDB adds about 31 to 34 MB to a 20 MB app, and 1 to 1.5 minutes to a cold build. Ship it in the default build, or behind a cargo feature? Recommendation: a `duckdb` feature that is on by default.
3. **Attached databases in the sidebar.** Current database only, `db.schema` labels, or a real database level (section 4)? Recommendation: `db.schema` labels.
4. **Explain Analyze times.** Scale thread time to the statement's latency, as SQLite's cycle shares are scaled, or show raw thread time? Recommendation: scale.
5. **MotherDuck.** In scope for the first release, or later? It needs a token field, the extension download, and plans from a partly remote engine that this research didn't capture.
