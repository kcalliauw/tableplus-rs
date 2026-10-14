//! Pending (uncommitted) row edits for a table page, and the grid view over them.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use tableplus_core::Dialect;
use tableplus_core::model::{ColumnInfo, ColumnMeta, Row};
use tableplus_core::sqlgen::{self, CellEdit, RowIdentity, SortKey};

use crate::grid::{CellState, CellView, GridData};

/// Edits keyed by (row, column) of the loaded page; inserted rows come after the page rows.
#[derive(Default, Debug, Clone)]
pub struct StagedChanges {
    pub edits: BTreeMap<(usize, usize), CellEdit>,
    pub deleted: BTreeSet<usize>,
    pub inserted: Vec<Vec<Option<CellEdit>>>,
}

impl StagedChanges {
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Number of affected rows (for the "Commit (n)" button).
    pub fn count(&self) -> usize {
        let edited: BTreeSet<usize> =
            self.edits.keys().map(|(r, _)| *r).filter(|r| !self.deleted.contains(r)).collect();
        edited.len() + self.deleted.len() + self.inserted.len()
    }

    /// Applies a grid edit to an existing (`row < page_len`) or inserted row.
    pub fn set(&mut self, page_rows: &[Row], row: usize, col: usize, edit: CellEdit) {
        let page_len = page_rows.len();
        if row >= page_len {
            if let Some(r) = self.inserted.get_mut(row - page_len) {
                r[col] = if edit == CellEdit::Default { None } else { Some(edit) };
            }
            return;
        }
        // Editing back to the original value removes the edit.
        let original = &page_rows[row][col];
        let same = match &edit {
            CellEdit::Null => original.is_null(),
            CellEdit::Text(t) => original.to_text().as_deref() == Some(t.as_str()),
            CellEdit::Default => false,
        };
        if same {
            self.edits.remove(&(row, col));
        } else {
            self.edits.insert((row, col), edit);
        }
    }

    pub fn add_row(&mut self, ncols: usize) {
        self.inserted.push(vec![None; ncols]);
    }

    /// Copies an existing or inserted row (minus key columns) into a new row.
    pub fn duplicate(&mut self, page_rows: &[Row], columns: &[ColumnInfo], row: usize) {
        let page_len = page_rows.len();
        let ncols = columns.len();
        let mut new_row: Vec<Option<CellEdit>> = vec![None; ncols];
        for (c, slot) in new_row.iter_mut().enumerate() {
            let auto = columns.get(c).is_some_and(|ci| ci.primary_key && (ci.default.is_some() || ci.extra.is_some()));
            if auto {
                continue;
            }
            *slot = if row < page_len {
                match self.edits.get(&(row, c)) {
                    Some(e) => Some(e.clone()),
                    None => Some(match page_rows[row][c].to_text() {
                        Some(t) => CellEdit::Text(t),
                        None => CellEdit::Null,
                    }),
                }
            } else {
                self.inserted.get(row - page_len).and_then(|r| r[c].clone())
            };
        }
        self.inserted.push(new_row);
    }

    /// Marks rows deleted; inserted rows are simply dropped.
    pub fn delete(&mut self, page_len: usize, rows: &[usize]) {
        let mut inserted: Vec<usize> = rows.iter().filter(|r| **r >= page_len).map(|r| r - page_len).collect();
        inserted.sort_unstable_by(|a, b| b.cmp(a));
        for i in inserted {
            if i < self.inserted.len() {
                self.inserted.remove(i);
            }
        }
        for r in rows.iter().filter(|r| **r < page_len) {
            self.deleted.insert(*r);
        }
    }

    pub fn undelete(&mut self, rows: &[usize]) {
        for r in rows {
            self.deleted.remove(r);
        }
    }

