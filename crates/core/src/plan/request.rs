//! Which statements ask a server for a plan, by engine and version. The rules come from the
//! EXPLAIN capability matrix captured from 18 servers (MySQL 5.6 to 9.7, MariaDB 10.6 to 11.8,
//! PostgreSQL 12 to 18 and SQLite 3.53).

use crate::dialect::Dialect;
use crate::server::{Flavor, ServerInfo, Version};
use crate::sqltext::{Token, TokenKind, tokenize, trim_statement};

use super::PlanFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExplainMode {
    /// The planner's estimates; the statement does not run.
    Estimate,
    /// Runs the statement and measures each step.
    Analyze,
}

/// Run `setup`, then `explain`, then `reset` (even when `explain` fails), on one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainStatements {
    pub setup: Vec<String>,
    pub explain: Explain,
    pub reset: Vec<String>,
    /// The format the server answers in.
    pub format: PlanFormat,
}

/// How the plan is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Explain {
    /// An EXPLAIN statement, whose rows are the plan.
    Statement(String),
    /// The user's statement itself, run by
    /// [`Connection::measure`](crate::driver::Connection::measure), which returns the plan with
    /// what the database measured. SQLite works this way.
    Measure(String),
}

impl Explain {
    /// The statement the driver runs.
    pub fn sql(&self) -> &str {
        match self {
            Explain::Statement(sql) | Explain::Measure(sql) => sql,
        }
    }
}

/// Why a plan can't be requested, phrased for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Unsupported(pub String);

enum Recipe {
    Wrap {
        prefix: &'static str,
        format: PlanFormat,
        setup: &'static [&'static str],
        reset: &'static [&'static str],
    },
    /// The driver runs the statement and measures it ([`Explain::Measure`]).
    Measure(PlanFormat),
    Unsupported(&'static str),
}

/// A recipe for servers of `flavor` from version `since` on. The newest rule a server's version
/// reaches wins.
struct Rule {
    flavor: Flavor,
    mode: ExplainMode,
    since: Version,
    recipe: Recipe,
}

const fn wrap(flavor: Flavor, mode: ExplainMode, since: Version, prefix: &'static str, format: PlanFormat) -> Rule {
    Rule { flavor, mode, since, recipe: Recipe::Wrap { prefix, format, setup: &[], reset: &[] } }
}

const fn unsupported(flavor: Flavor, mode: ExplainMode, since: Version, reason: &'static str) -> Rule {
    Rule { flavor, mode, since, recipe: Recipe::Unsupported(reason) }
}

/// MySQL's JSON format version 2 for one statement, with the session's own setting put back
/// afterwards. Version 2 lists the same steps as TREE text but keeps tables, columns, indexes,
/// lookup and filter conditions and the join algorithm as separate fields, at full precision.
const fn json_v2(mode: ExplainMode, prefix: &'static str) -> Rule {
    const SETUP: &[&str] = &["SET @tableplus_explain_json_format = @@SESSION.explain_json_format_version, \
          explain_json_format_version = 2"];
    const RESET: &[&str] = &["SET explain_json_format_version = @tableplus_explain_json_format"];
    Rule {
        flavor: Flavor::MySql,
        mode,
        since: MYSQL_JSON_V2,
        recipe: Recipe::Wrap { prefix, format: PlanFormat::MySqlJsonV2, setup: SETUP, reset: RESET },
    }
}

/// Statements that make the session's next transactions read-only for an Explain Analyze, and
/// put back the session's own setting, which may already be read-only. The statement check below
/// only sees the statement's shape; a query can still write through a function such as
/// `nextval`, which a read-only transaction refuses.
const fn read_only(flavor: Flavor) -> (&'static [&'static str], &'static [&'static str]) {
    match flavor {
        Flavor::Postgres => (
            &[
                "SELECT set_config('tableplus.explain_read_only', current_setting('default_transaction_read_only'), false), \
               set_config('default_transaction_read_only', 'on', false)",
            ],
            &[
                "SELECT set_config('default_transaction_read_only', current_setting('tableplus.explain_read_only'), false)",
            ],
        ),
        Flavor::MySql => (
            &[
                "SET @tableplus_explain_read_only = @@SESSION.transaction_read_only, @@SESSION.transaction_read_only = 1",
            ],
            &["SET @@SESSION.transaction_read_only = @tableplus_explain_read_only"],
        ),
        // MariaDB before 11.1 only knows the older name.
        Flavor::MariaDb => (
            &["SET @tableplus_explain_read_only = @@SESSION.tx_read_only, @@SESSION.tx_read_only = 1"],
            &["SET @@SESSION.tx_read_only = @tableplus_explain_read_only"],
        ),
        Flavor::Sqlite => (&[], &[]),
    }
}

