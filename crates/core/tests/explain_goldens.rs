//! Parses the EXPLAIN output captured from real servers (see `common/golden.rs`). Timings change
//! with every capture, so these tests check structure, never measured values.

mod common;

use common::golden::{self, Golden};
use tableplus_core::Flavor;
use tableplus_core::plan::{self, NodeId, Plan, PlanFormat};

/// The plan's nodes in tree order, walked through the public parent-to-children links.
fn tree_order(plan: &Plan) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![NodeId::ROOT];
    while let Some(id) = stack.pop() {
        out.push(id);
        stack.extend(plan.children(id).iter().rev());
    }
    out
}

/// `(depth, text)` for each `->` line, the depth read from MySQL's four-space indentation.
fn tree_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .filter_map(|line| {
            let body = line.trim_start();
            body.strip_prefix("-> ").map(|rest| ((line.len() - body.len()) / 4, rest))
        })
        .collect()
}

fn is_tree_variant(g: &Golden) -> bool {
    g.server.flavor == Flavor::MySql
        && g.outcome == "ok"
        && (g.variant == "tree" || g.variant == "analyze" || (g.target == "mysql-9.7" && g.variant == "plain"))
}

#[test]
fn mysql_tree_goldens_keep_their_line_structure() {
    let Some(goldens) = golden::load() else { return };
    let tree: Vec<&Golden> = goldens.iter().filter(|g| is_tree_variant(g)).collect();
    assert!(tree.len() >= 20, "expected the TREE goldens of MySQL 8.0.18 to 9.7, found {}", tree.len());
    let mut parsed = 0;
    for g in tree {
        let text = g.text().unwrap_or_else(|| panic!("{}: one text cell", g.name));
        let lines = tree_lines(&text);
        let plan = match plan::parse(&g.server, g.result.as_ref().unwrap()) {
            None => {
                assert!(lines.is_empty(), "{}: TREE text was not recognized", g.name);
                continue;
            }
            Some(result) => result.unwrap_or_else(|e| panic!("{}: {e}", g.name)),
        };
        parsed += 1;
        assert_eq!(plan.format, PlanFormat::MySqlTree, "{}", g.name);
        assert_eq!(plan.raw, text, "{}: raw text is the server's output", g.name);
        assert_eq!(plan.analyzed(), g.variant == "analyze", "{}", g.name);
        let order = tree_order(&plan);
        assert_eq!(order.len(), lines.len(), "{}: one node per `->` line", g.name);
        for (id, (depth, line)) in order.iter().zip(&lines) {
            let node = plan.node(*id);
            let at = format!("{}: {line}", g.name);
            assert_eq!(plan.depth(*id), *depth, "depth of {at}");
            assert!(line.starts_with(node.operation.as_str()), "operation {:?} of {at}", node.operation);
            for part in [&node.object, &node.index, &node.condition].into_iter().flatten() {
                assert!(line.contains(part.as_str()), "{part:?} comes from {at}");
            }
            assert_eq!(node.estimate.is_some(), line.contains("(cost="), "estimate of {at}");
            assert_eq!(
                node.actual.is_some(),
                line.contains("(actual time=") || line.contains("(never executed)"),
                "actuals of {at}"
            );
            assert!(node.attributes.iter().any(|a| a.key == "step"), "{at}");
            if let (Some(total), Some(own)) = (plan.inclusive_ms(*id), plan.self_ms(*id)) {
                assert!((0.0..=total + 1e-9).contains(&own), "self time within inclusive time for {at}");
            }
        }
    }
    assert!(parsed >= 19, "parsed {parsed} TREE goldens");
}

#[test]
fn mysql_84_portable_analyze_reads_like_the_text() {
    let Some(g) = golden::one("mysql-8.4", "portable", "analyze") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let root = plan.node(NodeId::ROOT);
    assert_eq!((root.operation.as_str(), root.condition.as_deref()), ("Limit", Some("20 row(s)")));
    assert!(plan.total_ms().is_some_and(|ms| ms > 0.0));
    let deepest = plan.nodes().map(|(id, _)| plan.depth(id)).max().unwrap();
    assert!(deepest >= 15, "the union and the hash joins nest deeply, got {deepest}");

    let lookup = plan
        .nodes()
        .map(|(_, n)| n)
        .find(|n| n.operation == "Covering index lookup" && n.object.as_deref() == Some("p2"))
        .expect("the p2 lookup");
    assert_eq!(lookup.index.as_deref(), Some("idx_products_category_id"));
    assert_eq!(lookup.condition.as_deref(), Some("category_id=cat2.category_id"));
    assert!(lookup.estimate.is_some() && lookup.actual.is_some());

    let hashes: Vec<NodeId> = plan.nodes().filter(|(_, n)| n.operation == "Hash").map(|(id, _)| id).collect();
    assert!(!hashes.is_empty());
    for id in hashes {
        let n = plan.node(id);
        assert!(n.estimate.is_none() && n.actual.is_none(), "`-> Hash` has no numbers");
        assert_eq!(plan.children(id).len(), 1, "`-> Hash` builds from one input");
        assert_eq!(plan.inclusive_ms(id), None);
    }

    let having = plan
        .nodes()
        .map(|(_, n)| n)
        .find(|n| n.operation == "Filter" && n.condition.as_deref() == Some("count(distinct orders.order_id) >= 3"))
        .expect("the HAVING filter");
    assert!(having.estimate.is_none() && having.actual.is_some(), "MySQL measures but doesn't estimate HAVING");
}

