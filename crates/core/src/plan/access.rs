//! How a step reaches a table's rows, and which steps are joins.

use super::PlanNode;

/// How a step reads a table, from the costliest to the cheapest way. These are MySQL's access
/// types (`ALL`, `index`, `range`, `ref`, `eq_ref`, `const`), which MySQL Workbench's Visual
/// Explain colours its boxes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Reads every row of the table (`ALL`).
    FullScan,
    /// Reads every entry of an index (`index`).
    IndexScan,
    /// Reads the part of an index between bounds (`range`).
    Range,
    /// Looks up the rows matching a key value, possibly several (`ref`).
    Lookup,
    /// Looks up at most one row through a unique key (`eq_ref`).
    UniqueLookup,
    /// Reads a single row known before the statement runs (`const`, `system`).
    Constant,
}

impl Access {
    /// MySQL Workbench's wording.
    pub fn label(self) -> &'static str {
        match self {
            Access::FullScan => "Full table scan",
            Access::IndexScan => "Full index scan",
            Access::Range => "Index range scan",
            Access::Lookup => "Non-unique key lookup",
            Access::UniqueLookup => "Unique key lookup",
            Access::Constant => "Constant row",
        }
    }

    /// MySQL's `access_type` as EXPLAIN FORMAT=JSON prints it.
    fn from_mysql_type(access_type: &str) -> Option<Self> {
        Some(match access_type {
            "ALL" => Access::FullScan,
            "index" => Access::IndexScan,
            "range" | "index_merge" => Access::Range,
            "ref" | "ref_or_null" | "fulltext" | "index_subquery" => Access::Lookup,
            "eq_ref" | "unique_subquery" => Access::UniqueLookup,
            "const" | "system" => Access::Constant,
            _ => return None,
        })
    }
}

/// MySQL TREE iterator names and their access type.
const MYSQL_TREE: &[(&str, Access)] = &[
    ("Table scan", Access::FullScan),
    ("Sample scan", Access::FullScan),
    ("Scan new records", Access::FullScan),
    ("Index scan", Access::IndexScan),
    ("Covering index scan", Access::IndexScan),
    ("Index range scan", Access::Range),
    ("Covering index range scan", Access::Range),
    ("Index skip scan", Access::Range),
    ("Covering index skip scan", Access::Range),
    ("Index skip scan for grouping", Access::Range),
    ("Covering index skip scan for grouping", Access::Range),
    ("Index skip scan for deduplication", Access::Range),
    ("Covering index skip scan for deduplication", Access::Range),
    ("Multi-range index lookup", Access::Range),
    ("Index lookup", Access::Lookup),
    ("Covering index lookup", Access::Lookup),
    ("Indexed full text search", Access::Lookup),
    ("Single-row index lookup", Access::UniqueLookup),
    ("Single-row covering index lookup", Access::UniqueLookup),
    ("Rows fetched before execution", Access::Constant),
];

impl PlanNode {
    /// How this step reads its table; `None` for steps that work on other steps' rows, such as
    /// joins, filters and sorts.
    pub fn access(&self) -> Option<Access> {
        // MySQL prints `Constant row from <table>` with no `on`, so the table stays in the name.
        if self.operation.starts_with("Constant row from") {
            return Some(Access::Constant);
        }
        if let Some((_, access)) = MYSQL_TREE.iter().find(|(op, _)| *op == self.operation) {
            return Some(*access);
        }
        // Checked after the name: JSON format version 2 prints a generic `index` for every index
        // access, which would read as a full index scan in version 1's vocabulary.
        let access_type = self
            .access_type
            .as_deref()
            .or_else(|| self.attributes.iter().find(|a| a.key == "access_type").map(|a| a.value.as_str()));
        access_type.and_then(Access::from_mysql_type)
    }

    /// True for a step that combines the rows of its inputs: a nested loop, hash or merge join,
    /// including semi- and antijoins.
    pub fn is_join(&self) -> bool {
        let op = self.operation.to_ascii_lowercase();
        op.contains("join") || op.starts_with("nested loop")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Attribute, NodeId, Plan};
    use crate::server::{Flavor, ServerInfo, Version};

    /// Every step of a plan as `(operation, access, is_join)`, through the real TREE parser.
    fn steps(tree: &str) -> Vec<(String, Option<Access>, bool)> {
        use crate::model::{ColumnMeta, ResultSet};
        use crate::value::Value;
        let result = ResultSet {
            columns: vec![ColumnMeta::new("EXPLAIN", "VARCHAR")],
            rows: vec![vec![Value::text(tree)].into()],
            truncated: false,
        };
        let server = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        let plan: Plan = crate::plan::parse(&server, &result).unwrap().unwrap();
        plan.nodes().map(|(_, n)| (n.operation.clone(), n.access(), n.is_join())).collect()
    }

