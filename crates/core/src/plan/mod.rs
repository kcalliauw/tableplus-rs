//! Query plans: one tree model for every engine's EXPLAIN output, the statements that ask a
//! server for a plan ([`request`]), and the parsers that read its output ([`parse`]).
//!
//! The model keeps what the engines report in common. A node has an operation, the object it
//! reads, a condition, the planner's estimate and the measured actuals, plus every raw attribute
//! for display. MySQL TREE text, MySQL and MariaDB JSON, PostgreSQL JSON, SQLite's
//! `EXPLAIN QUERY PLAN` rows and SQLite's measured scan status all fit it.

mod access;
mod mariadb_json;
mod mysql_json;
mod mysql_tree;
mod postgres_json;
mod request;
mod sqlite_eqp;
mod sqlite_scanstatus;

pub use access::Access;
pub use request::{ExplainMode, ExplainStatements, Unsupported, request};
pub(crate) use sqlite_scanstatus::{COLUMNS as SQLITE_SCAN_STATUS_COLUMNS, STATEMENT_ID as SQLITE_STATEMENT_ID};

use crate::model::ResultSet;
use crate::server::ServerInfo;

/// A node's position in its [`Plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(usize);

impl NodeId {
    pub const ROOT: NodeId = NodeId(0);

    /// Position in [`Plan::nodes`], for per-node state kept in a `Vec`.
    pub fn index(self) -> usize {
        self.0
    }
}

/// The output format a plan was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanFormat {
    /// MySQL 8.0.18+ `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE` text (the 9.x default).
    MySqlTree,
    /// MySQL `EXPLAIN FORMAT=JSON`, version 1.
    MySqlJson,
    /// MySQL `EXPLAIN [ANALYZE] FORMAT=JSON` with `explain_json_format_version = 2`, the 9.x
    /// default.
    MySqlJsonV2,
    /// MariaDB `EXPLAIN FORMAT=JSON` and `ANALYZE FORMAT=JSON`.
    MariaDbJson,
    /// PostgreSQL `EXPLAIN (FORMAT JSON)`, with or without ANALYZE.
    PostgresJson,
    /// SQLite `EXPLAIN QUERY PLAN` rows.
    SqliteQueryPlan,
    /// What SQLite measured per plan step while the statement ran (`sqlite3_stmt_scanstatus_v2`).
    /// Step times are shares of counted CPU cycles, not timed.
    SqliteScanStatus,
}

impl PlanFormat {
    pub fn label(self) -> &'static str {
        match self {
            PlanFormat::MySqlTree => "MySQL tree",
            PlanFormat::MySqlJson => "MySQL JSON",
            PlanFormat::MySqlJsonV2 => "MySQL JSON version 2",
            PlanFormat::MariaDbJson => "MariaDB JSON",
            PlanFormat::PostgresJson => "PostgreSQL JSON",
            PlanFormat::SqliteQueryPlan => "SQLite query plan",
            PlanFormat::SqliteScanStatus => "SQLite scan status (times from CPU cycles)",
        }
    }
}

/// One key and value exactly as the engine reported it, for the detail panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub key: String,
    pub value: String,
}

impl Attribute {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: value.into() }
    }
}

/// The planner's cost, in the engine's own units. Costs include the node's children.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    /// Cost before the first row, where the engine reports one (`cost=2.6..2.6`).
    pub startup: Option<f64>,
    pub total: f64,
}

/// What the planner expected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Rows per loop.
    pub rows: f64,
    pub cost: Option<Cost>,
}

/// Measured time per loop, including the node's children.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActualTime {
    /// Milliseconds to the first row; `None` when the engine reports only a duration (MariaDB).
    pub first_row_ms: Option<f64>,
    /// Milliseconds to the last row.
    pub last_row_ms: f64,
}

