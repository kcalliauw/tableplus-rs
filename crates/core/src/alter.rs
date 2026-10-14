//! DDL generation for the structure editor: staged column/index changes become
//! `ALTER TABLE` statements that are previewed before being applied.

use crate::dialect::Dialect;
use crate::model::{ColumnInfo, ConstraintKind, TableStructure};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    /// Default as an SQL expression (`0`, `'US'`, `now()`), `None` for no default.
    pub default: Option<String>,
    pub comment: Option<String>,
    /// Extra column attributes to preserve (MySQL `auto_increment`, `on update …`).
    pub extra: Option<String>,
    pub primary_key: bool,
}

impl From<&ColumnInfo> for ColumnDef {
    fn from(c: &ColumnInfo) -> Self {
        ColumnDef {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: c.nullable,
            default: c.default.clone(),
            comment: c.comment.clone(),
            extra: c.extra.clone(),
            primary_key: c.primary_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StructureChange {
    AddColumn(ColumnDef),
    DropColumn(String),
    ModifyColumn { original: String, new: ColumnDef },
    AddIndex { name: String, columns: Vec<String>, unique: bool },
    DropIndex(String),
    DropConstraint { name: String, kind: ConstraintKind },
}

fn opt_eq(a: &Option<String>, b: &Option<String>) -> bool {
    let norm = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    norm(a) == norm(b)
}

fn column_sql(dialect: Dialect, c: &ColumnDef, inline_comment: bool) -> String {
    let mut s = format!("{} {}", dialect.quote_ident(&c.name), c.data_type.trim());
    if !c.nullable {
        s.push_str(" NOT NULL");
    } else if dialect == Dialect::MySql {
        s.push_str(" NULL");
    }
    if let Some(d) = c.default.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        s.push_str(" DEFAULT ");
        s.push_str(d);
    }
    if dialect == Dialect::MySql {
        if let Some(e) = c.extra.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            s.push(' ');
            s.push_str(e);
        }
        if inline_comment {
            if let Some(cm) = c.comment.as_deref().filter(|c| !c.is_empty()) {
                s.push_str(" COMMENT ");
                s.push_str(&dialect.quote_str(cm));
            }
        }
    }
    s
}

/// `CREATE TABLE` for a new table (also used by the SQLite table rebuild).
pub fn create_table_sql(
    dialect: Dialect,
    schema: &str,
    table: &str,
    columns: &[ColumnDef],
    extra_defs: &[String],
) -> String {
    let mut defs: Vec<String> = columns.iter().map(|c| format!("  {}", column_sql(dialect, c, true))).collect();
    let pk: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| dialect.quote_ident(&c.name)).collect();
    if !pk.is_empty() {
        defs.push(format!("  PRIMARY KEY ({})", pk.join(", ")));
    }
    defs.extend(extra_defs.iter().map(|d| format!("  {d}")));
    format!("CREATE TABLE {} (\n{}\n)", dialect.qualified(schema, table), defs.join(",\n"))
}

/// Generates the statements implementing `changes`, in order.
pub fn alter_statements(
    dialect: Dialect,
    schema: &str,
    table: &str,
    current: &TableStructure,
    changes: &[StructureChange],
) -> Result<Vec<String>, String> {
    if dialect == Dialect::Sqlite && needs_sqlite_rebuild(current, changes) {
        return sqlite_rebuild(schema, table, current, changes);
    }
    let t = dialect.qualified(schema, table);
    let q = |s: &str| dialect.quote_ident(s);
    let mut out = Vec::new();
    for change in changes {
        match change {
            StructureChange::AddColumn(c) => {
                if c.name.trim().is_empty() || c.data_type.trim().is_empty() {
                    return Err("New columns need a name and a type".into());
                }
                out.push(format!("ALTER TABLE {t} ADD COLUMN {}", column_sql(dialect, c, true)));
                if dialect == Dialect::Postgres {
                    if let Some(cm) = c.comment.as_deref().filter(|c| !c.is_empty()) {
                        out.push(format!("COMMENT ON COLUMN {t}.{} IS {}", q(&c.name), dialect.quote_str(cm)));
                    }
                }
            }
            StructureChange::DropColumn(name) => out.push(format!("ALTER TABLE {t} DROP COLUMN {}", q(name))),
            StructureChange::ModifyColumn { original, new } => {
                let orig = current.column(original).ok_or_else(|| format!("Column {original} no longer exists"))?;
                modify_column(dialect, &t, orig, new, &mut out);
            }
            StructureChange::AddIndex { name, columns, unique } => {
                if columns.is_empty() {
                    return Err("An index needs at least one column".into());
                }
                let cols = columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
                let unique = if *unique { "UNIQUE " } else { "" };
                let idx_name = if dialect == Dialect::Sqlite && !schema.is_empty() && schema != "main" {
                    dialect.qualified(schema, name)
                } else {
                    q(name)
                };
                let target = if dialect == Dialect::Sqlite { q(table) } else { t.clone() };
                out.push(format!("CREATE {unique}INDEX {idx_name} ON {target} ({cols})"));
            }
            StructureChange::DropIndex(name) => out.push(match dialect {
                Dialect::Postgres | Dialect::Sqlite => {
                    format!("DROP INDEX {}", dialect.qualified(schema, name))
                }
                Dialect::MySql => format!("DROP INDEX {} ON {t}", q(name)),
            }),
            StructureChange::DropConstraint { name, kind } => out.push(match (dialect, kind) {
                (Dialect::MySql, ConstraintKind::ForeignKey) => {
                    format!("ALTER TABLE {t} DROP FOREIGN KEY {}", q(name))
                }
                (Dialect::MySql, ConstraintKind::PrimaryKey) => {
                    format!("ALTER TABLE {t} DROP PRIMARY KEY")
                }
                (Dialect::MySql, ConstraintKind::Unique) => {
                    format!("ALTER TABLE {t} DROP INDEX {}", q(name))
                }
                (Dialect::Sqlite, _) => {
                    return Err("SQLite cannot drop constraints in place".into());
                }
                _ => format!("ALTER TABLE {t} DROP CONSTRAINT {}", q(name)),
            }),
        }
    }
    Ok(out)
}