#[test]
fn mysql_97_plain_explain_is_an_estimate_tree() {
    let Some(g) = golden::one("mysql-9.7", "portable", "plain") else { return };
    assert!(g.statement.starts_with("EXPLAIN SELECT"), "a bare EXPLAIN");
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    assert!(!plan.analyzed());
    assert_eq!(plan.total_ms(), None);
    assert!(plan.nodes().any(|(_, n)| n.estimate.is_some_and(|e| e.cost.is_some())));
}

#[test]
fn other_output_is_not_read_as_a_tree() {
    let Some(goldens) = golden::load() else { return };
    let mut checked = 0;
    for g in goldens.iter().filter(|g| g.outcome == "ok" && !is_tree_variant(g)) {
        if let Some(Ok(plan)) = plan::parse(&g.server, g.result.as_ref().unwrap()) {
            assert_ne!(plan.format, PlanFormat::MySqlTree, "{} is not TREE text", g.name);
        }
        checked += 1;
    }
    assert!(checked > 150, "checked {checked} goldens");
}

fn is_mariadb_json(g: &Golden) -> bool {
    g.server.flavor == Flavor::MariaDb && g.outcome == "ok" && (g.variant == "json" || g.variant == "analyze-json")
}

/// A JSON value with every object member kept in order, duplicate keys included, read by hand
/// so these tests share nothing with the parser under test.
enum Shape {
    Object(Vec<(String, Shape)>),
    Array(Vec<Shape>),
    Scalar(String),
}

impl Shape {
    fn read(text: &str) -> Shape {
        let mut at = 0;
        let shape = Self::value(text.as_bytes(), &mut at);
        assert!(text[at..].trim().is_empty(), "trailing text after the JSON value");
        shape
    }

    fn value(s: &[u8], at: &mut usize) -> Shape {
        let skip = |at: &mut usize| {
            while s[*at].is_ascii_whitespace() {
                *at += 1;
            }
        };
        skip(at);
        let (open, close) = (s[*at], if s[*at] == b'{' { b'}' } else { b']' });
        if open == b'"' {
            return Shape::Scalar(Self::string(s, at));
        }
        if open != b'{' && open != b'[' {
            let start = *at;
            while !matches!(s[*at], b',' | b'}' | b']') {
                *at += 1;
            }
            return Shape::Scalar(String::from_utf8_lossy(&s[start..*at]).trim().to_string());
        }
        *at += 1;
        let (mut members, mut items) = (Vec::new(), Vec::new());
        loop {
            skip(at);
            if s[*at] == close {
                *at += 1;
                break;
            }
            if open == b'{' {
                let key = Self::string(s, at);
                skip(at);
                assert_eq!(s[*at], b':');
                *at += 1;
                members.push((key, Self::value(s, at)));
            } else {
                items.push(Self::value(s, at));
            }
            skip(at);
            if s[*at] == b',' {
                *at += 1;
            }
        }
        if open == b'{' { Shape::Object(members) } else { Shape::Array(items) }
    }

    fn string(s: &[u8], at: &mut usize) -> String {
        assert_eq!(s[*at], b'"');
        let start = *at + 1;
        *at = start;
        while s[*at] != b'"' {
            *at += if s[*at] == b'\\' { 2 } else { 1 };
        }
        *at += 1;
        String::from_utf8_lossy(&s[start..*at - 1]).into_owned()
    }