/// What happened when the statement ran.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Actual {
    /// Rows per loop, averaged over the loops, so it can be fractional. `None` for a step the
    /// engine times without counting its rows, such as a MariaDB query block.
    pub rows: Option<f64>,
    /// How many times the node ran; zero when it never executed.
    pub loops: u64,
    /// `None` when the engine counted rows without timing them.
    pub time: Option<ActualTime>,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// What the step does: `Nested loop inner join`, `Seq Scan`, `SCAN`…
    pub operation: String,
    /// How the step feeds its parent when it is not one of its inputs, such as PostgreSQL's
    /// `SubPlan 1` or `CTE chain`: a subquery the parent runs for an expression.
    pub role: Option<String>,
    /// The table, CTE or temporary result it reads.
    pub object: Option<String>,
    /// The index it uses.
    pub index: Option<String>,
    /// How the engine reaches the rows, in its own words: `ALL`, `ref` or `eq_ref` in MySQL's
    /// JSON version 1, `table`, `index` or `join` in version 2.
    pub access_type: Option<String>,
    /// Join, filter, sort or lookup condition, as the engine prints it.
    pub condition: Option<String>,
    pub estimate: Option<Estimate>,
    pub actual: Option<Actual>,
    /// Everything the engine reported for this node, in its order and wording.
    pub attributes: Vec<Attribute>,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    depth: usize,
}

/// Actual rows this many times above or below the estimate count as a misestimate.
pub const MISESTIMATE_FACTOR: f64 = 10.0;

impl PlanNode {
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            role: None,
            object: None,
            index: None,
            access_type: None,
            condition: None,
            estimate: None,
            actual: None,
            attributes: Vec::new(),
            parent: None,
            children: Vec::new(),
            depth: 0,
        }
    }

    /// Actual rows divided by estimated rows, per loop. Both count as at least one row, so a
    /// guess of 0.3 rows that turns out to be 0.03 is not a misestimate. `None` without both
    /// numbers, when the node never ran, or for an estimate of exactly zero, which MySQL prints
    /// for steps it doesn't estimate (temporary tables, and the sort and limit above them).
    pub fn row_estimate_ratio(&self) -> Option<f64> {
        let estimated = self.estimate?.rows;
        if estimated == 0.0 {
            return None;
        }
        let actual = self.actual.filter(|a| a.loops > 0)?.rows?;
        Some(actual.max(1.0) / estimated.max(1.0))
    }

    /// The actual rows are at least [`MISESTIMATE_FACTOR`] times above or below the estimate.
    pub fn is_misestimated(&self) -> bool {
        self.row_estimate_ratio().is_some_and(|r| r >= MISESTIMATE_FACTOR || r <= 1.0 / MISESTIMATE_FACTOR)
    }
}

/// A parsed plan: a tree of [`PlanNode`]s whose root is [`NodeId::ROOT`].
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub format: PlanFormat,
    /// Plan-wide values, such as planning time.
    pub attributes: Vec<Attribute>,
    /// The server's output as text, for the raw view.
    pub raw: String,
    nodes: Vec<PlanNode>,
}

impl Plan {
    pub fn new(format: PlanFormat, root: PlanNode, raw: impl Into<String>) -> Self {
        Self { format, attributes: Vec::new(), raw: raw.into(), nodes: vec![root] }
    }

    /// Appends `node` as the last child of `parent`.
    pub fn add_child(&mut self, parent: NodeId, mut node: PlanNode) -> NodeId {
        let id = NodeId(self.nodes.len());
        node.parent = Some(parent);
        node.depth = self.nodes[parent.0].depth + 1;
        node.children.clear();
        self.nodes[parent.0].children.push(id);
        self.nodes.push(node);
        id
    }

    pub fn node(&self, id: NodeId) -> &PlanNode {
        &self.nodes[id.0]
    }

    /// Every node with its id, parents before their children.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = (NodeId, &PlanNode)> {
        self.nodes.iter().enumerate().map(|(i, n)| (NodeId(i), n))
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.0].children
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id.0].parent
    }

    /// Zero for the root.
    pub fn depth(&self, id: NodeId) -> usize {
        self.nodes[id.0].depth
    }

    /// Nodes below `id`, at any depth.
    pub fn descendant_count(&self, id: NodeId) -> usize {
        self.children(id).iter().map(|c| 1 + self.descendant_count(*c)).sum()
    }

    /// True when the statement ran and the plan carries measured actuals.
    pub fn analyzed(&self) -> bool {
        self.nodes.iter().any(|n| n.actual.is_some())
    }

    /// Milliseconds spent in the node and its children over all loops: the time to the last row
    /// per loop, times the loop count.
    pub fn inclusive_ms(&self, id: NodeId) -> Option<f64> {
        let actual = self.node(id).actual?;
        Some(actual.time?.last_row_ms * actual.loops as f64)
    }

    /// Milliseconds spent in the node itself: its inclusive time minus its children's, never
    /// below zero. A child without its own time, such as MySQL's `-> Hash`, passes on the time
    /// of its children.
    pub fn self_ms(&self, id: NodeId) -> Option<f64> {
        Some((self.inclusive_ms(id)? - self.children_ms(id)).max(0.0))
    }

    /// The whole statement's measured time: the root's inclusive time, or its children's.
    pub fn total_ms(&self) -> Option<f64> {
        self.inclusive_ms(NodeId::ROOT).or_else(|| Some(self.children_ms(NodeId::ROOT)).filter(|ms| *ms > 0.0))
    }

    fn children_ms(&self, id: NodeId) -> f64 {
        self.children(id).iter().map(|c| self.inclusive_ms(*c).unwrap_or_else(|| self.children_ms(*c))).sum()
    }
}

