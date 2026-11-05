//! DuckDB's `EXPLAIN (FORMAT JSON)` and `EXPLAIN (ANALYZE, FORMAT JSON)`: two columns,
//! `explain_key` and `explain_value`. An estimate's value is an array of root operators; a
//! profile's is an object with the statement's metrics and the operator tree below it.
//!
//! ```json
//! [{"name": "HASH_JOIN", "extra_info": {"Conditions": "user_id = id", "Estimated Cardinality": "418"},
//!   "children": [ … ]}]
//! {"latency": 0.0026, "cpu_time": 0.0015, "children": [{"operator_name": "EXPLAIN_ANALYZE",
//!   "children": [{"operator_name": "TOP_N", "operator_cardinality": 10, "cpu_time": 0.0014, …}]}]}
//! ```
//!
//! A profile's `cpu_time` is the operator's own time plus its children's, summed over every thread
//! that ran them, so a parallel plan's root claims more time than the statement took. Each time is
//! scaled by `latency / cpu_time` of the statement, so the root equals the measured statement
//! time and every step keeps its share, as SQLite's cycle counts are scaled to the time spent in
//! the statement.

use serde_json::{Map, Value as Json};

use super::{
    Actual, ActualTime, Attribute, Estimate, NodeId, ParseError, Plan, PlanFormat, PlanNode, strip_outer_parens,
};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};
use crate::value::Value;

type Object = Map<String, Json>;

/// JSON levels a plan may nest; deeper input is refused, as for PostgreSQL.
const MAX_DEPTH: usize = 256;

pub(super) fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    if server.flavor != Flavor::DuckDb || result.columns.len() != 2 || result.columns[1].name != "explain_value" {
        return None;
    }
    // With `explain_output = 'all'` the logical plans come first; the physical one is the plan.
    fn cell(row: &[Value], i: usize) -> Option<&str> {
        match &row[i] {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }
    let (key, text) = result.rows.iter().find_map(|r| {
        let key = cell(r, 0)?;
        matches!(key, "physical_plan" | "analyzed_plan").then(|| (key, cell(r, 1)))
    })?;
    let text = text?;
    let json = text.trim_start();
    let readable =
        (key == "physical_plan" && json.starts_with('[')) || (key == "analyzed_plan" && json.starts_with('{'));
    readable.then(|| parse_text(text).map_err(|message| ParseError { format: PlanFormat::DuckDbJson, message }))
}

fn parse_text(text: &str) -> Result<Plan, String> {
    if super::postgres_json::json_depth(text) > MAX_DEPTH {
        return Err(format!("the plan nests deeper than {MAX_DEPTH} levels"));
    }
    let mut de = serde_json::Deserializer::from_str(text);
    de.disable_recursion_limit();
    let json: Json = serde::Deserialize::deserialize(&mut de).map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    match json {
        Json::Array(roots) => estimate(&roots, text),
        Json::Object(profile) => analyzed(&profile, text),
        _ => Err("the plan is neither an array nor an object".into()),
    }
}

/// An estimate-only plan: one operator tree per root, under a `Query` node when there are several.
fn estimate(roots: &[Json], text: &str) -> Result<Plan, String> {
    let objects: Vec<&Object> =
        roots.iter().map(|r| r.as_object().ok_or("a root operator is not an object")).collect::<Result<_, _>>()?;
    let scale = Scale(None);
    match objects.as_slice() {
        [] => Err("the plan holds no operator".into()),
        [one] => {
            let mut plan = Plan::new(PlanFormat::DuckDbJson, node(one, scale)?, text);
            add_children(&mut plan, NodeId::ROOT, one, scale)?;
            Ok(plan)
        }
        many => {
            let mut plan = Plan::new(PlanFormat::DuckDbJson, PlanNode::new("Query"), text);
            for root in many {
                let id = plan.add_child(NodeId::ROOT, node(root, scale)?);
                add_children(&mut plan, id, root, scale)?;
            }
            Ok(plan)
        }
    }
}