    fn get(&self, key: &str) -> Option<&Shape> {
        match self {
            Shape::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn scalar(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Shape::Scalar(s) => Some(s.trim_matches('"')),
            _ => None,
        }
    }
}

/// The steps MariaDB's JSON describes, by the operation each one gets in the plan.
const MARIADB_STEPS: &[(&str, &str)] = &[
    ("filesort", "Sort"),
    ("temporary_table", "Temporary table"),
    ("union_result", "Union"),
    ("recursive_union", "Recursive union"),
    ("materialized", "Materialize"),
    ("materialization", "Materialize subquery"),
    ("subquery_cache", "Subquery cache"),
    ("expression_cache", "Expression cache"),
    ("read_sorted_file", "Read sorted file"),
    ("window_functions_computation", "Window functions"),
    ("duplicates_removal", "Remove duplicates"),
];

/// A table access as the JSON states it.
#[derive(Debug, PartialEq)]
struct TableFacts {
    name: String,
    depth: usize,
    /// Query blocks with a `select_id` around it.
    selects: usize,
    rows: bool,
    cost: bool,
    r_loops: Option<u64>,
    timed: bool,
}

#[derive(Default)]
struct Expected {
    /// Steps per operation; `table` for table accesses, `Select` for numbered query blocks.
    counts: std::collections::BTreeMap<String, usize>,
    tables: Vec<TableFacts>,
}

impl Expected {
    fn of(json: &Shape) -> Self {
        let mut out = Expected::default();
        out.walk(json, 0, 0);
        out
    }

    fn count(&mut self, operation: &str) {
        *self.counts.entry(operation.to_string()).or_default() += 1;
    }