/// Why a result that looks like a plan could not be read.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("could not read the {} plan: {message}", format.label())]
pub struct ParseError {
    pub format: PlanFormat,
    pub message: String,
}

type Parser = fn(&ServerInfo, &ResultSet) -> Option<Result<Plan, ParseError>>;

/// Every format reader. Each recognizes its own result shape and returns `None` for others.
const PARSERS: &[Parser] = &[
    mysql_tree::parse,
    mysql_json::parse_v1,
    mysql_json::parse_v2,
    mariadb_json::parse,
    postgres_json::parse,
    sqlite_eqp::parse,
    sqlite_scanstatus::parse,
];

/// Reads a statement's result as a query plan. `None` when the result isn't a plan in a format
/// with a parser; the caller then shows it as rows or as [`raw_text`].
pub fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    PARSERS.iter().find_map(|parser| parser(server, result))
}

/// `(a = b)` → `a = b`, but `(a = b), (c = d)` stays as it is.
fn strip_outer_parens(text: &str) -> &str {
    if !(text.starts_with('(') && text.ends_with(')')) {
        return text;
    }
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && i < text.len() - 1 {
            return text;
        }
    }
    &text[1..text.len() - 1]
}

/// A result as plain text: the values of a one-column result line by line, otherwise aligned
/// columns under a header.
pub fn raw_text(result: &ResultSet) -> String {
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_text().unwrap_or_else(|| "NULL".into())).collect())
        .collect();
    if result.columns.len() == 1 {
        return cells.into_iter().map(|mut row| row.remove(0)).collect::<Vec<_>>().join("\n");
    }
    let header: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|c| {
            std::iter::once(&header)
                .chain(&cells)
                .map(|row| row.get(c).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    std::iter::once(&header)
        .chain(&cells)
        .map(|row| {
            let line: Vec<String> = row.iter().zip(&widths).map(|(s, w)| format!("{s:<w$}")).collect();
            line.join("  ").trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::value::Value;

    fn timed(op: &str, first: f64, last: f64, rows: f64, loops: u64) -> PlanNode {
        let mut n = PlanNode::new(op);
        n.actual = Some(Actual {
            rows: Some(rows),
            loops,
            time: Some(ActualTime { first_row_ms: Some(first), last_row_ms: last }),
        });
        n
    }

    /// join (10 ms) ─┬─ scan (2 ms) ── hash (untimed) ── scan (3 ms)
    ///               └─ lookup (0.01 ms × 100 loops)
    fn sample() -> (Plan, [NodeId; 5]) {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 1.0, 10.0, 50.0, 1), "");
        let scan = plan.add_child(NodeId::ROOT, timed("scan", 0.1, 2.0, 100.0, 1));
        let hash = plan.add_child(NodeId::ROOT, PlanNode::new("hash"));
        let inner = plan.add_child(hash, timed("scan", 0.1, 3.0, 20.0, 1));
        let lookup = plan.add_child(NodeId::ROOT, timed("lookup", 0.005, 0.01, 1.0, 100));
        (plan, [NodeId::ROOT, scan, hash, inner, lookup])
    }

    #[test]
    fn tree_shape() {
        let (plan, [root, scan, hash, inner, lookup]) = sample();
        assert_eq!(plan.node_count(), 5);
        assert_eq!(plan.children(root), &[scan, hash, lookup]);
        assert_eq!(plan.children(hash), &[inner]);
        assert_eq!(plan.parent(inner), Some(hash));
        assert_eq!(plan.parent(root), None);
        assert_eq!((plan.depth(root), plan.depth(hash), plan.depth(inner)), (0, 1, 2));
        assert_eq!(plan.descendant_count(root), 4);
        assert_eq!(plan.descendant_count(hash), 1);
        assert!(plan.analyzed());
    }

    #[test]
    fn inclusive_time_multiplies_loops() {
        let (plan, [root, _, hash, _, lookup]) = sample();
        assert_eq!(plan.inclusive_ms(root), Some(10.0));
        assert!((plan.inclusive_ms(lookup).unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(plan.inclusive_ms(hash), None);
        assert_eq!(plan.total_ms(), Some(10.0));
    }

    #[test]
    fn self_time_subtracts_children_through_untimed_nodes() {
        let (plan, [root, scan, hash, inner, _]) = sample();
        // 10 - (2 + 3 through the untimed hash + 100 × 0.01)
        assert!((plan.self_ms(root).unwrap() - 4.0).abs() < 1e-9);
        assert_eq!(plan.self_ms(scan), Some(2.0));
        assert_eq!(plan.self_ms(inner), Some(3.0));
        assert_eq!(plan.self_ms(hash), None);
    }

    #[test]
    fn self_time_never_goes_negative() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("limit", 0.0, 0.03, 20.0, 1), "");
        plan.add_child(NodeId::ROOT, timed("materialize", 6.0, 6.6, 20.0, 1));
        assert_eq!(plan.self_ms(NodeId::ROOT), Some(0.0));
    }

    #[test]
    fn total_time_falls_back_to_children() {
        let mut plan = Plan::new(PlanFormat::MariaDbJson, PlanNode::new("union"), "");
        plan.add_child(NodeId::ROOT, timed("block", 0.0, 1.5, 10.0, 1));
        plan.add_child(NodeId::ROOT, timed("block", 0.0, 0.5, 10.0, 1));
        assert_eq!(plan.total_ms(), Some(2.0));
        let estimate_only = Plan::new(PlanFormat::MySqlTree, PlanNode::new("scan"), "");
        assert_eq!(estimate_only.total_ms(), None);
        assert!(!estimate_only.analyzed());
    }

    #[test]
    fn row_estimate_ratio() {
        let with = |estimated: f64, actual: f64, loops: u64| {
            let mut n = timed("scan", 0.0, 1.0, actual, loops);
            n.estimate = Some(Estimate { rows: estimated, cost: None });
            n
        };
        assert_eq!(with(10.0, 100.0, 1).row_estimate_ratio(), Some(10.0));
        assert!(with(10.0, 100.0, 1).is_misestimated());
        assert!(with(1000.0, 99.0, 1).is_misestimated());
        assert!(!with(1000.0, 101.0, 1).is_misestimated());
        // MySQL's `rows=0` on a union's temporary table means no estimate.
        assert_eq!(with(0.0, 67.0, 1).row_estimate_ratio(), None);
        assert_eq!(with(0.5, 67.0, 1).row_estimate_ratio(), Some(67.0));
        // Fractions of a row are not a misestimate.
        assert!(!with(0.333, 0.01, 1083).is_misestimated());
        assert_eq!(with(5.0, 0.0, 0).row_estimate_ratio(), None);
        assert_eq!(timed("scan", 0.0, 1.0, 5.0, 1).row_estimate_ratio(), None);
        let mut uncounted = with(10.0, 0.0, 1);
        uncounted.actual.as_mut().unwrap().rows = None;
        assert_eq!(uncounted.row_estimate_ratio(), None);
    }

    #[test]
    fn raw_text_lines_and_columns() {
        let one = ResultSet {
            columns: vec![ColumnMeta::new("QUERY PLAN", "text")],
            rows: vec![vec![Value::text("Seq Scan on t")].into(), vec![Value::text("  Filter: (a > 1)")].into()],
            truncated: false,
        };
        assert_eq!(raw_text(&one), "Seq Scan on t\n  Filter: (a > 1)");
        let table = ResultSet {
            columns: vec![ColumnMeta::new("id", ""), ColumnMeta::new("detail", "")],
            rows: vec![vec![Value::Int(4), Value::text("SCAN t")].into(), vec![Value::Int(15), Value::Null].into()],
            truncated: false,
        };
        assert_eq!(raw_text(&table), "id  detail\n4   SCAN t\n15  NULL");
    }
}