const ANY: Version = Version::new(0, 0, 0);
const MYSQL_TREE: Version = Version::new(8, 0, 18);
const MYSQL_JSON_V2: Version = Version::new(8, 4, 0);
/// `sqlite3_stmt_scanstatus_v2`, with every plan step and its ids and cycles.
const SQLITE_SCAN_STATUS: Version = Version::new(3, 42, 0);

use ExplainMode::{Analyze, Estimate};
use Flavor::{MariaDb, MySql, Postgres, Sqlite};

const RULES: &[Rule] = &[
    wrap(MySql, Estimate, ANY, "EXPLAIN FORMAT=JSON ", PlanFormat::MySqlJson),
    wrap(MySql, Estimate, MYSQL_TREE, "EXPLAIN FORMAT=TREE ", PlanFormat::MySqlTree),
    json_v2(Estimate, "EXPLAIN FORMAT=JSON "),
    unsupported(MySql, Analyze, ANY, "EXPLAIN ANALYZE needs MySQL 8.0.18 or later."),
    wrap(MySql, Analyze, MYSQL_TREE, "EXPLAIN ANALYZE ", PlanFormat::MySqlTree),
    json_v2(Analyze, "EXPLAIN ANALYZE FORMAT=JSON "),
    wrap(MariaDb, Estimate, ANY, "EXPLAIN FORMAT=JSON ", PlanFormat::MariaDbJson),
    wrap(MariaDb, Analyze, ANY, "ANALYZE FORMAT=JSON ", PlanFormat::MariaDbJson),
    wrap(Postgres, Estimate, ANY, "EXPLAIN (FORMAT JSON) ", PlanFormat::PostgresJson),
    wrap(Postgres, Analyze, ANY, "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ", PlanFormat::PostgresJson),
    wrap(Sqlite, Estimate, ANY, "EXPLAIN QUERY PLAN ", PlanFormat::SqliteQueryPlan),
    unsupported(Sqlite, Analyze, ANY, "Explain Analyze needs SQLite 3.42 or later."),
    Rule {
        flavor: Sqlite,
        mode: Analyze,
        since: SQLITE_SCAN_STATUS,
        recipe: Recipe::Measure(PlanFormat::SqliteScanStatus),
    },
];

/// The statements that explain `sql` on `server`.
pub fn request(server: &ServerInfo, sql: &str, mode: ExplainMode) -> Result<ExplainStatements, Unsupported> {
    let sql = trim_statement(sql);
    let shape = StatementShape::read(sql, server.flavor.dialect());
    if shape.verb == "EXPLAIN" || shape.verb == "DESCRIBE" || (shape.verb == "ANALYZE" && server.flavor == MariaDb) {
        return Err(Unsupported("The statement already asks for a plan. Run it to see the plan.".into()));
    }
    if mode == Analyze
        && let Some(write) = shape.writes()
    {
        return Err(Unsupported(format!("Explain Analyze runs the statement, and this {write} would change data.")));
    }
    let rule = RULES
        .iter()
        .filter(|r| r.flavor == server.flavor && r.mode == mode && r.since <= server.version)
        .max_by_key(|r| r.since)
        .expect("every flavor and mode has a rule for any version");
    match rule.recipe {
        Recipe::Wrap { prefix, format, setup, reset } => {
            let (ro_setup, ro_reset) = if mode == Analyze { read_only(server.flavor) } else { (&[][..], &[][..]) };
            Ok(ExplainStatements {
                setup: ro_setup.iter().chain(setup).map(|s| s.to_string()).collect(),
                explain: Explain::Statement(format!("{prefix}{sql}")),
                reset: reset.iter().chain(ro_reset).map(|s| s.to_string()).collect(),
                format,
            })
        }
        Recipe::Measure(_) if !server.scan_status => Err(Unsupported(
            "This SQLite library was built without SQLITE_ENABLE_STMT_SCANSTATUS, so it can't measure the statement."
                .into(),
        )),
        Recipe::Measure(format) => Ok(ExplainStatements {
            setup: Vec::new(),
            explain: Explain::Measure(sql.to_string()),
            reset: Vec::new(),
            format,
        }),
        Recipe::Unsupported(reason) => Err(Unsupported(reason.into())),
    }
}