/// A profile: the statement as a `Query` root holding its metrics, with the operators below.
/// DuckDB's own `EXPLAIN_ANALYZE` operator, which reports nothing of its own, is left out.
fn analyzed(profile: &Object, text: &str) -> Result<Plan, String> {
    let seconds = |key: &str| profile.get(key).and_then(Json::as_f64);
    let latency = seconds("latency").ok_or("the profile has no \"latency\"")?;
    let thread_time = seconds("cpu_time").filter(|t| *t > 0.0);
    let scale = Scale(Some(thread_time.map_or(1.0, |t| latency / t)));

    let mut tops: Vec<&Object> = Vec::new();
    for child in children(profile)? {
        if child.get("operator_name").and_then(Json::as_str) == Some("EXPLAIN_ANALYZE") {
            tops.extend(children(child)?);
        } else {
            tops.push(child);
        }
    }
    let mut root = PlanNode::new("Query");
    root.actual = Some(Actual {
        rows: tops.first().and_then(|t| t.get("operator_cardinality")).and_then(Json::as_f64),
        loops: 1,
        time: Some(ActualTime { first_row_ms: None, last_row_ms: latency * 1000.0 }),
    });
    let mut plan = Plan::new(PlanFormat::DuckDbJson, root, text);
    for top in tops {
        let id = plan.add_child(NodeId::ROOT, node(top, scale)?);
        add_children(&mut plan, id, top, scale)?;
    }
    for (key, value) in profile.iter().filter(|(k, _)| !matches!(k.as_str(), "children" | "query_name")) {
        if let Some(value) = scalar(value) {
            plan.attributes.push(Attribute::new(key.clone(), value));
        }
    }
    Ok(plan)
}

/// What a profile's thread time is multiplied by to read as a share of the statement's time;
/// `None` for an estimate.
#[derive(Clone, Copy)]
struct Scale(Option<f64>);

fn children(object: &Object) -> Result<Vec<&Object>, String> {
    object
        .get("children")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|c| c.as_object().ok_or_else(|| "a child operator is not an object".to_string()))
        .collect()
}

fn add_children(plan: &mut Plan, parent: NodeId, object: &Object, scale: Scale) -> Result<(), String> {
    for child in children(object)? {
        let id = plan.add_child(parent, node(child, scale)?);
        add_children(plan, id, child, scale)?;
    }
    // DuckDB prints an estimate of 0 on the step above one it doesn't estimate, such as the
    // projection over an ORDER_BY; that is a placeholder, not a guess of no rows.
    let unestimated_child = plan.children(parent).first().is_some_and(|c| plan.node(*c).estimate.is_none());
    let node = &mut plan.nodes[parent.index()];
    if unestimated_child && node.estimate.is_some_and(|e| e.rows == 0.0) {
        node.estimate = None;
    }
    Ok(())
}

/// Operators that report 0 rows while rows flow through their children.
const UNCOUNTED: &[&str] = &["EXPLAIN_ANALYZE", "DELIM_SCAN", "LEFT_DELIM_JOIN", "RIGHT_DELIM_JOIN"];

const CONDITION_KEYS: &[&str] = &["Conditions", "Filters", "Expression"];

fn node(object: &Object, scale: Scale) -> Result<PlanNode, String> {
    let text = |key: &str| object.get(key).and_then(Json::as_str);
    let name = text("operator_name").or_else(|| text("name")).ok_or("an operator has no name")?;
    let mut node = PlanNode::new(name);
    let empty = Object::new();
    let info = object.get("extra_info").and_then(Json::as_object).unwrap_or(&empty);
    let info_text = |key: &str| info.get(key).and_then(scalar_or_list);
    node.object = info_text("Table").or_else(|| info_text("Function"));
    node.access_type = info_text("Type");
    let conditions: Vec<(&str, String)> = CONDITION_KEYS.iter().filter_map(|k| Some((*k, info_text(k)?))).collect();
    node.condition = match conditions.as_slice() {
        [] => None,
        [(_, only)] => Some(strip_outer_parens(only).to_string()),
        several => {
            Some(several.iter().map(|(k, v)| format!("{k}: {}", strip_outer_parens(v))).collect::<Vec<_>>().join("\n"))
        }
    };
    node.estimate =
        info_text("Estimated Cardinality").and_then(|e| e.parse().ok()).map(|rows| Estimate { rows, cost: None });
    if let Scale(Some(scale)) = scale {
        let rows = object.get("operator_cardinality").and_then(Json::as_f64).filter(|_| !UNCOUNTED.contains(&name));
        let time = object
            .get("cpu_time")
            .and_then(Json::as_f64)
            .map(|seconds| ActualTime { first_row_ms: None, last_row_ms: seconds * 1000.0 * scale });
        node.actual = Some(Actual { rows, loops: 1, time });
    }
    for (key, value) in info {
        if let Some(value) = scalar_or_list(value) {
            node.attributes.push(Attribute::new(key.clone(), value));
        }
    }
    for (key, value) in object.iter().filter(|(k, _)| !matches!(k.as_str(), "children" | "extra_info" | "name")) {
        if let Some(value) = scalar(value) {
            node.attributes.push(Attribute::new(key.clone(), value));
        }
    }
    Ok(node)
}

