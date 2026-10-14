//! SQL generation for browsing tables (filters, sorting, paging) and for applying staged
//! row edits.

use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;
use crate::model::ColumnInfo;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilterOp {
    Equal,
    NotEqual,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    In,
    NotIn,
    IsNull,
    IsNotNull,
    Between,
    /// Free-form SQL condition; the column is ignored.
    Raw,
}

impl FilterOp {
    pub const ALL: [FilterOp; 16] = [
        FilterOp::Equal,
        FilterOp::NotEqual,
        FilterOp::Less,
        FilterOp::Greater,
        FilterOp::LessOrEqual,
        FilterOp::GreaterOrEqual,
        FilterOp::Contains,
        FilterOp::NotContains,
        FilterOp::StartsWith,
        FilterOp::EndsWith,
        FilterOp::In,
        FilterOp::NotIn,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
        FilterOp::Between,
        FilterOp::Raw,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Equal => "=",
            FilterOp::NotEqual => "<>",
            FilterOp::Less => "<",
            FilterOp::Greater => ">",
            FilterOp::LessOrEqual => "<=",
            FilterOp::GreaterOrEqual => ">=",
            FilterOp::Contains => "Contains",
            FilterOp::NotContains => "Not contains",
            FilterOp::StartsWith => "Begins with",
            FilterOp::EndsWith => "Ends with",
            FilterOp::In => "IN",
            FilterOp::NotIn => "NOT IN",
            FilterOp::IsNull => "IS NULL",
            FilterOp::IsNotNull => "IS NOT NULL",
            FilterOp::Between => "BETWEEN",
            FilterOp::Raw => "Raw SQL",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }

    pub fn needs_second_value(self) -> bool {
        self == FilterOp::Between
    }
}

/// One row of the filter bar above the data grid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub enabled: bool,
    /// `None` searches every column.
    pub column: Option<String>,
    pub op: FilterOp,
    pub value: String,
    pub value2: String,
}