    /// `depth` is the depth of the steps directly in `shape`.
    fn walk(&mut self, shape: &Shape, selects: usize, depth: usize) {
        match shape {
            Shape::Array(items) => items.iter().for_each(|item| self.walk(item, selects, depth)),
            Shape::Scalar(_) => {}
            Shape::Object(members) => {
                // A join of two or more members reads as one `Nested loop`. Its members are the
                // object's own table-like keys plus every entry of its `nested_loop` arrays.
                let joined =
                    |k: &str| matches!(k, "table" | "read_sorted_file" | "duplicates_removal" | "block-nl-join");
                let mut join = members.iter().filter(|(k, _)| joined(k)).count();
                for (_, list) in members.iter().filter(|(k, _)| k == "nested_loop") {
                    if let Shape::Array(items) = list {
                        join += items.len();
                    }
                }
                if join >= 2 {
                    self.count("Nested loop");
                }
                for (key, value) in members {
                    let here = depth + usize::from(join >= 2 && (joined(key) || key == "nested_loop"));
                    let (mut inner, mut step) = (selects, true);
                    if key == "query_block" && value.get("select_id").is_some() {
                        self.count("Select");
                        inner += 1;
                    } else if key == "table" {
                        self.count("table");
                        let number = |k: &str| value.scalar(k).and_then(|v| v.parse::<f64>().ok());
                        self.tables.push(TableFacts {
                            name: value.scalar("table_name").unwrap_or_default().to_string(),
                            depth: here,
                            selects,
                            rows: number("rows").is_some(),
                            cost: number("cost").is_some(),
                            r_loops: number("r_loops").map(|n| n as u64),
                            timed: number("r_table_time_ms").is_some(),
                        });
                    } else if let Some((_, op)) = MARIADB_STEPS.iter().find(|(k, _)| k == key)
                        && (matches!(value, Shape::Object(_)) || key == "duplicates_removal")
                    {
                        self.count(op);
                    } else {
                        step = false;
                    }
                    self.walk(value, inner, here + usize::from(step));
                }
            }
        }
    }
}

#[test]
fn mariadb_json_goldens_keep_the_server_structure() {
    let Some(goldens) = golden::load() else { return };
    let mut parsed = 0;
    let mut costed = 0;
    for g in goldens.iter().filter(|g| is_mariadb_json(g)) {
        let text = g.text().unwrap_or_else(|| panic!("{}: one text cell", g.name));
        let plan = plan::parse(&g.server, g.result.as_ref().unwrap())
            .unwrap_or_else(|| panic!("{}: JSON was not recognized", g.name))
            .unwrap_or_else(|e| panic!("{}: {e}", g.name));
        parsed += 1;
        let analyze = g.variant == "analyze-json";
        assert_eq!(plan.format, PlanFormat::MariaDbJson, "{}", g.name);
        assert_eq!(plan.raw, text, "{}: raw text is the server's output", g.name);
        assert_eq!(plan.analyzed(), analyze, "{}", g.name);

        let expected = Expected::of(&Shape::read(&text));
        let order = tree_order(&plan);
        assert_eq!(order.len(), plan.node_count(), "{}: every node is reachable from the root", g.name);
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        let mut tables = Vec::new();
        for id in &order {
            let node = plan.node(*id);
            let selects = std::iter::successors(plan.parent(*id), |p| plan.parent(*p))
                .filter(|p| plan.node(*p).operation.starts_with("Select #"))
                .count();
            let kind = if node.operation.starts_with("Select #") {
                "Select"
            } else if node.object.is_some() && !node.operation.ends_with("nion") {
                "table"
            } else {
                node.operation.as_str()
            };
            *counts.entry(kind.to_string()).or_default() += 1;
            if kind == "table" {
                tables.push(TableFacts {
                    name: node.object.clone().unwrap(),
                    depth: plan.depth(*id),
                    selects,
                    rows: node.estimate.is_some(),
                    cost: node.estimate.is_some_and(|e| e.cost.is_some()),
                    r_loops: node.actual.map(|a| a.loops),
                    timed: node.actual.is_some_and(|a| a.time.is_some()),
                });
            }
            if let (Some(total), Some(own)) = (plan.inclusive_ms(*id), plan.self_ms(*id)) {
                assert!((0.0..=total + 1e-9).contains(&own), "{}: self time within inclusive time", g.name);
            }
        }
        assert_eq!(counts, expected.counts, "{}: steps per operation", g.name);
        assert_eq!(tables, expected.tables, "{}: tables in order, nesting and numbers", g.name);
        if analyze {
            assert!(plan.total_ms().is_some_and(|ms| ms > 0.0), "{}: a measured total", g.name);
            assert!(tables.iter().all(|t| t.timed == t.r_loops.is_some_and(|l| l > 0)), "{}", g.name);
        } else {
            assert_eq!(plan.total_ms(), None, "{}", g.name);
        }
        costed += tables.iter().filter(|t| t.cost).count();
        let has_cost = g.server.version >= tableplus_core::Version::new(11, 0, 0);
        assert_eq!(tables.iter().any(|t| t.cost), has_cost, "{}: MariaDB 11 adds costs", g.name);
    }
    assert_eq!(parsed, 20, "the json and analyze-json goldens of MariaDB 10.6, 10.11, 11.4 and 11.8");
    assert!(costed > 0);
}

#[test]
fn mariadb_118_portable_analyze_reads_like_the_json() {
    let Some(g) = golden::one("mariadb-11.8", "portable", "analyze-json") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let root = plan.node(NodeId::ROOT);
    assert_eq!((root.operation.as_str(), root.object.as_deref()), ("Union", Some("<union1,5>")));
    let blocks: Vec<&str> = plan.children(NodeId::ROOT).iter().map(|c| plan.node(*c).operation.as_str()).collect();
    assert_eq!(blocks, ["Select #1", "Select #5"]);
    assert_eq!(plan.inclusive_ms(NodeId::ROOT), None, "the union result has no time of its own");
    assert!(plan.total_ms().is_some_and(|ms| ms > 0.0));

    let find = |object: &str| {
        plan.nodes().find(|(_, n)| n.object.as_deref() == Some(object)).unwrap_or_else(|| panic!("table {object}"))
    };
    let (derived, node) = find("<derived3>");
    assert_eq!((node.operation.as_str(), node.index.as_deref()), ("Index lookup", Some("key0")));
    let materialize = plan.children(derived)[0];
    assert_eq!(plan.node(materialize).operation, "Materialize");
    let select3 = plan.children(materialize)[0];
    assert_eq!(plan.node(select3).operation, "Select #3");
    assert!(
        plan.inclusive_ms(derived).unwrap() > plan.inclusive_ms(select3).unwrap(),
        "the derived table's time adds the materialization it triggered"
    );

    let (_, o) = plan
        .nodes()
        .find(|(_, n)| n.object.as_deref() == Some("o") && n.actual.is_some_and(|a| a.loops == 404))
        .expect("the semi-join's o");
    assert_eq!(o.operation, "Single-row index lookup");
    assert!(o.condition.as_deref().is_some_and(|c| c.starts_with("o.order_date >= '1997-01-01'")));
    assert!(o.estimate.is_some_and(|e| e.cost.is_some()));
    for key in ["access_type", "r_filtered", "r_table_time_ms", "r_other_time_ms", "r_engine_stats.pages_accessed"] {
        assert!(o.attributes.iter().any(|a| a.key == key), "o keeps {key}");
    }
    let cache = plan.nodes().map(|(_, n)| n).find(|n| n.operation == "Subquery cache").expect("the cache");
    assert!(cache.attributes.iter().any(|a| a.key == "r_hit_ratio"));
    assert!(plan.attributes.iter().any(|a| a.key == "query_optimization.r_total_time_ms"));
}

#[test]
fn mariadb_106_repeated_table_keys_read_as_a_join() {
    let Some(g) = golden::one("mariadb-10.6", "portable", "json") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let select1 = plan.children(NodeId::ROOT)[0];
    let join = plan.children(select1)[0];
    assert_eq!(plan.node(join).operation, "Nested loop");
    let members: Vec<String> = plan
        .children(join)
        .iter()
        .map(|c| plan.node(*c).object.clone().unwrap_or_else(|| plan.node(*c).operation.clone()))
        .collect();
    assert_eq!(members, ["Read sorted file", "o", "sh", "e", "<subquery4>", "<derived3>"]);
}