/// The leading keywords of a statement: its main verb, the verbs of its WITH clauses, and
/// whether a SELECT writes `INTO` a table, file or variable.
struct StatementShape {
    /// The first keyword in capitals, or empty when the statement starts with something else.
    verb: String,
    cte_verbs: Vec<String>,
    select_into: bool,
}

impl StatementShape {
    fn read(sql: &str, dialect: Dialect) -> Self {
        let tokens: Vec<Token> = tokenize(sql, dialect).into_iter().filter(|t| !t.is_trivia()).collect();
        let word = |i: usize| tokens.get(i).map(|t| t.text(sql).to_ascii_uppercase()).unwrap_or_default();
        let punct = |i: usize, p: &str| tokens.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text(sql) == p);
        let skip_parens = |mut i: usize| {
            while punct(i, "(") {
                i += 1;
            }
            i
        };
        // Index just past the parenthesized group that opens at `i`.
        let close = |i: usize| {
            let mut depth = 0usize;
            for (j, t) in tokens.iter().enumerate().skip(i) {
                match (t.kind, t.text(sql)) {
                    (TokenKind::Punct, "(") => depth += 1,
                    (TokenKind::Punct, ")") => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
            }
            tokens.len()
        };

        let mut i = skip_parens(0);
        let mut cte_verbs = Vec::new();
        if word(i) == "WITH" {
            i += 1;
            if word(i) == "RECURSIVE" {
                i += 1;
            }
            // name [(columns)] AS [NOT] [MATERIALIZED] (body) [, …]
            while i < tokens.len() {
                i += 1;
                if punct(i, "(") {
                    i = close(i);
                }
                while matches!(word(i).as_str(), "AS" | "NOT" | "MATERIALIZED") {
                    i += 1;
                }
                if !punct(i, "(") {
                    break;
                }
                cte_verbs.push(word(skip_parens(i)));
                i = close(i);
                if !punct(i, ",") {
                    break;
                }
                i += 1;
            }
            i = skip_parens(i);
        }
        let keyword = tokens.get(i).is_some_and(|t| t.kind == TokenKind::Keyword);
        let verb = match word(i) {
            _ if !keyword => String::new(),
            w if w == "DESC" => "DESCRIBE".to_string(),
            w => w,
        };
        let mut depth = 0usize;
        let select_into = verb == "SELECT"
            && tokens[i..].iter().any(|t| match (t.kind, t.text(sql)) {
                (TokenKind::Punct, "(") => {
                    depth += 1;
                    false
                }
                (TokenKind::Punct, ")") => {
                    depth = depth.saturating_sub(1);
                    false
                }
                (TokenKind::Keyword, w) => depth == 0 && w.eq_ignore_ascii_case("INTO"),
                _ => false,
            });
        Self { verb, cte_verbs, select_into }
    }