impl Default for Filter {
    fn default() -> Self {
        Self { enabled: true, column: None, op: FilterOp::Contains, value: String::new(), value2: String::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

fn text_cast(dialect: Dialect, expr: &str) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!("CAST({expr} AS TEXT)"),
        Dialect::MySql => format!("CAST({expr} AS CHAR)"),
    }
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn like_expr(dialect: Dialect, column_expr: &str, pattern: &str, negate: bool) -> String {
    let not = if negate { "NOT " } else { "" };
    let pattern = dialect.quote_str(pattern);
    match dialect {
        Dialect::Postgres => format!("{} {not}ILIKE {pattern}", text_cast(dialect, column_expr)),
        Dialect::MySql => format!("{} {not}LIKE {pattern}", text_cast(dialect, column_expr)),
        Dialect::Sqlite => format!("{} {not}LIKE {pattern} ESCAPE '\\'", text_cast(dialect, column_expr)),
    }
}

fn split_list(s: &str) -> Vec<&str> {
    s.split(',').map(str::trim).filter(|v| !v.is_empty()).collect()
}

/// SQL condition for one filter applied to one column expression.
fn column_condition(dialect: Dialect, col: &str, f: &Filter) -> Option<String> {
    let v = || dialect.quote_str(&f.value);
    Some(match f.op {
        FilterOp::Equal => format!("{col} = {}", v()),
        FilterOp::NotEqual => format!("{col} <> {}", v()),
        FilterOp::Less => format!("{col} < {}", v()),
        FilterOp::Greater => format!("{col} > {}", v()),
        FilterOp::LessOrEqual => format!("{col} <= {}", v()),
        FilterOp::GreaterOrEqual => format!("{col} >= {}", v()),
        FilterOp::Contains => like_expr(dialect, col, &format!("%{}%", like_escape(&f.value)), false),
        FilterOp::NotContains => like_expr(dialect, col, &format!("%{}%", like_escape(&f.value)), true),
        FilterOp::StartsWith => like_expr(dialect, col, &format!("{}%", like_escape(&f.value)), false),
        FilterOp::EndsWith => like_expr(dialect, col, &format!("%{}", like_escape(&f.value)), false),
        FilterOp::In | FilterOp::NotIn => {
            let items = split_list(&f.value);
            if items.is_empty() {
                return None;
            }
            let list = items.iter().map(|i| dialect.quote_str(i)).collect::<Vec<_>>().join(", ");
            let not = if f.op == FilterOp::NotIn { "NOT " } else { "" };
            format!("{col} {not}IN ({list})")
        }
        FilterOp::IsNull => format!("{col} IS NULL"),
        FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
        FilterOp::Between => {
            format!("{col} BETWEEN {} AND {}", v(), dialect.quote_str(&f.value2))
        }
        FilterOp::Raw => return None,
    })
}

/// SQL condition for one filter, or `None` when the filter is incomplete/disabled.
pub fn filter_condition(dialect: Dialect, f: &Filter, all_columns: &[String]) -> Option<String> {
    if !f.enabled {
        return None;
    }
    if f.op == FilterOp::Raw {
        let raw = f.value.trim();
        return (!raw.is_empty()).then(|| format!("({raw})"));
    }
    if f.op.needs_value() && f.value.is_empty() {
        return None;
    }
    match &f.column {
        Some(c) => column_condition(dialect, &dialect.quote_ident(c), f),
        None => {
            // "Any column": text-match against every column.
            let parts: Vec<String> = all_columns
                .iter()
                .filter_map(|c| {
                    let expr = dialect.quote_ident(c);
                    match f.op {
                        FilterOp::Equal
                        | FilterOp::NotEqual
                        | FilterOp::Less
                        | FilterOp::Greater
                        | FilterOp::LessOrEqual
                        | FilterOp::GreaterOrEqual
                        | FilterOp::In
                        | FilterOp::NotIn
                        | FilterOp::Between => column_condition(dialect, &text_cast(dialect, &expr), f),
                        _ => column_condition(dialect, &expr, f),
                    }
                })
                .collect();
            if parts.is_empty() {
                return None;
            }
            let joiner =
                if matches!(f.op, FilterOp::NotContains | FilterOp::NotEqual | FilterOp::NotIn | FilterOp::IsNotNull) {
                    " AND "
                } else {
                    " OR "
                };
            Some(format!("({})", parts.join(joiner)))
        }
    }
}

/// Combined `WHERE` body (without the keyword) for all enabled filters, ANDed.
pub fn where_clause(dialect: Dialect, filters: &[Filter], all_columns: &[String]) -> Option<String> {
    let parts: Vec<String> = filters.iter().filter_map(|f| filter_condition(dialect, f, all_columns)).collect();
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

/// `sort` plus any key columns not already in it, giving a total order for stable paging.
pub fn with_tiebreaker(sort: &[SortKey], key_columns: &[String]) -> Vec<SortKey> {
    let mut out = sort.to_vec();
    for k in key_columns {
        if !out.iter().any(|s| &s.column == k) {
            out.push(SortKey { column: k.clone(), descending: false });
        }
    }
    out
}

pub fn order_by_clause(dialect: Dialect, sort: &[SortKey]) -> Option<String> {
    if sort.is_empty() {
        return None;
    }
    Some(
        sort.iter()
            .map(|s| format!("{} {}", dialect.quote_ident(&s.column), if s.descending { "DESC" } else { "ASC" }))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// `SELECT * FROM t [WHERE …] [ORDER BY …] LIMIT n OFFSET m`
pub fn select_page_sql(
    dialect: Dialect,
    schema: &str,
    table: &str,
    where_body: Option<&str>,
    sort: &[SortKey],
    limit: Option<u64>,
    offset: u64,
) -> String {
    let mut sql = format!("SELECT * FROM {}", dialect.qualified(schema, table));
    if let Some(w) = where_body {
        sql.push_str(" WHERE ");
        sql.push_str(w);
    }
    if let Some(o) = order_by_clause(dialect, sort) {
        sql.push_str(" ORDER BY ");
        sql.push_str(&o);
    }
    match limit {
        Some(l) => sql.push_str(&format!(" LIMIT {l} OFFSET {offset}")),
        None if offset > 0 => match dialect {
            Dialect::MySql => sql.push_str(&format!(" LIMIT 18446744073709551615 OFFSET {offset}")),
            Dialect::Sqlite => sql.push_str(&format!(" LIMIT -1 OFFSET {offset}")),
            Dialect::Postgres => sql.push_str(&format!(" OFFSET {offset}")),
        },
        None => {}
    }
    sql
}

pub fn count_sql(dialect: Dialect, schema: &str, table: &str, where_body: Option<&str>) -> String {
    let mut sql = format!("SELECT COUNT(*) FROM {}", dialect.qualified(schema, table));
    if let Some(w) = where_body {
        sql.push_str(" WHERE ");
        sql.push_str(w);
    }
    sql
}

/// A pending change to one cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellEdit {
    Text(String),
    Null,
    Default,
}

impl CellEdit {
    fn to_sql(&self, dialect: Dialect, column_type: &str) -> String {
        match self {
            CellEdit::Text(t) => dialect.edit_literal(column_type, t),
            CellEdit::Null => "NULL".into(),
            CellEdit::Default => "DEFAULT".into(),
        }
    }
}

/// How a row in the grid maps back to exactly one row in the table.
#[derive(Debug, Clone)]
pub struct RowIdentity {
    /// `(column, original value)`; primary-key columns when available, otherwise every column.
    pub columns: Vec<(String, Value)>,
    pub by_primary_key: bool,
}

impl RowIdentity {
    /// Builds the identity of `row` (values aligned with `columns`).
    pub fn new(columns: &[ColumnInfo], row: &[Value]) -> Self {
        let pk: Vec<(String, Value)> =
            columns.iter().zip(row).filter(|(c, _)| c.primary_key).map(|(c, v)| (c.name.clone(), v.clone())).collect();
        if pk.is_empty() {
            let all = columns.iter().zip(row).map(|(c, v)| (c.name.clone(), v.clone())).collect();
            RowIdentity { columns: all, by_primary_key: false }
        } else {
            RowIdentity { columns: pk, by_primary_key: true }
        }
    }

    fn predicate(&self, dialect: Dialect) -> String {
        self.columns
            .iter()
            .map(|(name, value)| {
                let col = dialect.quote_ident(name);
                match value {
                    Value::Null => format!("{col} IS NULL"),
                    Value::Int(_) | Value::UInt(_) | Value::Bool(_) | Value::Decimal(_) => {
                        format!("{col} = {}", dialect.literal(value))
                    }
                    Value::Text(t) if !self.by_primary_key && dialect == Dialect::Postgres => {
                        // Compare through the text rendering so types without an equality
                        // operator (json, point…) still match exactly what was displayed.
                        format!("{col}::text = {}", dialect.quote_str(t))
                    }
                    Value::Float(f) if !self.by_primary_key && dialect == Dialect::Postgres => {
                        format!("{col}::text = {}", dialect.quote_str(&f.to_string()))
                    }
                    other => format!("{col} = {}", dialect.literal(other)),
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// `WHERE` body selecting exactly this row (limited to one row when matching by value).
    fn where_one(&self, dialect: Dialect, table: &str) -> (String, &'static str) {
        let pred = self.predicate(dialect);
        if self.by_primary_key {
            return (pred, "");
        }
        match dialect {
            Dialect::Postgres => (format!("ctid = (SELECT ctid FROM {table} WHERE {pred} LIMIT 1)"), ""),
            Dialect::Sqlite => (format!("rowid = (SELECT rowid FROM {table} WHERE {pred} LIMIT 1)"), ""),
            Dialect::MySql => (pred, " LIMIT 1"),
        }
    }
}

fn type_of<'a>(columns: &'a [ColumnInfo], name: &str) -> &'a str {
    columns.iter().find(|c| c.name == name).map(|c| c.data_type.as_str()).unwrap_or("")
}

pub fn update_sql(
    dialect: Dialect,
    schema: &str,
    table: &str,
    columns: &[ColumnInfo],
    identity: &RowIdentity,
    changes: &[(String, CellEdit)],
) -> String {
    let t = dialect.qualified(schema, table);
    let sets = changes
        .iter()
        .map(|(c, e)| format!("{} = {}", dialect.quote_ident(c), e.to_sql(dialect, type_of(columns, c))))
        .collect::<Vec<_>>()
        .join(", ");
    let (w, suffix) = identity.where_one(dialect, &t);
    format!("UPDATE {t} SET {sets} WHERE {w}{suffix}")
}

pub fn delete_sql(dialect: Dialect, schema: &str, table: &str, identity: &RowIdentity) -> String {
    let t = dialect.qualified(schema, table);
    let (w, suffix) = identity.where_one(dialect, &t);
    format!("DELETE FROM {t} WHERE {w}{suffix}")
}

pub fn insert_sql(
    dialect: Dialect,
    schema: &str,
    table: &str,
    columns: &[ColumnInfo],
    values: &[(String, CellEdit)],
) -> String {
    let t = dialect.qualified(schema, table);
    let values: Vec<_> = values.iter().filter(|(_, e)| *e != CellEdit::Default).collect();
    if values.is_empty() {
        return match dialect {
            Dialect::MySql => format!("INSERT INTO {t} () VALUES ()"),
            _ => format!("INSERT INTO {t} DEFAULT VALUES"),
        };
    }
    let cols = values.iter().map(|(c, _)| dialect.quote_ident(c)).collect::<Vec<_>>().join(", ");
    let vals = values.iter().map(|(c, e)| e.to_sql(dialect, type_of(columns, c))).collect::<Vec<_>>().join(", ");
    format!("INSERT INTO {t} ({cols}) VALUES ({vals})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), primary_key: true, ..Default::default() },
            ColumnInfo { name: "name".into(), data_type: "text".into(), nullable: true, ..Default::default() },
            ColumnInfo { name: "data".into(), data_type: "bytea".into(), nullable: true, ..Default::default() },
        ]
    }

    #[test]
    fn filters() {
        let all = vec!["id".to_string(), "name".to_string()];
        let f =
            Filter { column: Some("name".into()), op: FilterOp::Contains, value: "50%".into(), ..Default::default() };
        assert_eq!(filter_condition(Dialect::Postgres, &f, &all).unwrap(), "CAST(\"name\" AS TEXT) ILIKE '%50\\%%'");
        assert_eq!(filter_condition(Dialect::MySql, &f, &all).unwrap(), "CAST(`name` AS CHAR) LIKE '%50\\\\%%'");
        assert_eq!(
            filter_condition(Dialect::Sqlite, &f, &all).unwrap(),
            "CAST(\"name\" AS TEXT) LIKE '%50\\%%' ESCAPE '\\'"
        );
        let any = Filter { column: None, op: FilterOp::Contains, value: "ada".into(), ..Default::default() };
        assert_eq!(
            filter_condition(Dialect::Sqlite, &any, &all).unwrap(),
            "(CAST(\"id\" AS TEXT) LIKE '%ada%' ESCAPE '\\' OR CAST(\"name\" AS TEXT) LIKE '%ada%' ESCAPE '\\')"
        );
        let inl = Filter { column: Some("id".into()), op: FilterOp::In, value: "1, 2,3".into(), ..Default::default() };
        assert_eq!(filter_condition(Dialect::Postgres, &inl, &all).unwrap(), "\"id\" IN ('1', '2', '3')");
        let null = Filter { column: Some("name".into()), op: FilterOp::IsNull, ..Default::default() };
        assert_eq!(filter_condition(Dialect::MySql, &null, &all).unwrap(), "`name` IS NULL");
        let empty = Filter { column: Some("name".into()), op: FilterOp::Contains, ..Default::default() };
        assert_eq!(filter_condition(Dialect::MySql, &empty, &all), None);
        let disabled = Filter { enabled: false, ..null.clone() };
        assert_eq!(where_clause(Dialect::MySql, &[disabled, null], &all).unwrap(), "`name` IS NULL");
        let raw = Filter { op: FilterOp::Raw, value: "id > 3 OR id < 1".into(), ..Default::default() };
        assert_eq!(filter_condition(Dialect::MySql, &raw, &all).unwrap(), "(id > 3 OR id < 1)");
    }

    #[test]
    fn tiebreaker() {
        let sort = vec![SortKey { column: "score".into(), descending: true }];
        let keys = vec!["id".to_string()];
        let s = with_tiebreaker(&sort, &keys);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1], SortKey { column: "id".into(), descending: false });
        assert_eq!(with_tiebreaker(&s, &keys), s);
        assert_eq!(with_tiebreaker(&[], &keys).len(), 1);
    }

    #[test]
    fn paging() {
        let sort = vec![SortKey { column: "id".into(), descending: true }];
        assert_eq!(
            select_page_sql(Dialect::Postgres, "public", "users", Some("\"id\" > '3'"), &sort, Some(100), 200),
            "SELECT * FROM \"public\".\"users\" WHERE \"id\" > '3' ORDER BY \"id\" DESC LIMIT 100 OFFSET 200"
        );
        assert_eq!(select_page_sql(Dialect::Sqlite, "main", "t", None, &[], None, 0), "SELECT * FROM \"t\"");
        assert_eq!(count_sql(Dialect::MySql, "shop", "t", None), "SELECT COUNT(*) FROM `shop`.`t`");
    }

    #[test]
    fn dml() {
        let cols = cols();
        let row = vec![Value::Int(7), Value::text("x"), Value::Null];
        let id = RowIdentity::new(&cols, &row);
        assert!(id.by_primary_key);
        let sql = update_sql(
            Dialect::Postgres,
            "public",
            "t",
            &cols,
            &id,
            &[("name".into(), CellEdit::Text("O'Hara".into())), ("data".into(), CellEdit::Text("0xBEEF".into()))],
        );
        assert_eq!(
            sql,
            "UPDATE \"public\".\"t\" SET \"name\" = 'O''Hara', \"data\" = '\\xbeef'::bytea WHERE \"id\" = 7"
        );
        assert_eq!(delete_sql(Dialect::MySql, "shop", "t", &id), "DELETE FROM `shop`.`t` WHERE `id` = 7");
        assert_eq!(
            insert_sql(
                Dialect::Sqlite,
                "main",
                "t",
                &cols,
                &[("name".into(), CellEdit::Null), ("id".into(), CellEdit::Default)]
            ),
            "INSERT INTO \"t\" (\"name\") VALUES (NULL)"
        );
        assert_eq!(insert_sql(Dialect::MySql, "s", "t", &cols, &[]), "INSERT INTO `s`.`t` () VALUES ()");
    }

    #[test]
    fn dml_without_primary_key() {
        let cols: Vec<ColumnInfo> = cols().into_iter().map(|c| ColumnInfo { primary_key: false, ..c }).collect();
        let row = vec![Value::Int(7), Value::Null, Value::text("{}")];
        let id = RowIdentity::new(&cols, &row);
        assert!(!id.by_primary_key);
        assert_eq!(
            delete_sql(Dialect::Postgres, "public", "t", &id),
            "DELETE FROM \"public\".\"t\" WHERE ctid = (SELECT ctid FROM \"public\".\"t\" WHERE \"id\" = 7 AND \"name\" IS NULL AND \"data\"::text = '{}' LIMIT 1)"
        );
        assert_eq!(
            delete_sql(Dialect::MySql, "s", "t", &id),
            "DELETE FROM `s`.`t` WHERE `id` = 7 AND `name` IS NULL AND `data` = '{}' LIMIT 1"
        );
        assert_eq!(
            delete_sql(Dialect::Sqlite, "main", "t", &id),
            "DELETE FROM \"t\" WHERE rowid = (SELECT rowid FROM \"t\" WHERE \"id\" = 7 AND \"name\" IS NULL AND \"data\" = '{}' LIMIT 1)"
        );
    }
}
