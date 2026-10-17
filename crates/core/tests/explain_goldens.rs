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

type Json = serde_json::Map<String, serde_json::Value>;

fn json(g: &Golden) -> Json {
    serde_json::from_str(&g.text().unwrap_or_else(|| panic!("{}: one text cell", g.name)))
        .unwrap_or_else(|e| panic!("{}: {e}", g.name))
}

fn is_json_v1(g: &Golden) -> bool {
    g.server.flavor == Flavor::MySql && g.outcome == "ok" && g.variant == "json" && g.target != "mysql-9.7"
}

fn is_json_v2(g: &Golden) -> bool {
    g.server.flavor == Flavor::MySql
        && g.outcome == "ok"
        && (g.variant.ends_with("json-v2") || (g.target == "mysql-9.7" && g.variant.ends_with("json")))
}

/// A version 1 table as the JSON states it: name, access type, key, condition, whether it has
/// rows and costs, and how many query blocks enclose it.
#[derive(Debug, PartialEq)]
struct V1Table {
    name: Option<String>,
    access: Option<String>,
    key: Option<String>,
    condition: Option<String>,
    rows: bool,
    cost: bool,
    blocks: usize,
}

/// Every object with a `table_name` in document order, and the number of query blocks.
fn v1_tables(value: &serde_json::Value, blocks: usize, tables: &mut Vec<V1Table>, block_count: &mut usize) {
    match value {
        serde_json::Value::Object(obj) => {
            let s = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
            if obj.contains_key("table_name") {
                let has_rows = obj.contains_key("rows_examined_per_scan") || obj.contains_key("rows");
                tables.push(V1Table {
                    name: s("table_name"),
                    access: s("access_type"),
                    key: s("key"),
                    condition: s("attached_condition"),
                    rows: has_rows && !obj.contains_key("query_specifications"),
                    cost: has_rows && obj.contains_key("cost_info"),
                    blocks,
                });
            }
            for (k, v) in obj {
                let inner = if k == "query_block" {
                    *block_count += 1;
                    blocks + 1
                } else {
                    blocks
                };
                v1_tables(v, inner, tables, block_count);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| v1_tables(v, blocks, tables, block_count)),
        _ => {}
    }
}

fn is_block(node: &tableplus_core::plan::PlanNode) -> bool {
    node.operation.starts_with("Select #") || node.operation == "Query"
}

#[test]
fn mysql_json_v1_goldens_keep_every_table_and_block() {
    let Some(goldens) = golden::load() else { return };
    let v1: Vec<&Golden> = goldens.iter().filter(|g| is_json_v1(g)).collect();
    assert!(v1.len() >= 13, "expected the JSON goldens of MySQL 5.6 to 8.4, found {}", v1.len());
    for g in v1 {
        let doc = serde_json::Value::Object(json(g));
        let (mut expected, mut blocks) = (Vec::new(), 0);
        v1_tables(&doc, 0, &mut expected, &mut blocks);
        let plan = plan::parse(&g.server, g.result.as_ref().unwrap())
            .expect("a plan")
            .unwrap_or_else(|e| panic!("{}: {e}", g.name));
        assert_eq!(plan.format, PlanFormat::MySqlJson, "{}", g.name);
        assert_eq!(plan.raw, g.text().unwrap(), "{}: raw text is the server's output", g.name);
        assert!(!plan.analyzed(), "{}: version 1 has no actuals", g.name);
        let order = tree_order(&plan);
        let actual: Vec<V1Table> = order
            .iter()
            .filter(|id| plan.node(**id).object.is_some())
            .map(|id| {
                let n = plan.node(*id);
                let mut up = plan.parent(*id);
                let mut enclosing = 0;
                while let Some(p) = up {
                    enclosing += usize::from(is_block(plan.node(p)));
                    up = plan.parent(p);
                }
                V1Table {
                    name: n.object.clone(),
                    access: n.access_type.clone(),
                    key: n.index.clone(),
                    condition: n.condition.clone(),
                    rows: n.estimate.is_some(),
                    cost: n.estimate.is_some_and(|e| e.cost.is_some()),
                    blocks: enclosing,
                }
            })
            .collect();
        assert_eq!(actual, expected, "{}: tables in document order", g.name);
        assert_eq!(plan.nodes().filter(|(_, n)| is_block(n)).count(), blocks, "{}: one node per query block", g.name);
        let costed = expected.iter().any(|t| t.cost);
        assert_eq!(costed, g.target != "mysql-5.6", "{}: costs from 5.7 on", g.name);
    }
}

/// A version 2 step as the JSON states it, with its depth below the top step.
#[derive(Debug)]
struct V2Step {
    depth: usize,
    operation: String,
    access: Option<String>,
    estimate: bool,
    actual: bool,
}

/// Steps in document order through every list of steps, a heading taking a level of its own.
fn v2_steps(step: &Json, depth: usize, out: &mut Vec<V2Step>) {
    let heading = step.get("heading").and_then(|h| h.as_str());
    if let Some(h) = heading {
        out.push(V2Step { depth, operation: h.to_string(), access: None, estimate: false, actual: false });
    }
    let depth = depth + usize::from(heading.is_some());
    out.push(V2Step {
        depth,
        operation: step["operation"].as_str().unwrap().to_string(),
        access: step.get("access_type").and_then(|a| a.as_str()).map(str::to_string),
        estimate: step.contains_key("estimated_rows"),
        actual: step.contains_key("actual_loops"),
    });
    for list in step.values().filter_map(|v| v.as_array()) {
        for child in list.iter().filter_map(|v| v.as_object()).filter(|o| o.contains_key("operation")) {
            v2_steps(child, depth + 1, out);
        }
    }
}

#[test]
fn mysql_json_v2_goldens_keep_every_step() {
    let Some(goldens) = golden::load() else { return };
    let v2: Vec<&Golden> = goldens.iter().filter(|g| is_json_v2(g)).collect();
    assert!(v2.len() >= 15, "expected the version 2 goldens of MySQL 8.4 and 9.7, found {}", v2.len());
    for g in v2 {
        let doc = json(g);
        let top = doc.get("query_plan").and_then(|v| v.as_object()).unwrap_or(&doc);
        let mut expected = Vec::new();
        v2_steps(top, 0, &mut expected);
        let plan = plan::parse(&g.server, g.result.as_ref().unwrap())
            .expect("a plan")
            .unwrap_or_else(|e| panic!("{}: {e}", g.name));
        assert_eq!(plan.format, PlanFormat::MySqlJsonV2, "{}", g.name);
        assert_eq!(plan.raw, g.text().unwrap(), "{}", g.name);
        assert_eq!(plan.analyzed(), g.variant.starts_with("analyze"), "{}", g.name);
        let order = tree_order(&plan);
        assert_eq!(order.len(), expected.len(), "{}: one node per step and heading", g.name);
        for (id, step) in order.iter().zip(&expected) {
            let node = plan.node(*id);
            let at = format!("{}: {}", g.name, step.operation);
            assert_eq!(plan.depth(*id), step.depth, "depth of {at}");
            assert!(step.operation.starts_with(node.operation.as_str()), "operation {:?} of {at}", node.operation);
            for part in [&node.object, &node.index, &node.condition].into_iter().flatten() {
                assert!(step.operation.contains(part.as_str()), "{part:?} comes from {at}");
            }
            assert_eq!(node.estimate.is_some(), step.estimate, "estimate of {at}");
            assert_eq!(node.actual.is_some(), step.actual, "actuals of {at}");
            assert_eq!(node.access_type, step.access, "access type of {at}");
        }
    }
}

/// Version 2 lists the same steps as TREE text, in the same order. Only a select-list subquery
/// sits one level deeper: under the step that produces the select list, where TREE prints it
/// beside that step.
#[test]
fn mysql_json_v2_has_the_tree_formats_steps() {
    let mut compared = 0;
    for target in ["mysql-8.4", "mysql-9.7"] {
        for query in ["portable", "modern", "dml"] {
            for (tree, json) in [("tree", "json-v2"), ("analyze", "analyze-json-v2")] {
                let (Some(tree), Some(json)) = (golden::one(target, query, tree), golden::one(target, query, json))
                else {
                    return;
                };
                if tree.outcome != "ok" || json.outcome != "ok" {
                    continue;
                }
                let steps = |g: &Golden| {
                    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).unwrap().unwrap();
                    let in_projection = |id: NodeId| {
                        std::iter::successors(Some(id), |i| plan.parent(*i)).any(|i| {
                            plan.node(i).condition.as_deref().is_some_and(|c| c.starts_with("subquery in projection"))
                        })
                    };
                    tree_order(&plan)
                        .into_iter()
                        .map(|id| {
                            let n = plan.node(id);
                            let depth = plan.depth(id) - usize::from(g.variant.contains("json") && in_projection(id));
                            (depth, n.operation.clone(), n.object.clone(), n.index.clone())
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(steps(&json), steps(&tree), "{target}/{query}: {} and {}", json.variant, tree.variant);
                compared += 1;
            }
        }
    }
    assert!(compared >= 8, "compared {compared} pairs");
}