fn modify_column(dialect: Dialect, t: &str, orig: &ColumnInfo, new: &ColumnDef, out: &mut Vec<String>) {
    let q = |s: &str| dialect.quote_ident(s);
    let renamed = orig.name != new.name;
    let retyped = orig.data_type.trim() != new.data_type.trim();
    let null_changed = orig.nullable != new.nullable;
    let default_changed = !opt_eq(&orig.default, &new.default);
    let comment_changed = !opt_eq(&orig.comment, &new.comment);
    match dialect {
        Dialect::MySql => {
            if renamed || retyped || null_changed || default_changed || comment_changed {
                out.push(format!("ALTER TABLE {t} CHANGE COLUMN {} {}", q(&orig.name), column_sql(dialect, new, true)));
            }
        }
        Dialect::Postgres => {
            let col = q(&new.name);
            if renamed {
                out.push(format!("ALTER TABLE {t} RENAME COLUMN {} TO {col}", q(&orig.name)));
            }
            if retyped {
                let ty = new.data_type.trim();
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty} USING {col}::{ty}"));
            }
            if null_changed {
                let action = if new.nullable { "DROP NOT NULL" } else { "SET NOT NULL" };
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {action}"));
            }
            if default_changed {
                match new.default.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                    Some(d) => out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {d}")),
                    None => out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT")),
                }
            }
            if comment_changed {
                let c = match new.comment.as_deref().filter(|c| !c.is_empty()) {
                    Some(c) => dialect.quote_str(c),
                    None => "NULL".into(),
                };
                out.push(format!("COMMENT ON COLUMN {t}.{col} IS {c}"));
            }
        }
        Dialect::Sqlite => {
            // Only reached for pure renames; everything else goes through the rebuild.
            if renamed {
                out.push(format!("ALTER TABLE {t} RENAME COLUMN {} TO {}", q(&orig.name), q(&new.name)));
            }
        }
    }
}

fn needs_sqlite_rebuild(current: &TableStructure, changes: &[StructureChange]) -> bool {
    changes.iter().any(|c| match c {
        StructureChange::ModifyColumn { original, new } => match current.column(original) {
            Some(o) => {
                o.data_type.trim() != new.data_type.trim()
                    || o.nullable != new.nullable
                    || !opt_eq(&o.default, &new.default)
                    || o.primary_key != new.primary_key
            }
            None => false,
        },
        StructureChange::AddColumn(c) => c.primary_key || (!c.nullable && c.default.is_none()),
        _ => false,
    })
}