    /// SQL for every pending change: deletes, then updates, then inserts.
    pub fn statements(
        &self,
        dialect: Dialect,
        schema: &str,
        table: &str,
        columns: &[ColumnInfo],
        page_rows: &[Row],
    ) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.deleted {
            if let Some(row) = page_rows.get(*r) {
                out.push(sqlgen::delete_sql(dialect, schema, table, &RowIdentity::new(columns, row)));
            }
        }
        let mut by_row: BTreeMap<usize, Vec<(String, CellEdit)>> = BTreeMap::new();
        for ((r, c), e) in &self.edits {
            if self.deleted.contains(r) {
                continue;
            }
            if let Some(col) = columns.get(*c) {
                by_row.entry(*r).or_default().push((col.name.clone(), e.clone()));
            }
        }
        for (r, changes) in by_row {
            if let Some(row) = page_rows.get(r) {
                out.push(sqlgen::update_sql(
                    dialect,
                    schema,
                    table,
                    columns,
                    &RowIdentity::new(columns, row),
                    &changes,
                ));
            }
        }
        for row in &self.inserted {
            let values: Vec<(String, CellEdit)> =
                row.iter().enumerate().filter_map(|(c, e)| Some((columns.get(c)?.name.clone(), e.clone()?))).collect();
            out.push(sqlgen::insert_sql(dialect, schema, table, columns, &values));
        }
        out
    }
}

/// Grid adapter: the loaded page plus staged edits.
pub struct TableData<'a> {
    pub columns: &'a [ColumnMeta],
    pub rows: &'a [Row],
    pub staged: &'a StagedChanges,
    pub sort: &'a [SortKey],
    pub keys: &'a [bool],
    pub editable: bool,
    pub first_row_number: u64,
}

impl GridData for TableData<'_> {
    fn columns(&self) -> &[ColumnMeta] {
        self.columns
    }

    fn row_count(&self) -> usize {
        self.rows.len() + self.staged.inserted.len()
    }

    fn cell(&self, row: usize, col: usize) -> CellView<'_> {
        if row >= self.rows.len() {
            let edit = self.staged.inserted.get(row - self.rows.len()).and_then(|r| r.get(col)).cloned().flatten();
            return match edit {
                None => CellView {
                    text: Cow::Borrowed("DEFAULT"),
                    null: false,
                    numeric: false,
                    state: CellState::Inserted,
                    placeholder: true,
                },
                Some(CellEdit::Null) => CellView {
                    text: Cow::Borrowed("NULL"),
                    null: true,
                    numeric: false,
                    state: CellState::Inserted,
                    placeholder: false,
                },
                Some(CellEdit::Default) => CellView {
                    text: Cow::Borrowed("DEFAULT"),
                    null: false,
                    numeric: false,
                    state: CellState::Inserted,
                    placeholder: true,
                },
                Some(CellEdit::Text(t)) => CellView {
                    text: Cow::Owned(single_line(&t)),
                    null: false,
                    numeric: false,
                    state: CellState::Inserted,
                    placeholder: false,
                },
            };
        }
        if let Some(edit) = self.staged.edits.get(&(row, col)) {
            return match edit {
                CellEdit::Null => CellView {
                    text: Cow::Borrowed("NULL"),
                    null: true,
                    numeric: false,
                    state: CellState::Modified,
                    placeholder: false,
                },
                CellEdit::Default => CellView {
                    text: Cow::Borrowed("DEFAULT"),
                    null: false,
                    numeric: false,
                    state: CellState::Modified,
                    placeholder: true,
                },
                CellEdit::Text(t) => CellView {
                    text: Cow::Owned(single_line(t)),
                    null: false,
                    numeric: false,
                    state: CellState::Modified,
                    placeholder: false,
                },
            };
        }
        let v = &self.rows[row][col];
        CellView {
            text: v.display(),
            null: v.is_null(),
            numeric: v.is_numeric(),
            state: CellState::Normal,
            placeholder: false,
        }
    }

    fn row_state(&self, row: usize) -> CellState {
        if row >= self.rows.len() {
            CellState::Inserted
        } else if self.staged.deleted.contains(&row) {
            CellState::Deleted
        } else {
            CellState::Normal
        }
    }

    fn full_text(&self, row: usize, col: usize) -> Option<String> {
        if row >= self.rows.len() {
            return match self.staged.inserted.get(row - self.rows.len()).and_then(|r| r.get(col)).cloned().flatten() {
                Some(CellEdit::Text(t)) => Some(t),
                _ => None,
            };
        }
        match self.staged.edits.get(&(row, col)) {
            Some(CellEdit::Text(t)) => Some(t.clone()),
            Some(_) => None,
            None => self.rows[row][col].to_text(),
        }
    }

    fn editable(&self) -> bool {
        self.editable
    }

    fn sort_of(&self, col: usize) -> Option<bool> {
        let name = &self.columns.get(col)?.name;
        self.sort.iter().find(|s| &s.column == name).map(|s| s.descending)
    }

    fn is_key(&self, col: usize) -> bool {
        self.keys.get(col).copied().unwrap_or(false)
    }

    fn row_number(&self, row: usize) -> u64 {
        self.first_row_number + row as u64
    }
}