    #[test]
    fn mysql_tree_steps_map_to_workbench_access_types() {
        let tree = "\
-> Nested loop inner join  (cost=1150 rows=830)
    -> Inner hash join (c.customer_id = o.customer_id)  (cost=594 rows=1.95)
        -> Table scan on c  (cost=0.525 rows=91)
        -> Hash
            -> Covering index scan on sh using PRIMARY  (cost=0.55 rows=3)
    -> Index range scan on o using idx_date over ('1997-01-01' <= order_date)  (cost=60 rows=270)
    -> Index lookup on t using <auto_key0> (order_id=o.order_id)  (cost=0.25 rows=2)
    -> Covering index lookup on od2 using idx_product (product_id=p2.product_id)  (cost=0.5 rows=28)
    -> Single-row index lookup on e using PRIMARY (employee_id=o.employee_id)  (cost=0.25 rows=1)
    -> Single-row covering index lookup on m using PRIMARY (employee_id=e.reports_to)  (cost=0.25 rows=1)
    -> Constant row from shippers
    -> Rows fetched before execution
    -> Index scan on od using PRIMARY (reverse)  (cost=217 rows=2155)
    -> Covering index skip scan for grouping on p using idx_supplier  (cost=3 rows=29)
    -> Scan new records on chain  (cost=2.73 rows=2)
    -> Filter: (r.order_date >= DATE'1997-01-01')
    -> Sort: c.customer_id
    -> Nested loop antijoin
    -> Hash semijoin (a.x = b.x)
";
        use Access::*;
        let expected: &[(&str, Option<Access>, bool)] = &[
            ("Nested loop inner join", None, true),
            ("Inner hash join", None, true),
            ("Table scan", Some(FullScan), false),
            ("Hash", None, false),
            ("Covering index scan", Some(IndexScan), false),
            ("Index range scan", Some(Range), false),
            ("Index lookup", Some(Lookup), false),
            ("Covering index lookup", Some(Lookup), false),
            ("Single-row index lookup", Some(UniqueLookup), false),
            ("Single-row covering index lookup", Some(UniqueLookup), false),
            ("Constant row from shippers", Some(Constant), false),
            ("Rows fetched before execution", Some(Constant), false),
            ("Index scan", Some(IndexScan), false),
            ("Covering index skip scan for grouping", Some(Range), false),
            ("Scan new records", Some(FullScan), false),
            ("Filter", None, false),
            ("Sort", None, false),
            ("Nested loop antijoin", None, true),
            ("Hash semijoin", None, true),
        ];
        let got = steps(tree);
        let got: Vec<(&str, Option<Access>, bool)> = got.iter().map(|(o, a, j)| (o.as_str(), *a, *j)).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn json_access_type_classifies_steps_without_a_tree_name() {
        let with_type = |t: &str| {
            let mut n = PlanNode::new("Full table scan");
            n.attributes.push(Attribute::new("access_type", t));
            n.access()
        };
        assert_eq!(with_type("eq_ref"), Some(Access::UniqueLookup));
        assert_eq!(with_type("ALL"), Some(Access::FullScan));
        assert_eq!(with_type("index"), Some(Access::IndexScan));
        assert_eq!(with_type("system"), Some(Access::Constant));
        assert_eq!(with_type("something new"), None);
    }

    #[test]
    fn a_tree_name_wins_over_version_2s_generic_index() {
        let mut lookup = PlanNode::new("Single-row index lookup");
        lookup.access_type = Some("index".into());
        assert_eq!(lookup.access(), Some(Access::UniqueLookup));
        let mut v1 = PlanNode::new("Full table scan");
        v1.access_type = Some("ALL".into());
        assert_eq!(v1.access(), Some(Access::FullScan));
    }

    #[test]
    fn joins_are_never_table_access() {
        for op in ["Nested Loop", "Hash Join", "Merge Join", "Left hash join", "Nested loop semijoin"] {
            let node = PlanNode::new(op);
            assert!(node.is_join(), "{op}");
            assert_eq!(node.access(), None, "{op}");
        }
        let plan = Plan::new(crate::plan::PlanFormat::MySqlTree, PlanNode::new("Limit"), "");
        assert!(!plan.node(NodeId::ROOT).is_join());
    }
}
