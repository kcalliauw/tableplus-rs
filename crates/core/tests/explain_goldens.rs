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

fn sqlite_goldens(variant: &str) -> Vec<Golden> {
    let Some(goldens) = golden::load() else { return Vec::new() };
    goldens.into_iter().filter(|g| g.server.flavor == Flavor::Sqlite && g.variant == variant).collect()
}

#[test]
fn sqlite_query_plans_follow_their_parent_ids() {
    let Some(_) = golden::dir() else { return };
    let goldens = sqlite_goldens("query-plan");
    assert_eq!(goldens.len(), 3, "portable, modern and dml");
    for g in goldens {
        let rs = g.result.as_ref().unwrap();
        let rows: Vec<(i64, i64, String)> = rs
            .rows
            .iter()
            .map(|r| {
                (
                    r[0].to_text().unwrap().parse().unwrap(),
                    r[1].to_text().unwrap().parse().unwrap(),
                    r[3].to_text().unwrap(),
                )
            })
            .collect();
        let plan = plan::parse(&g.server, rs).expect("a plan").unwrap_or_else(|e| panic!("{}: {e}", g.name));
        assert_eq!(plan.format, PlanFormat::SqliteQueryPlan, "{}", g.name);
        assert!(!plan.analyzed(), "{}", g.name);
        let synthetic_root = rows.iter().filter(|r| r.1 == 0).count() > 1;
        if synthetic_root {
            assert_eq!(plan.node(NodeId::ROOT).operation, "Query", "{}: a synthetic root", g.name);
        }
        assert_eq!(plan.node_count(), rows.len() + synthetic_root as usize, "{}: one node per row", g.name);

        let nodes: Vec<NodeId> = plan.nodes().map(|(id, _)| id).skip(synthetic_root as usize).collect();
        for (i, (id, parent, detail)) in rows.iter().enumerate() {
            let at = format!("{}: {id} {detail}", g.name);
            let node = plan.node(nodes[i]);
            let expected_parent = match rows.iter().position(|r| r.0 == *parent) {
                Some(p) => Some(nodes[p]),
                None => synthetic_root.then_some(NodeId::ROOT),
            };
            assert_eq!(plan.parent(nodes[i]), expected_parent, "parent of {at}");
            assert_eq!(node.attributes[0].value, *detail, "detail of {at}");
            assert!(node.estimate.is_none() && node.actual.is_none(), "{at}");
            assert_ne!(&node.operation, detail, "every golden line is a known wording: {at}");
            for part in [&node.object, &node.index, &node.condition].into_iter().flatten() {
                assert!(detail.contains(part.as_str()), "{part:?} comes from {at}");
            }
            let words: Vec<&str> = detail.split(' ').collect();
            match words[0] {
                "SCAN" => {
                    assert_eq!((node.operation.as_str(), node.object.as_deref()), ("Scan", Some(words[1])), "{at}")
                }
                "SEARCH" => {
                    assert_eq!((node.operation.as_str(), node.object.as_deref()), ("Search", Some(words[1])), "{at}");
                    assert!(
                        node.index.is_some() && node.condition.is_some(),
                        "a search uses an index with a key: {at}"
                    );
                }
                _ => {}
            }
        }
    }
}

#[test]
fn sqlite_portable_plan_nests_its_subqueries() {
    let Some(g) = golden::one("sqlite-bundled", "portable", "query-plan") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let find =
        |op: &str| plan.nodes().find(|(_, n)| n.operation == op).map(|(id, _)| id).unwrap_or_else(|| panic!("{op}"));
    let merge = find("Merge");
    assert_eq!(plan.node(merge).condition.as_deref(), Some("UNION ALL"));
    let arms: Vec<&str> = plan.children(merge).iter().map(|c| plan.node(*c).operation.as_str()).collect();
    assert_eq!(arms, ["Left", "Right"]);
    let correlated = find("Correlated scalar subquery 3");
    let lookups: Vec<(&str, &str)> = plan
        .children(correlated)
        .iter()
        .map(|c| (plan.node(*c).object.as_deref().unwrap(), plan.node(*c).index.as_deref().unwrap()))
        .collect();
    assert_eq!(
        lookups,
        [
            ("od2", "COVERING INDEX sqlite_autoindex_order_details_1"),
            ("p2", "INTEGER PRIMARY KEY"),
            ("cat2", "INTEGER PRIMARY KEY")
        ]
    );
}

#[test]
fn sqlite_bytecode_stays_raw_text() {
    for g in sqlite_goldens("bytecode") {
        assert!(plan::parse(&g.server, g.result.as_ref().unwrap()).is_none(), "{}", g.name);
    }
}