    /// The keyword that makes running the statement change data, if it would.
    fn writes(&self) -> Option<String> {
        const QUERY: &[&str] = &["SELECT", "TABLE", "VALUES", "WITH"];
        if let Some(cte) = self.cte_verbs.iter().find(|v| !QUERY.contains(&v.as_str())) {
            return Some(cte.clone());
        }
        if self.select_into {
            return Some("SELECT … INTO".into());
        }
        match self.verb.as_str() {
            "SELECT" | "TABLE" | "VALUES" => None,
            "" => Some("statement".into()),
            verb => Some(verb.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL_57: ServerInfo = ServerInfo::new(MySql, Version::new(5, 7, 44));
    const MYSQL_8018: ServerInfo = ServerInfo::new(MySql, Version::new(8, 0, 18));
    const MYSQL_83: ServerInfo = ServerInfo::new(MySql, Version::new(8, 3, 0));
    const MYSQL_84: ServerInfo = ServerInfo::new(MySql, Version::new(8, 4, 11));
    const MARIADB: ServerInfo = ServerInfo::new(MariaDb, Version::new(11, 4, 13));
    const POSTGRES: ServerInfo = ServerInfo::new(Postgres, Version::new(17, 6, 0));
    const SQLITE: ServerInfo = ServerInfo::new(Sqlite, Version::new(3, 53, 2)).with_scan_status(true);

    fn explain(server: ServerInfo, sql: &str, mode: ExplainMode) -> Result<String, String> {
        request(&server, sql, mode).map(|r| r.explain.sql().to_string()).map_err(|e| e.0)
    }

    #[test]
    fn statements_follow_the_matrix() {
        let q = "select * from orders";
        for (server, mode, expected, format) in [
            (MYSQL_57, Estimate, "EXPLAIN FORMAT=JSON select * from orders", PlanFormat::MySqlJson),
            (MYSQL_8018, Estimate, "EXPLAIN FORMAT=TREE select * from orders", PlanFormat::MySqlTree),
            (MYSQL_83, Estimate, "EXPLAIN FORMAT=TREE select * from orders", PlanFormat::MySqlTree),
            (MYSQL_8018, Analyze, "EXPLAIN ANALYZE select * from orders", PlanFormat::MySqlTree),
            (MYSQL_83, Analyze, "EXPLAIN ANALYZE select * from orders", PlanFormat::MySqlTree),
            (MARIADB, Estimate, "EXPLAIN FORMAT=JSON select * from orders", PlanFormat::MariaDbJson),
            (MARIADB, Analyze, "ANALYZE FORMAT=JSON select * from orders", PlanFormat::MariaDbJson),
            (POSTGRES, Estimate, "EXPLAIN (FORMAT JSON) select * from orders", PlanFormat::PostgresJson),
            (
                POSTGRES,
                Analyze,
                "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) select * from orders",
                PlanFormat::PostgresJson,
            ),
            (SQLITE, Estimate, "EXPLAIN QUERY PLAN select * from orders", PlanFormat::SqliteQueryPlan),
        ] {
            let r = request(&server, q, mode).unwrap_or_else(|e| panic!("{server} {mode:?}: {e}"));
            assert_eq!(r.explain, Explain::Statement(expected.into()), "{server} {mode:?}");
            assert_eq!(r.format, format, "{server} {mode:?}");
            assert_eq!(
                r.setup,
                read_only(server.flavor).0.iter().filter(|_| mode == Analyze).copied().collect::<Vec<_>>()
            );
            assert_eq!(
                r.reset,
                read_only(server.flavor).1.iter().filter(|_| mode == Analyze).copied().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn mysql_84_asks_for_json_version_2_and_restores_the_setting() {
        for (mode, expected) in [
            (Estimate, "EXPLAIN FORMAT=JSON select * from orders"),
            (Analyze, "EXPLAIN ANALYZE FORMAT=JSON select * from orders"),
        ] {
            for server in [MYSQL_84, ServerInfo::new(MySql, Version::new(9, 7, 2))] {
                let r = request(&server, "select * from orders", mode).unwrap();
                assert_eq!((r.explain.sql(), r.format), (expected, PlanFormat::MySqlJsonV2), "{server} {mode:?}");
                let json = r.setup.last().unwrap();
                assert!(json.contains("explain_json_format_version = 2"), "{json}");
                assert!(json.contains("@tableplus_explain_json_format = @@SESSION.explain_json_format_version"));
                assert_eq!(r.reset[0], "SET explain_json_format_version = @tableplus_explain_json_format");
                assert_eq!(r.setup.len(), if mode == Analyze { 2 } else { 1 }, "{server} {mode:?}");
            }
        }
    }

    #[test]
    fn analyze_is_refused_where_the_server_lacks_it() {
        let q = "select 1";
        assert!(explain(MYSQL_57, q, Analyze).unwrap_err().contains("8.0.18"));
        assert!(explain(ServerInfo::new(MySql, Version::new(8, 0, 17)), q, Analyze).is_err());
        assert!(explain(ServerInfo::new(Sqlite, Version::new(3, 41, 2)).with_scan_status(true), q, Analyze).is_err());
        let without = explain(ServerInfo::new(Sqlite, Version::new(3, 53, 2)), q, Analyze).unwrap_err();
        assert!(without.contains("SQLITE_ENABLE_STMT_SCANSTATUS"), "{without}");
    }

    #[test]
    fn sqlite_measures_the_statement_itself() {
        let r = request(&SQLITE, "select * from orders;", Analyze).unwrap();
        assert_eq!(r.explain, Explain::Measure("select * from orders".into()));
        assert_eq!(r.format, PlanFormat::SqliteScanStatus);
        assert!(r.setup.is_empty() && r.reset.is_empty());
    }

    #[test]
    fn every_engine_has_a_rule_for_every_version() {
        for flavor in [MySql, MariaDb, Postgres, Sqlite] {
            for mode in [Estimate, Analyze] {
                assert!(
                    RULES.iter().any(|r| r.flavor == flavor && r.mode == mode && r.since == ANY),
                    "{flavor:?} {mode:?} needs a rule from version 0"
                );
            }
        }
    }

    #[test]
    fn strips_the_trailing_semicolon() {
        assert_eq!(explain(MYSQL_84, "  select 1 ;\n", Estimate).unwrap(), "EXPLAIN FORMAT=JSON select 1");
    }

    #[test]
    fn analyze_runs_in_a_read_only_session() {
        for server in [POSTGRES, MYSQL_8018, MYSQL_84, MARIADB] {
            let r = request(&server, "select nextval('s')", Analyze).unwrap();
            let (setup, reset) = (&r.setup[0], r.reset.last().unwrap());
            assert!(
                setup.contains("explain_read_only") && setup.contains("read_only"),
                "{server} saves the setting: {setup}"
            );
            assert!(reset.contains("explain_read_only"), "{server} restores the saved setting: {reset}");
            assert!(request(&server, "select 1", Estimate).unwrap().setup.iter().all(|s| !s.contains("read_only")));
        }
    }

    #[test]
    fn analyze_never_runs_writes() {
        for (server, sql, verb) in [
            (MYSQL_84, "update products set unit_price = unit_price * 1.05", "UPDATE"),
            (MYSQL_84, "DELETE FROM orders WHERE id = 1", "DELETE"),
            (MYSQL_84, "insert into t select * from u", "INSERT"),
            (MYSQL_84, "replace into t values (1)", "REPLACE"),
            (MYSQL_84, "with x as (select 1) update t join x set a = 1", "UPDATE"),
            (POSTGRES, "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", "DELETE"),
            (
                POSTGRES,
                "with recursive a(n) as (select 1), b as materialized (insert into t values (1) returning *) select 1",
                "INSERT",
            ),
            (POSTGRES, "select * into new_table from t", "SELECT … INTO"),
            (MYSQL_84, "select * from orders into outfile '/tmp/orders.csv'", "SELECT … INTO"),
            (MYSQL_84, "select count(*) into @n from orders", "SELECT … INTO"),
            (POSTGRES, "create table t as select 1", "CREATE"),
            (MARIADB, "call refresh_stats()", "CALL"),
            (MYSQL_84, "s.country, count(distinct od.order_id)", "statement"),
            (SQLITE, "UPDATE products SET unit_price = unit_price * 1.05", "UPDATE"),
            (SQLITE, "insert or replace into t values (1)", "INSERT"),
            (SQLITE, "replace into t values (1)", "REPLACE"),
            (SQLITE, "with x as (select 1) delete from t where a in (select * from x)", "DELETE"),
            (SQLITE, "pragma user_version = 3", "statement"),
        ] {
            let err = explain(server, sql, Analyze).unwrap_err();
            assert!(err.contains(verb), "{sql}: {err}");
            assert!(explain(server, sql, Estimate).is_ok(), "{sql} still gets an estimate");
        }
    }

    #[test]
    fn analyze_runs_queries() {
        for sql in [
            "select * from t for update",
            "SELECT replace(name, 'a', 'b') FROM t",
            "(select 1) union (select 2)",
            "with recursive chain as (select 1 union all select n + 1 from chain) select * from chain",
            "with a as (select 1), b (x) as not materialized (select 2) select * from a, b",
            "select (select max(x) from (select 1 as x) s) as m",
            "select a from t where b in (select b into_col from u)",
            "values (1), (2)",
            "table orders",
        ] {
            assert!(explain(POSTGRES, sql, Analyze).is_ok(), "{sql}");
        }
    }

    #[test]
    fn existing_explains_run_as_they_are() {
        for (server, sql) in [
            (MYSQL_84, "EXPLAIN ANALYZE SELECT 1"),
            (MYSQL_84, "explain format=json select 1"),
            (MYSQL_84, "describe orders"),
            (MYSQL_84, "desc orders"),
            (POSTGRES, "explain (analyze) select 1"),
            (MARIADB, "ANALYZE FORMAT=JSON SELECT 1"),
            (SQLITE, "EXPLAIN QUERY PLAN SELECT 1"),
        ] {
            assert!(explain(server, sql, Estimate).unwrap_err().contains("already"), "{sql}");
        }
        assert!(explain(MYSQL_84, "analyze table orders", Estimate).is_ok());
    }
}