/// SQLite can't alter column types/nullability in place, so the table is rebuilt:
/// create a copy with the new definition, copy the data, swap, and recreate indexes.
fn sqlite_rebuild(
    schema: &str,
    table: &str,
    current: &TableStructure,
    changes: &[StructureChange],
) -> Result<Vec<String>, String> {
    let d = Dialect::Sqlite;
    // (new definition, source column name in the old table)
    let mut cols: Vec<(ColumnDef, Option<String>)> =
        current.columns.iter().map(|c| (ColumnDef::from(c), Some(c.name.clone()))).collect();
    let mut index_changes = Vec::new();
    for change in changes {
        match change {
            StructureChange::AddColumn(c) => cols.push((c.clone(), None)),
            StructureChange::DropColumn(name) => cols.retain(|(c, _)| &c.name != name),
            StructureChange::ModifyColumn { original, new } => {
                let slot = cols
                    .iter_mut()
                    .find(|(_, src)| src.as_deref() == Some(original.as_str()))
                    .ok_or_else(|| format!("Column {original} no longer exists"))?;
                slot.0 = new.clone();
            }
            other => index_changes.push(other.clone()),
        }
    }
    let renames: Vec<(String, String)> =
        cols.iter().filter_map(|(c, src)| src.as_ref().map(|s| (s.clone(), c.name.clone()))).collect();
    let rename = |old: &str| renames.iter().find(|(o, _)| o == old).map(|(_, n)| n.clone()).unwrap_or(old.into());

    let fks: Vec<String> = current
        .constraints
        .iter()
        .filter(|c| c.kind == ConstraintKind::ForeignKey)
        .map(|c| {
            let cols = c.columns.iter().map(|col| d.quote_ident(&rename(col))).collect::<Vec<_>>().join(", ");
            format!("FOREIGN KEY ({cols}) {}", c.definition)
        })
        .collect();
    let tmp = format!("_tableplus_rebuild_{table}");
    let defs: Vec<ColumnDef> = cols.iter().map(|(c, _)| c.clone()).collect();
    let mut out = vec![create_table_sql(d, schema, &tmp, &defs, &fks)];
    let copied: Vec<&(ColumnDef, Option<String>)> = cols.iter().filter(|(_, src)| src.is_some()).collect();
    if !copied.is_empty() {
        let dst = copied.iter().map(|(c, _)| d.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
        let src =
            copied.iter().map(|(_, s)| d.quote_ident(s.as_deref().unwrap_or_default())).collect::<Vec<_>>().join(", ");
        out.push(format!(
            "INSERT INTO {} ({dst}) SELECT {src} FROM {}",
            d.qualified(schema, &tmp),
            d.qualified(schema, table)
        ));
    }
    out.push(format!("DROP TABLE {}", d.qualified(schema, table)));
    out.push(format!("ALTER TABLE {} RENAME TO {}", d.qualified(schema, &tmp), d.quote_ident(table)));
    let dropped: Vec<&str> = index_changes
        .iter()
        .filter_map(|c| match c {
            StructureChange::DropIndex(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    let surviving: Vec<String> = cols.iter().map(|(c, _)| c.name.clone()).collect();
    for idx in &current.indexes {
        if idx.primary || dropped.contains(&idx.name.as_str()) {
            continue;
        }
        let new_cols: Vec<String> = idx.columns.iter().map(|c| rename(c)).collect();
        if !new_cols.iter().all(|c| surviving.contains(c)) {
            continue;
        }
        if idx.definition.is_some() {
            let cols = new_cols.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", ");
            let unique = if idx.unique { "UNIQUE " } else { "" };
            out.push(format!(
                "CREATE {unique}INDEX {} ON {} ({cols})",
                d.qualified(schema, &idx.name),
                d.quote_ident(table)
            ));
        }
    }
    let rest: Vec<StructureChange> =
        index_changes.into_iter().filter(|c| !matches!(c, StructureChange::DropIndex(_))).collect();
    if !rest.is_empty() {
        let mut after = current.clone();
        after.columns = cols
            .iter()
            .map(|(c, _)| ColumnInfo {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                nullable: c.nullable,
                default: c.default.clone(),
                primary_key: c.primary_key,
                ..Default::default()
            })
            .collect();
        out.extend(alter_statements(d, schema, table, &after, &rest)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConstraintInfo, IndexInfo};

    fn users() -> TableStructure {
        TableStructure {
            columns: vec![
                ColumnInfo { name: "id".into(), data_type: "integer".into(), primary_key: true, ..Default::default() },
                ColumnInfo {
                    name: "email".into(),
                    data_type: "varchar(255)".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnInfo {
                    name: "country".into(),
                    data_type: "char(2)".into(),
                    nullable: true,
                    default: Some("'US'".into()),
                    ..Default::default()
                },
            ],
            indexes: vec![IndexInfo {
                name: "users_country_idx".into(),
                columns: vec!["country".into()],
                definition: Some("CREATE INDEX users_country_idx ON users (country)".into()),
                ..Default::default()
            }],
            constraints: vec![ConstraintInfo {
                name: "fk".into(),
                kind: ConstraintKind::ForeignKey,
                columns: vec!["country".into()],
                definition: "REFERENCES \"countries\" (\"code\")".into(),
            }],
            comment: None,
        }
    }

    fn modified(f: impl FnOnce(&mut ColumnDef)) -> StructureChange {
        let s = users();
        let mut new = ColumnDef::from(&s.columns[2]);
        f(&mut new);
        StructureChange::ModifyColumn { original: "country".into(), new }
    }

    #[test]
    fn postgres_alter() {
        let change = modified(|c| {
            c.name = "cc".into();
            c.data_type = "varchar(3)".into();
            c.nullable = false;
            c.default = None;
            c.comment = Some("ISO".into());
        });
        let sql = alter_statements(Dialect::Postgres, "public", "users", &users(), &[change]).unwrap();
        assert_eq!(
            sql,
            vec![
                "ALTER TABLE \"public\".\"users\" RENAME COLUMN \"country\" TO \"cc\"",
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"cc\" TYPE varchar(3) USING \"cc\"::varchar(3)",
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"cc\" SET NOT NULL",
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"cc\" DROP DEFAULT",
                "COMMENT ON COLUMN \"public\".\"users\".\"cc\" IS 'ISO'",
            ]
        );
        let unchanged = modified(|_| {});
        assert!(alter_statements(Dialect::Postgres, "public", "users", &users(), &[unchanged]).unwrap().is_empty());
    }

    #[test]
    fn mysql_alter() {
        let changes = vec![
            modified(|c| c.default = Some("'CA'".into())),
            StructureChange::AddColumn(ColumnDef {
                name: "age".into(),
                data_type: "int".into(),
                nullable: true,
                ..Default::default()
            }),
            StructureChange::AddIndex { name: "age_idx".into(), columns: vec!["age".into()], unique: false },
            StructureChange::DropIndex("users_country_idx".into()),
            StructureChange::DropConstraint { name: "fk".into(), kind: ConstraintKind::ForeignKey },
        ];
        let sql = alter_statements(Dialect::MySql, "shop", "users", &users(), &changes).unwrap();
        assert_eq!(
            sql,
            vec![
                "ALTER TABLE `shop`.`users` CHANGE COLUMN `country` `country` char(2) NULL DEFAULT 'CA'",
                "ALTER TABLE `shop`.`users` ADD COLUMN `age` int NULL",
                "CREATE INDEX `age_idx` ON `shop`.`users` (`age`)",
                "DROP INDEX `users_country_idx` ON `shop`.`users`",
                "ALTER TABLE `shop`.`users` DROP FOREIGN KEY `fk`",
            ]
        );
    }

    #[test]
    fn sqlite_simple_and_rebuild() {
        let rename = modified(|c| c.name = "cc".into());
        assert_eq!(
            alter_statements(Dialect::Sqlite, "main", "users", &users(), &[rename]).unwrap(),
            vec!["ALTER TABLE \"users\" RENAME COLUMN \"country\" TO \"cc\""]
        );
        let retype = modified(|c| {
            c.name = "cc".into();
            c.nullable = false;
        });
        let sql = alter_statements(Dialect::Sqlite, "main", "users", &users(), &[retype]).unwrap();
        assert_eq!(sql.len(), 5, "{sql:#?}");
        assert!(sql[0].starts_with("CREATE TABLE \"_tableplus_rebuild_users\""));
        assert!(sql[0].contains("\"cc\" char(2) NOT NULL DEFAULT 'US'"));
        assert!(sql[0].contains("PRIMARY KEY (\"id\")"));
        assert!(sql[0].contains("FOREIGN KEY (\"cc\") REFERENCES \"countries\" (\"code\")"));
        assert_eq!(
            sql[1],
            "INSERT INTO \"_tableplus_rebuild_users\" (\"id\", \"email\", \"cc\") SELECT \"id\", \"email\", \"country\" FROM \"users\""
        );
        assert_eq!(sql[2], "DROP TABLE \"users\"");
        assert_eq!(sql[3], "ALTER TABLE \"_tableplus_rebuild_users\" RENAME TO \"users\"");
        assert_eq!(sql[4], "CREATE INDEX \"users_country_idx\" ON \"users\" (\"cc\")");
    }

    #[test]
    fn create_table() {
        let cols = vec![
            ColumnDef { name: "id".into(), data_type: "bigint".into(), primary_key: true, ..Default::default() },
            ColumnDef { name: "note".into(), data_type: "text".into(), nullable: true, ..Default::default() },
        ];
        assert_eq!(
            create_table_sql(Dialect::Postgres, "public", "t", &cols, &[]),
            "CREATE TABLE \"public\".\"t\" (\n  \"id\" bigint NOT NULL,\n  \"note\" text,\n  PRIMARY KEY (\"id\")\n)"
        );
    }
}