fn single_line(s: &str) -> String {
    tableplus_core::Value::text(s).display().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tableplus_core::Value;

    fn cols() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo {
                name: "id".into(),
                data_type: "integer".into(),
                primary_key: true,
                default: Some("nextval('s')".into()),
                ..Default::default()
            },
            ColumnInfo { name: "name".into(), data_type: "text".into(), nullable: true, ..Default::default() },
        ]
    }

    fn rows() -> Vec<Row> {
        vec![vec![Value::Int(1), Value::text("a")].into(), vec![Value::Int(2), Value::Null].into()]
    }

    #[test]
    fn edits_generate_sql() {
        let rows = rows();
        let mut s = StagedChanges::default();
        s.set(&rows, 0, 1, CellEdit::Text("b".into()));
        s.set(&rows, 1, 1, CellEdit::Null); // unchanged → no edit
        assert_eq!(s.edits.len(), 1);
        s.set(&rows, 0, 1, CellEdit::Text("a".into())); // back to original
        assert!(s.is_empty());
        s.set(&rows, 0, 1, CellEdit::Text("b".into()));
        s.delete(2, &[1]);
        s.add_row(2);
        s.set(&rows, 2, 1, CellEdit::Text("new".into()));
        s.duplicate(&rows, &cols(), 0);
        assert_eq!(s.count(), 4);
        let sql = s.statements(Dialect::Postgres, "public", "t", &cols(), &rows);
        assert_eq!(
            sql,
            vec![
                "DELETE FROM \"public\".\"t\" WHERE \"id\" = 2",
                "UPDATE \"public\".\"t\" SET \"name\" = 'b' WHERE \"id\" = 1",
                "INSERT INTO \"public\".\"t\" (\"name\") VALUES ('new')",
                "INSERT INTO \"public\".\"t\" (\"name\") VALUES ('b')",
            ]
        );
        s.delete(2, &[2]);
        assert_eq!(s.inserted.len(), 1);
        s.undelete(&[1]);
        assert!(s.deleted.is_empty());
    }

    #[test]
    fn grid_view_reflects_staging() {
        let rows = rows();
        let mut s = StagedChanges::default();
        s.set(&rows, 0, 1, CellEdit::Null);
        s.add_row(2);
        let meta = vec![ColumnMeta::new("id", "int4"), ColumnMeta::new("name", "text")];
        let view = TableData {
            columns: &meta,
            rows: &rows,
            staged: &s,
            sort: &[],
            keys: &[true, false],
            editable: true,
            first_row_number: 301,
        };
        assert_eq!(view.row_count(), 3);
        assert!(view.cell(0, 1).null);
        assert_eq!(view.cell(0, 1).state, CellState::Modified);
        assert_eq!(view.cell(2, 0).text, "DEFAULT");
        assert_eq!(view.row_state(2), CellState::Inserted);
        assert_eq!(view.row_number(1), 302);
        assert_eq!(view.full_text(0, 1), None);
        assert_eq!(view.full_text(0, 0).as_deref(), Some("1"));
    }
}
