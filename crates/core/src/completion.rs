//! Context-aware SQL autocomplete: keywords, tables and columns.

use crate::dialect::Dialect;
use crate::fuzzy::fuzzy_match;
use crate::model::{ColumnInfo, ObjectKind, SchemaObject};
use crate::sqltext::{FUNCTIONS, KEYWORDS, TokenKind, statement_at, tokenize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    /// Identifier characters immediately before the cursor.
    pub prefix: String,
    /// Byte offset where `prefix` starts (the replacement range is `replace_start..cursor`).
    pub replace_start: usize,
    /// Text before a `.` (`u` in `u.na|`), if any.
    pub qualifier: Option<String>,
    /// The previous keyword expects a relation (`FROM`, `JOIN`, `UPDATE`, `INTO`, `TABLE`).
    pub expects_table: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SuggestionKind {
    Column,
    Table,
    View,
    Function,
    Schema,
    Keyword,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub label: String,
    pub insert: String,
    pub kind: SuggestionKind,
    pub detail: String,
}

/// What the editor knows about the database. Columns are fetched lazily, so
/// `columns` may return `None` until a table's structure has been loaded.
pub trait Catalog {
    fn objects(&self) -> &[SchemaObject];
    fn columns(&self, schema: &str, table: &str) -> Option<&[ColumnInfo]>;
    fn default_schema(&self) -> &str;
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn strip_quotes(s: &str) -> String {
    let t = s.trim();
    for (o, c) in [('"', '"'), ('`', '`'), ('[', ']')] {
        if t.len() >= 2 && t.starts_with(o) && t.ends_with(c) {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

pub fn context_at(src: &str, cursor: usize, dialect: Dialect) -> CompletionContext {
    let cursor = cursor.min(src.len());
    let before = &src[..cursor];
    let prefix_len: usize = before.chars().rev().take_while(|c| ident_char(*c)).map(char::len_utf8).sum();
    let replace_start = cursor - prefix_len;
    let prefix = before[replace_start..].to_string();

    let mut qualifier = None;
    let head = &before[..replace_start];
    if let Some(stripped) = head.strip_suffix('.') {
        let q = if stripped.ends_with(['"', '`']) {
            let quote = stripped.chars().last().unwrap();
            stripped[..stripped.len() - 1].rfind(quote).map(|p| &stripped[p..])
        } else {
            let n: usize = stripped.chars().rev().take_while(|c| ident_char(*c)).map(char::len_utf8).sum();
            (n > 0).then(|| &stripped[stripped.len() - n..])
        };
        qualifier = q.map(strip_quotes);
    }

    let tokens = tokenize(head, dialect);
    let prev_kw = tokens
        .iter()
        .rev()
        .filter(|t| !t.is_trivia())
        .find(|t| !(t.kind == TokenKind::Punct && t.text(head) == "."))
        .filter(|t| t.kind == TokenKind::Keyword)
        .map(|t| t.text(head).to_ascii_uppercase());
    let expects_table = qualifier.is_none()
        && matches!(prev_kw.as_deref(), Some("FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE" | "DESCRIBE"));
    CompletionContext { prefix, replace_start, qualifier, expects_table }
}

/// Tables referenced by `FROM`/`JOIN`/`UPDATE`/`INTO` clauses, with their aliases.
pub fn referenced_tables(sql: &str, dialect: Dialect) -> Vec<TableRef> {
    let tokens: Vec<_> = tokenize(sql, dialect).into_iter().filter(|t| !t.is_trivia()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let is_rel_kw = t.kind == TokenKind::Keyword
            && matches!(t.text(sql).to_ascii_uppercase().as_str(), "FROM" | "JOIN" | "UPDATE" | "INTO");
        i += 1;
        if !is_rel_kw {
            continue;
        }
        // A comma-separated list of relations may follow FROM.
        while let Some(first) = tokens.get(i) {
            if !matches!(first.kind, TokenKind::Ident | TokenKind::QuotedIdent) {
                break;
            }
            let mut parts = vec![strip_quotes(first.text(sql))];
            i += 1;
            while tokens.get(i).is_some_and(|t| t.text(sql) == ".")
                && tokens.get(i + 1).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent))
            {
                parts.push(strip_quotes(tokens[i + 1].text(sql)));
                i += 2;
            }
            let name = parts.pop().unwrap_or_default();
            let schema = parts.pop();
            let mut alias = None;
            if tokens.get(i).is_some_and(|t| t.kind == TokenKind::Keyword && t.text(sql).eq_ignore_ascii_case("AS")) {
                i += 1;
            }
            if let Some(a) = tokens.get(i).filter(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent)) {
                alias = Some(strip_quotes(a.text(sql)));
                i += 1;
            }
            out.push(TableRef { schema, name, alias });
            if tokens.get(i).is_some_and(|t| t.text(sql) == ",") {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// Inserts an identifier, quoting it when the dialect requires.
pub fn quote_if_needed(dialect: Dialect, ident: &str) -> String {
    let simple = ident.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && ident.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !crate::sqltext::is_keyword(ident);
    let case_ok = dialect != Dialect::Postgres || !ident.chars().any(|c| c.is_uppercase());
    if simple && case_ok { ident.to_string() } else { dialect.quote_ident(ident) }
}

pub struct CompletionResult {
    pub context: CompletionContext,
    pub suggestions: Vec<Suggestion>,
    /// Tables whose columns would improve suggestions but are not loaded yet.
    pub missing_columns: Vec<(String, String)>,
}

/// Computes suggestions for the cursor position in `src`.
pub fn complete(src: &str, cursor: usize, dialect: Dialect, catalog: &dyn Catalog) -> CompletionResult {
    let ctx = context_at(src, cursor, dialect);
    let stmt = statement_at(src, cursor, dialect).map(|s| s.text(src).to_string()).unwrap_or_default();
    let refs = referenced_tables(&stmt, dialect);
    let mut candidates: Vec<Suggestion> = Vec::new();
    let mut missing = Vec::new();

    let resolve = |r: &TableRef| -> Option<&SchemaObject> {
        let objs = catalog.objects();
        objs.iter()
            .filter(|o| o.kind.is_relation())
            .find(|o| {
                o.name == r.name
                    && r.schema.as_deref().is_none_or(|s| s == o.schema)
                    && (r.schema.is_some() || o.schema == catalog.default_schema())
            })
            .or_else(|| objs.iter().filter(|o| o.kind.is_relation()).find(|o| o.name.eq_ignore_ascii_case(&r.name)))
    };
    let mut push_columns =
        |obj: &SchemaObject, candidates: &mut Vec<Suggestion>| match catalog.columns(&obj.schema, &obj.name) {
            Some(cols) => {
                for c in cols {
                    candidates.push(Suggestion {
                        label: c.name.clone(),
                        insert: quote_if_needed(dialect, &c.name),
                        kind: SuggestionKind::Column,
                        detail: format!("{} · {}", c.data_type, obj.name),
                    });
                }
            }
            None => missing.push((obj.schema.clone(), obj.name.clone())),
        };

    let relation = |o: &SchemaObject, candidates: &mut Vec<Suggestion>, qualify: bool| {
        let name = quote_if_needed(dialect, &o.name);
        let insert = if qualify && !o.schema.is_empty() && o.schema != catalog.default_schema() {
            format!("{}.{name}", quote_if_needed(dialect, &o.schema))
        } else {
            name
        };
        candidates.push(Suggestion {
            label: o.name.clone(),
            insert,
            kind: if o.kind == ObjectKind::Table { SuggestionKind::Table } else { SuggestionKind::View },
            detail: if o.schema.is_empty() {
                o.kind.label().to_string()
            } else {
                format!("{} · {}", o.kind.label(), o.schema)
            },
        });
    };

    if let Some(q) = &ctx.qualifier {
        // `alias.` / `table.` → columns; `schema.` → relations in that schema.
        let target = refs
            .iter()
            .find(|r| r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q)))
            .or_else(|| refs.iter().find(|r| r.name.eq_ignore_ascii_case(q)))
            .cloned()
            .or_else(|| Some(TableRef { schema: None, name: q.clone(), alias: None }));
        if let Some(obj) = target.as_ref().and_then(resolve) {
            push_columns(&obj.clone(), &mut candidates);
        }
        for o in catalog.objects().iter().filter(|o| o.schema.eq_ignore_ascii_case(q) && o.kind.is_relation()) {
            relation(o, &mut candidates, false);
        }
    } else if ctx.expects_table {
        for o in catalog.objects().iter().filter(|o| o.kind.is_relation()) {
            relation(o, &mut candidates, true);
        }
        let mut schemas: Vec<&str> = catalog.objects().iter().map(|o| o.schema.as_str()).collect();
        schemas.sort_unstable();
        schemas.dedup();
        for s in schemas.into_iter().filter(|s| !s.is_empty() && *s != catalog.default_schema()) {
            candidates.push(Suggestion {
                label: s.to_string(),
                insert: quote_if_needed(dialect, s),
                kind: SuggestionKind::Schema,
                detail: "Schema".into(),
            });
        }
    } else {
        for r in &refs {
            if let Some(obj) = resolve(r) {
                push_columns(&obj.clone(), &mut candidates);
            }
        }
        if !ctx.prefix.is_empty() {
            for o in catalog.objects().iter().filter(|o| o.kind.is_relation()) {
                relation(o, &mut candidates, true);
            }
            let upper = ctx.prefix.chars().any(char::is_uppercase);
            for k in KEYWORDS {
                candidates.push(Suggestion {
                    label: k.to_string(),
                    insert: if upper { k.to_string() } else { k.to_ascii_lowercase() },
                    kind: SuggestionKind::Keyword,
                    detail: "Keyword".into(),
                });
            }
            for f in FUNCTIONS {
                candidates.push(Suggestion {
                    label: format!("{f}()"),
                    insert: format!("{}(", if upper { f.to_ascii_uppercase() } else { f.to_string() }),
                    kind: SuggestionKind::Function,
                    detail: "Function".into(),
                });
            }
        }
    }

    let prefix = ctx.prefix.as_str();
    let mut scored: Vec<(i64, Suggestion)> = candidates
        .into_iter()
        .filter_map(|s| {
            let (score, _) = fuzzy_match(prefix, &s.label)?;
            let prefix_hit = s.label.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase());
            if !prefix.is_empty() && !prefix_hit && prefix.len() < 2 {
                return None;
            }
            let kind_bonus = match s.kind {
                SuggestionKind::Column => 40,
                SuggestionKind::Table | SuggestionKind::View => 30,
                SuggestionKind::Schema => 20,
                SuggestionKind::Function => 5,
                SuggestionKind::Keyword => 0,
            };
            Some((score + kind_bonus + if prefix_hit { 2000 } else { 0 }, s))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
    let mut seen = std::collections::HashSet::new();
    let suggestions: Vec<Suggestion> = scored
        .into_iter()
        .map(|(_, s)| s)
        .filter(|s| seen.insert((s.label.clone(), s.kind, s.insert.clone())))
        .filter(|s| !(s.label.eq_ignore_ascii_case(prefix) && s.kind == SuggestionKind::Keyword))
        .take(60)
        .collect();
    missing.sort();
    missing.dedup();
    CompletionResult { context: ctx, suggestions, missing_columns: missing }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cat {
        objects: Vec<SchemaObject>,
        users: Vec<ColumnInfo>,
    }

    impl Catalog for Cat {
        fn objects(&self) -> &[SchemaObject] {
            &self.objects
        }
        fn columns(&self, _schema: &str, table: &str) -> Option<&[ColumnInfo]> {
            (table == "users").then_some(self.users.as_slice())
        }
        fn default_schema(&self) -> &str {
            "public"
        }
    }

    fn cat() -> Cat {
        Cat {
            objects: vec![
                SchemaObject::new("public", "users", ObjectKind::Table),
                SchemaObject::new("public", "orders", ObjectKind::Table),
                SchemaObject::new("public", "order_items", ObjectKind::Table),
                SchemaObject::new("analytics", "events", ObjectKind::Table),
                SchemaObject::new("public", "active_users", ObjectKind::View),
            ],
            users: vec![
                ColumnInfo { name: "id".into(), data_type: "bigint".into(), ..Default::default() },
                ColumnInfo { name: "email".into(), data_type: "varchar".into(), ..Default::default() },
                ColumnInfo { name: "full_name".into(), data_type: "text".into(), ..Default::default() },
            ],
        }
    }

    fn labels(src: &str) -> Vec<String> {
        complete(src, src.len(), Dialect::Postgres, &cat()).suggestions.into_iter().map(|s| s.label).collect()
    }

    #[test]
    fn context() {
        let c = context_at("select u.em", 11, Dialect::Postgres);
        assert_eq!(c.prefix, "em");
        assert_eq!(c.replace_start, 9);
        assert_eq!(c.qualifier.as_deref(), Some("u"));
        let c = context_at("select * from ord", 17, Dialect::Postgres);
        assert!(c.expects_table);
        assert_eq!(c.prefix, "ord");
        let c = context_at("select \"weird\".x", 16, Dialect::Postgres);
        assert_eq!(c.qualifier.as_deref(), Some("weird"));
    }

    #[test]
    fn refs() {
        let r =
            referenced_tables("select * from public.users u join orders as o on o.user_id = u.id", Dialect::Postgres);
        assert_eq!(
            r,
            vec![
                TableRef { schema: Some("public".into()), name: "users".into(), alias: Some("u".into()) },
                TableRef { schema: None, name: "orders".into(), alias: Some("o".into()) },
            ]
        );
        let r = referenced_tables("select * from a, \"B\" b where x", Dialect::Postgres);
        assert_eq!(
            r,
            vec![
                TableRef { schema: None, name: "a".into(), alias: None },
                TableRef { schema: None, name: "B".into(), alias: Some("b".into()) },
            ]
        );
    }

    #[test]
    fn suggestions() {
        assert_eq!(labels("select * from ord")[..2], ["orders".to_string(), "order_items".to_string()]);
        let src = "select u. from users u";
        let res = complete(src, 9, Dialect::Postgres, &cat());
        let l: Vec<_> = res.suggestions.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(l, vec!["email", "full_name", "id"]);
        let src = "select fu from users";
        let res = complete(src, 9, Dialect::Postgres, &cat());
        assert_eq!(res.suggestions[0].label, "full_name");
        assert!(labels("SEL").contains(&"SELECT".to_string()));
        let res = complete("select * from orders where ", 27, Dialect::Postgres, &cat());
        assert_eq!(res.missing_columns, vec![("public".to_string(), "orders".to_string())]);
        let ev = complete("select * from ev", 16, Dialect::Postgres, &cat());
        assert_eq!(ev.suggestions[0].insert, "analytics.events");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_if_needed(Dialect::Postgres, "users"), "users");
        assert_eq!(quote_if_needed(Dialect::Postgres, "Users"), "\"Users\"");
        assert_eq!(quote_if_needed(Dialect::MySql, "Users"), "Users");
        assert_eq!(quote_if_needed(Dialect::MySql, "order"), "`order`");
    }
}
