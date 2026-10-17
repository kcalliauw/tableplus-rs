//! Parses the EXPLAIN output captured from real servers (see `common/golden.rs`). Timings change
//! with every capture, so these tests check structure, never measured values.

mod common;

use common::golden::{self, Golden};
use tableplus_core::Flavor;
use tableplus_core::plan::{self, Access, NodeId, Plan, PlanFormat};

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
fn mysql_97_costs_leave_out_the_union_below_the_limit() {
    let Some(g) = golden::one("mysql-9.7", "portable", "tree") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let find = |op: &str| plan.nodes().find(|(_, n)| n.operation == op).map(|(id, _)| id).unwrap();
    // `-> Limit: 20 row(s)  (cost=2.85..2.85 rows=0)`
    assert_eq!(plan.estimated_rows(NodeId::ROOT), None, "rows=0 is MySQL's placeholder");
    assert_eq!(plan.estimated_cost(NodeId::ROOT).map(|c| c.total), Some(2.85));
    // `-> Union all materialize  (cost=0..0 rows=0)`
    let union = find("Union all materialize");
    assert_eq!((plan.estimated_rows(union), plan.estimated_cost(union)), (None, None));
    // The branches cost 757 and 31,389, which the limit's own 2.85 leaves out.
    let spans = plan.cost_spans();
    assert!(spans[NodeId::ROOT.index()] > 757.0 + 31389.0, "{}", spans[NodeId::ROOT.index()]);
    assert_eq!(spans[NodeId::ROOT.index()], spans[union.index()]);
}

#[test]
fn postgres_12_costs_already_include_the_steps_below() {
    let Some(g) = golden::one("postgres-12", "portable", "json") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let spans = plan.cost_spans();
    for (id, _) in plan.nodes() {
        let cost = plan.estimated_cost(id).expect("every PostgreSQL step is costed").total;
        assert!(spans[id.index()] <= cost * plan::SPAN_SLACK, "{id:?}: {} over {cost}", spans[id.index()]);
    }
}

/// `(object, access)` for every step that reads rows, in plan order.
fn reads(plan: &Plan) -> Vec<(String, Access)> {
    plan.nodes()
        .filter_map(|(id, n)| Some((n.object.clone().unwrap_or_else(|| n.operation.clone()), plan.access(id)?)))
        .collect()
}

#[test]
fn full_scans_are_of_real_tables_and_internal_results_have_their_own_class() {
    let Some(goldens) = golden::load() else { return };
    let (mut plans, mut internal) = (0, 0);
    for g in goldens.iter().filter(|g| g.outcome == "ok") {
        let Some(Ok(plan)) = plan::parse(&g.server, g.result.as_ref().unwrap()) else { continue };
        plans += 1;
        for (id, node) in plan.nodes() {
            let object = node.object.as_deref().unwrap_or("");
            match plan.access(id) {
                Some(Access::FullScan) => {
                    assert!(!object.starts_with('<') && !object.starts_with('('), "{}: {object} is built", g.name);
                    // JSON version 1 and MariaDB hang subqueries of a table's condition below it.
                    if matches!(plan.format, PlanFormat::MySqlTree | PlanFormat::MySqlJsonV2) {
                        assert!(plan.children(id).is_empty(), "{}: a full scan of {object} reads an input", g.name);
                    }
                }
                Some(Access::InternalResult) => internal += 1,
                _ => {}
            }
        }
    }
    assert!(plans > 100 && internal > 100, "{plans} plans, {internal} internal results");
}

