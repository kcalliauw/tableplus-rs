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
    pub explain: String,
    pub reset: Vec<String>,
    /// The format the server answers in.
    pub format: PlanFormat,
}

/// Why a plan can't be requested, phrased for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Unsupported(pub String);

enum Recipe {
    Wrap { prefix: &'static str, format: PlanFormat, setup: &'static [&'static str], reset: &'static [&'static str] },
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

const ANY: Version = Version::new(0, 0, 0);
const MYSQL_TREE: Version = Version::new(8, 0, 18);

use ExplainMode::{Analyze, Estimate};
use Flavor::{MariaDb, MySql, Postgres, Sqlite};

const RULES: &[Rule] = &[
    wrap(MySql, Estimate, ANY, "EXPLAIN FORMAT=JSON ", PlanFormat::MySqlJson),
    wrap(MySql, Estimate, MYSQL_TREE, "EXPLAIN FORMAT=TREE ", PlanFormat::MySqlTree),
    unsupported(MySql, Analyze, ANY, "EXPLAIN ANALYZE needs MySQL 8.0.18 or later."),
    wrap(MySql, Analyze, MYSQL_TREE, "EXPLAIN ANALYZE ", PlanFormat::MySqlTree),
    wrap(MariaDb, Estimate, ANY, "EXPLAIN FORMAT=JSON ", PlanFormat::MariaDbJson),
    wrap(MariaDb, Analyze, ANY, "ANALYZE FORMAT=JSON ", PlanFormat::MariaDbJson),
    wrap(Postgres, Estimate, ANY, "EXPLAIN (FORMAT JSON) ", PlanFormat::PostgresJson),
    wrap(Postgres, Analyze, ANY, "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ", PlanFormat::PostgresJson),
    wrap(Sqlite, Estimate, ANY, "EXPLAIN QUERY PLAN ", PlanFormat::SqliteQueryPlan),
    unsupported(Sqlite, Analyze, ANY, "SQLite can't measure a statement while it runs."),
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
        Recipe::Wrap { prefix, format, setup, reset } => Ok(ExplainStatements {
            setup: setup.iter().map(|s| s.to_string()).collect(),
            explain: format!("{prefix}{sql}"),
            reset: reset.iter().map(|s| s.to_string()).collect(),
            format,
        }),
        Recipe::Unsupported(reason) => Err(Unsupported(reason.into())),
    }
}

/// The leading keywords of a statement: its main verb, the verbs of its WITH clauses, and
/// whether a SELECT writes `INTO` a table, file or variable.
struct StatementShape {
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
        let verb = if word(i) == "DESC" { "DESCRIBE".to_string() } else { word(i) };
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
    const MYSQL_84: ServerInfo = ServerInfo::new(MySql, Version::new(8, 4, 11));
    const MARIADB: ServerInfo = ServerInfo::new(MariaDb, Version::new(11, 4, 13));
    const POSTGRES: ServerInfo = ServerInfo::new(Postgres, Version::new(17, 6, 0));
    const SQLITE: ServerInfo = ServerInfo::new(Sqlite, Version::new(3, 53, 2));

    fn explain(server: ServerInfo, sql: &str, mode: ExplainMode) -> Result<String, String> {
        request(&server, sql, mode).map(|r| r.explain).map_err(|e| e.0)
    }

    #[test]
    fn statements_follow_the_matrix() {
        let q = "select * from orders";
        for (server, mode, expected, format) in [
            (MYSQL_57, Estimate, "EXPLAIN FORMAT=JSON select * from orders", PlanFormat::MySqlJson),
            (MYSQL_8018, Estimate, "EXPLAIN FORMAT=TREE select * from orders", PlanFormat::MySqlTree),
            (MYSQL_84, Estimate, "EXPLAIN FORMAT=TREE select * from orders", PlanFormat::MySqlTree),
            (MYSQL_8018, Analyze, "EXPLAIN ANALYZE select * from orders", PlanFormat::MySqlTree),
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
            assert_eq!(r.explain, expected, "{server} {mode:?}");
            assert_eq!(r.format, format, "{server} {mode:?}");
            assert!(r.setup.is_empty() && r.reset.is_empty());
        }
    }

    #[test]
    fn analyze_is_refused_where_the_server_lacks_it() {
        let q = "select 1";
        assert!(explain(MYSQL_57, q, Analyze).unwrap_err().contains("8.0.18"));
        assert!(explain(ServerInfo::new(MySql, Version::new(8, 0, 17)), q, Analyze).is_err());
        assert!(explain(SQLITE, q, Analyze).is_err());
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
        assert_eq!(explain(MYSQL_84, "  select 1 ;\n", Estimate).unwrap(), "EXPLAIN FORMAT=TREE select 1");
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
            (POSTGRES, "create table t as select 1", "CREATE"),
            (MARIADB, "call refresh_stats()", "CALL"),
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