fn scalar(value: &Json) -> Option<String> {
    match value {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A string, number or list of them (`"Projections": ["id", "total"]`) as one line of text.
fn scalar_or_list(value: &Json) -> Option<String> {
    match value {
        Json::Array(items) => Some(items.iter().filter_map(scalar).collect::<Vec<_>>().join(", ")),
        other => scalar(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::server::Version;

    fn result(key: &str, value: &str) -> ResultSet {
        ResultSet {
            columns: vec![ColumnMeta::new("explain_key", "VARCHAR"), ColumnMeta::new("explain_value", "VARCHAR")],
            rows: vec![vec![Value::text(key), Value::text(value)].into()],
            truncated: false,
        }
    }

    fn duckdb() -> ServerInfo {
        ServerInfo::new(Flavor::DuckDb, Version::new(1, 5, 6))
    }

    #[test]
    fn profile_times_are_scaled_to_the_statement() {
        // Two threads: 4 ms of thread time for a statement that took 2 ms.
        let profile = r#"{"latency": 0.002, "cpu_time": 0.004, "rows_returned": 0, "children": [
            {"operator_name": "EXPLAIN_ANALYZE", "operator_cardinality": 0, "cpu_time": 0.004, "children": [
              {"operator_name": "PROJECTION", "operator_cardinality": 3, "cpu_time": 0.004,
               "extra_info": {"Estimated Cardinality": "0"}, "children": [
                {"operator_name": "ORDER_BY", "operator_cardinality": 3, "cpu_time": 0.003,
                 "extra_info": {"Order By": "x"}, "children": [
                  {"operator_name": "SEQ_SCAN", "operator_type": "TABLE_SCAN", "operator_cardinality": 3,
                   "cpu_time": 0.001, "extra_info": {"Table": "db.main.t", "Type": "Sequential Scan",
                   "Projections": ["x", "y"], "Filters": "x>1", "Estimated Cardinality": "30"}, "children": []}]}]}]}]}"#;
        let plan = parse(&duckdb(), &result("analyzed_plan", profile)).unwrap().unwrap();
        assert_eq!(plan.format, PlanFormat::DuckDbJson);
        let ops: Vec<&str> = plan.nodes().map(|(_, n)| n.operation.as_str()).collect();
        assert_eq!(ops, ["Query", "PROJECTION", "ORDER_BY", "SEQ_SCAN"], "EXPLAIN_ANALYZE is left out");
        assert_eq!(plan.total_ms(), Some(2.0));
        let ms = |i: usize| plan.inclusive_ms(plan.nodes().nth(i).unwrap().0).unwrap();
        assert!((ms(1) - 2.0).abs() < 1e-9 && (ms(2) - 1.5).abs() < 1e-9 && (ms(3) - 0.5).abs() < 1e-9);
        let (scan_id, scan) = plan.nodes().nth(3).unwrap();
        assert_eq!(scan.object.as_deref(), Some("db.main.t"));
        assert_eq!(scan.access_type.as_deref(), Some("Sequential Scan"));
        assert_eq!(scan.condition.as_deref(), Some("x>1"));
        assert_eq!(scan.attribute("Projections"), Some("x, y"));
        assert_eq!(plan.row_estimate_ratio(scan_id), Some(0.1));
        let projection = plan.nodes().nth(1).unwrap().0;
        assert_eq!(plan.estimated_rows(projection), None, "0 above an unestimated sort is a placeholder");
        assert_eq!(plan.node(NodeId::ROOT).actual.unwrap().rows, Some(3.0));
        assert!(plan.attributes.iter().any(|a| a.key == "latency" && a.value == "0.002"));
    }

    #[test]
    fn estimate_with_several_roots_gets_a_query_root() {
        let plan = r#"[{"name": "SEQ_SCAN", "children": [], "extra_info": {"Estimated Cardinality": "0"}},
                       {"name": "DUMMY_SCAN", "children": [], "extra_info": {}}]"#;
        let plan = parse(&duckdb(), &result("physical_plan", plan)).unwrap().unwrap();
        assert!(!plan.analyzed());
        assert_eq!(plan.node(NodeId::ROOT).operation, "Query");
        assert_eq!(plan.estimated_rows(plan.children(NodeId::ROOT)[0]), Some(0.0), "a leaf's 0 is a real estimate");
        assert!(parse(&duckdb(), &result("physical_plan", "┌──┐ box text")).is_none());
        let other = ServerInfo::new(Flavor::Postgres, Version::new(17, 0, 0));
        assert!(parse(&other, &result("physical_plan", "[]")).is_none());
    }
}