#[test]
fn access_classes_of_the_portable_query_on_each_engine() {
    use Access::*;
    let classes = |target: &str, variant: &str| {
        let g = golden::one(target, "portable", variant)?;
        Some(reads(&plan::parse(&g.server, g.result.as_ref().unwrap())?.ok()?))
    };
    let has = |reads: &[(String, Access)], object: &str, access: Access| {
        assert!(reads.iter().any(|(o, a)| o == object && *a == access), "{object} as {access:?} in {reads:?}");
    };
    if let Some(r) = classes("mysql-8.4", "analyze") {
        has(&r, "<union temporary>", InternalResult);
        has(&r, "<subquery4>", InternalResult);
        has(&r, "c", FullScan);
        has(&r, "e", UniqueLookup);
        has(&r, "od", IndexScan);
    }
    if let Some(r) = classes("mysql-5.7", "json") {
        has(&r, "<union1,5>", InternalResult);
        has(&r, "t", InternalResult);
        has(&r, "cat", FullScan);
        has(&r, "s", FullScan);
        has(&r, "o2", Lookup);
    }
    if let Some(r) = classes("mariadb-11.8", "analyze-json") {
        has(&r, "<union1,5>", InternalResult);
        has(&r, "<derived3>", Lookup);
        has(&r, "cat2", FullScan);
        has(&r, "o", UniqueLookup);
    }
    if let Some(r) = classes("postgres-17", "analyze-buffers-json") {
        assert!(r.iter().any(|(o, a)| o.starts_with("*SELECT* 1") && *a == InternalResult), "{r:?}");
        has(&r, "orders o", FullScan);
        assert!(r.iter().any(|(_, a)| *a == Lookup), "{r:?}");
    }
    if let Some(g) = golden::one("sqlite-bundled", "modern", "query-plan") {
        let r = reads(&plan::parse(&g.server, g.result.as_ref().unwrap()).unwrap().unwrap());
        has(&r, "t", InternalResult);
        has(&r, "(subquery-7)", InternalResult);
        has(&r, "chain", InternalResult);
        has(&r, "employees", FullScan);
        has(&r, "o", UniqueLookup);
        has(&r, "e", Lookup);
    }
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

fn is_postgres_json(g: &Golden) -> bool {
    g.server.flavor == Flavor::Postgres && g.outcome == "ok" && g.variant.ends_with("json")
}

type JsonObject = serde_json::Map<String, serde_json::Value>;

/// `(depth, node)` for each object under `Plan`, parents before children, read from the JSON.
fn json_nodes(text: &str) -> Vec<(usize, JsonObject)> {
    fn walk(node: &JsonObject, depth: usize, out: &mut Vec<(usize, JsonObject)>) {
        out.push((depth, node.clone()));
        for child in node.get("Plans").and_then(|p| p.as_array()).into_iter().flatten() {
            walk(child.as_object().unwrap(), depth + 1, out);
        }
    }
    let statements: Vec<JsonObject> = serde_json::from_str(text).unwrap();
    assert_eq!(statements.len(), 1, "one statement per golden");
    let mut out = Vec::new();
    walk(statements[0]["Plan"].as_object().unwrap(), 0, &mut out);
    out
}

#[test]
fn postgres_json_goldens_keep_their_plan_structure() {
    let Some(goldens) = golden::load() else { return };
    let json: Vec<&Golden> = goldens.iter().filter(|g| is_postgres_json(g)).collect();
    assert!(json.len() >= 50, "expected the JSON goldens of PostgreSQL 12 to 18, found {}", json.len());
    for g in json {
        let text = g.text().unwrap_or_else(|| panic!("{}: one text cell", g.name));
        assert_eq!(g.result.as_ref().unwrap().columns[0].type_name, "json", "{}", g.name);
        let plan = plan::parse(&g.server, g.result.as_ref().unwrap())
            .unwrap_or_else(|| panic!("{}: JSON was not recognized", g.name))
            .unwrap_or_else(|e| panic!("{}: {e}", g.name));
        assert_eq!(plan.format, PlanFormat::PostgresJson, "{}", g.name);
        assert_eq!(plan.raw, text, "{}: raw text is the server's output", g.name);
        let analyzed = g.variant.starts_with("analyze");
        assert_eq!(plan.analyzed(), analyzed, "{}", g.name);
        assert_eq!(plan.node_count(), text.matches("\"Node Type\"").count(), "{}: one node per Node Type", g.name);
        let expected = json_nodes(&text);
        let order = tree_order(&plan);
        assert_eq!(order.len(), expected.len(), "{}", g.name);
        for (id, (depth, json)) in order.iter().zip(&expected) {
            let node = plan.node(*id);
            let str_of = |key: &str| json.get(key).and_then(|v| v.as_str());
            let at = format!("{}: {} at depth {depth}", g.name, str_of("Node Type").unwrap());
            assert_eq!(plan.depth(*id), *depth, "depth of {at}");
            let base =
                if str_of("Node Type") == Some("ModifyTable") { str_of("Operation") } else { str_of("Node Type") };
            for word in base.unwrap().split(' ') {
                assert!(node.operation.contains(word), "operation {:?} of {at}", node.operation);
            }
            if let Some(join) = str_of("Join Type").filter(|j| *j != "Inner") {
                assert!(node.operation.contains(join), "join type in {:?} of {at}", node.operation);
            }
            assert_eq!(node.index.as_deref(), str_of("Index Name"), "index of {at}");
            if let Some(relation) = str_of("Relation Name").or(str_of("CTE Name")) {
                assert!(node.object.as_deref().is_some_and(|o| o.starts_with(relation)), "object of {at}");
            }
            let subplan = matches!(str_of("Parent Relationship"), Some("SubPlan" | "InitPlan"));
            assert_eq!(node.role.as_deref(), str_of("Subplan Name").filter(|_| subplan), "role of {at}");
            assert_eq!(node.estimate.is_some(), json.contains_key("Plan Rows"), "estimate of {at}");
            assert!(node.estimate.unwrap().cost.is_some(), "cost of {at}");
            assert_eq!(node.actual.is_some(), analyzed, "actuals of {at}");
            if let Some(actual) = node.actual {
                assert_eq!(actual.time.is_some(), actual.loops > 0, "time of {at}");
            }
            for key in json.keys().filter(|k| *k != "Plans") {
                assert!(node.attributes.iter().any(|a| a.key.starts_with(key.as_str())), "attribute {key} of {at}");
            }
            if let (Some(total), Some(own)) = (plan.inclusive_ms(*id), plan.self_ms(*id)) {
                assert!((0.0..=total + 1e-9).contains(&own), "self time within inclusive time for {at}");
            }
        }
        let plan_keys: Vec<&str> = plan.attributes.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(plan_keys.contains(&"Execution Time"), analyzed, "{}", g.name);
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

#[test]
fn postgres_17_portable_analyze_reads_like_the_text() {
    let Some(g) = golden::one("postgres-17", "portable", "analyze-buffers-json") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    let root = plan.node(NodeId::ROOT);
    assert_eq!(root.operation, "Limit");
    assert!(plan.total_ms().is_some_and(|ms| ms > 0.0));
    assert!(root.attributes.iter().any(|a| a.key == "Shared Hit Blocks"), "BUFFERS counts stay as attributes");
    for key in ["Planning Time", "Execution Time", "Triggers"] {
        assert!(plan.attributes.iter().any(|a| a.key == key), "plan attribute {key}");
    }

    let ops: Vec<&str> = plan.nodes().map(|(_, n)| n.operation.as_str()).collect();
    for op in ["GroupAggregate", "HashAggregate", "Hash Semi Join", "Hash Right Anti Join", "Bitmap Heap Scan"] {
        assert!(ops.contains(&op), "{op} in {ops:?}");
    }

    let lookup = plan
        .nodes()
        .map(|(_, n)| n)
        .find(|n| n.operation == "Index Scan" && n.object.as_deref() == Some("order_details od2"))
        .expect("the od2 lookup");
    assert_eq!(lookup.index.as_deref(), Some("idx_order_details_product_id"));
    assert!(lookup.condition.is_some() && lookup.actual.is_some_and(|a| a.loops > 1));

    let (subplan, node) = plan.nodes().find(|(_, n)| n.role.is_some()).expect("the last_order subplan");
    assert_eq!((node.operation.as_str(), node.role.as_deref()), ("Aggregate", Some("SubPlan 1")));
    let parent = plan.node(plan.parent(subplan).unwrap());
    assert_eq!(parent.operation, "GroupAggregate", "the subplan hangs off the aggregate that runs it");
}

#[test]
fn postgres_12_plain_json_is_an_estimate_tree() {
    let Some(g) = golden::one("postgres-12", "portable", "json") else { return };
    let plan = plan::parse(&g.server, g.result.as_ref().unwrap()).expect("a plan").expect("parsed");
    assert!(!plan.analyzed());
    assert_eq!(plan.total_ms(), None);
    assert!(plan.attributes.is_empty(), "no planning or execution time without ANALYZE");
    assert!(plan.nodes().all(|(_, n)| n.estimate.is_some_and(|e| e.cost.is_some())));
}

#[test]
fn postgres_text_explain_stays_raw() {
    let Some(goldens) = golden::load() else { return };
    let text: Vec<&Golden> = goldens
        .iter()
        .filter(|g| g.server.flavor == Flavor::Postgres && g.outcome == "ok" && !g.variant.ends_with("json"))
        .collect();
    assert!(!text.is_empty());
    for g in text {
        assert!(plan::parse(&g.server, g.result.as_ref().unwrap()).is_none(), "{}", g.name);
    }
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
