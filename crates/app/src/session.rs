//! An open connection: schema objects, cached table structures and the open tabs.

use std::collections::{HashMap, HashSet};

use tableplus_core::Dialect;
use tableplus_core::completion::Catalog;
use tableplus_core::config::ConnectionConfig;
use tableplus_core::driver::DynConnection;
use tableplus_core::model::{ColumnInfo, ObjectKind, SchemaObject, TableStructure};

use crate::icons::Icon;
use crate::query_tab::QueryTab;
use crate::table_tab::TableTab;
use crate::task::{Spawner, Task, poll_slot};

/// What the editor's autocomplete knows about the database.
#[derive(Default)]
pub struct SessionCatalog {
    pub objects: Vec<SchemaObject>,
    pub structures: HashMap<(String, String), TableStructure>,
    pub default_schema: String,
}

impl Catalog for SessionCatalog {
    fn objects(&self) -> &[SchemaObject] {
        &self.objects
    }

    fn columns(&self, schema: &str, table: &str) -> Option<&[ColumnInfo]> {
        self.structures.get(&(schema.to_string(), table.to_string())).map(|s| s.columns.as_slice())
    }

    fn default_schema(&self) -> &str {
        &self.default_schema
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    Items,
    History,
}

pub struct SidebarState {
    pub filter: String,
    pub history_filter: String,
    pub expanded: HashSet<String>,
    pub mode: SidebarMode,
}

pub struct DefinitionTab {
    pub object: SchemaObject,
    pub ddl: Option<Result<String, String>>,
    pub task: Option<Task<String>>,
}

pub enum TabKind {
    Table(Box<TableTab>),
    Query(Box<QueryTab>),
    Definition(DefinitionTab),
}

pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

impl Tab {
    pub fn title(&self) -> String {
        match &self.kind {
            TabKind::Table(t) => t.object.name.clone(),
            TabKind::Query(q) => q.title.clone(),
            TabKind::Definition(d) => d.object.name.clone(),
        }
    }

    pub fn icon(&self) -> Icon {
        match &self.kind {
            TabKind::Table(t) => crate::icons::kind_icon(t.object.kind),
            TabKind::Query(_) => Icon::Sql,
            TabKind::Definition(d) => crate::icons::kind_icon(d.object.kind),
        }
    }

    pub fn object(&self) -> Option<&SchemaObject> {
        match &self.kind {
            TabKind::Table(t) => Some(&t.object),
            TabKind::Definition(d) => Some(&d.object),
            TabKind::Query(_) => None,
        }
    }

    pub fn has_unsaved_changes(&self) -> bool {
        match &self.kind {
            TabKind::Table(t) => t.has_pending_changes(),
            _ => false,
        }
    }
}

pub struct Session {
    pub key: u64,
    pub config: ConnectionConfig,
    pub conn: DynConnection,
    pub server_version: String,
    pub database: String,
    pub dialect: Dialect,
    pub schemas: Vec<String>,
    pub catalog: SessionCatalog,
    pub objects_task: Option<Task<(Vec<String>, Vec<SchemaObject>)>>,
    pub objects_error: Option<String>,
    structure_tasks: HashMap<(String, String), Task<TableStructure>>,
    pub sidebar: SidebarState,
    pub tabs: Vec<Tab>,
    pub active_tab: Option<usize>,
    next_tab_id: u64,
    query_counter: usize,
}

impl Session {
    pub fn new(key: u64, config: ConnectionConfig, conn: DynConnection, spawner: &Spawner) -> Self {
        let default_schema = conn.default_schema();
        let mut expanded = HashSet::new();
        expanded.insert(default_schema.clone());
        for kind in [ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView] {
            expanded.insert(format!("{default_schema}/{}", kind.plural()));
        }
        let mut s = Self {
            key,
            server_version: conn.server_version(),
            database: conn.database(),
            dialect: conn.dialect(),
            config,
            catalog: SessionCatalog { default_schema, ..Default::default() },
            conn,
            schemas: Vec::new(),
            objects_task: None,
            objects_error: None,
            structure_tasks: HashMap::new(),
            sidebar: SidebarState {
                filter: String::new(),
                history_filter: String::new(),
                expanded,
                mode: SidebarMode::Items,
            },
            tabs: Vec::new(),
            active_tab: None,
            next_tab_id: 1,
            query_counter: 0,
        };
        s.reload_objects(spawner);
        s
    }

    pub fn reload_objects(&mut self, spawner: &Spawner) {
        let conn = self.conn.clone();
        self.objects_task = Some(spawner.spawn(async move {
            let schemas = conn.list_schemas().await?;
            let objects = conn.list_objects().await?;
            Ok((schemas, objects))
        }));
    }

    /// Requests a table's structure for autocomplete / editing if not cached.
    pub fn ensure_structure(&mut self, spawner: &Spawner, schema: &str, table: &str) {
        let key = (schema.to_string(), table.to_string());
        if self.catalog.structures.contains_key(&key) || self.structure_tasks.contains_key(&key) {
            return;
        }
        let conn = self.conn.clone();
        let (s, t) = key.clone();
        self.structure_tasks.insert(key, spawner.spawn(async move { conn.table_structure(&s, &t).await }));
    }

    pub fn invalidate_structure(&mut self, schema: &str, table: &str) {
        self.catalog.structures.remove(&(schema.to_string(), table.to_string()));
    }

    pub fn poll(&mut self) {
        if let Some(result) = poll_slot(&mut self.objects_task) {
            match result {
                Ok((schemas, objects)) => {
                    self.schemas = schemas;
                    self.catalog.objects = objects;
                    self.objects_error = None;
                }
                Err(e) => self.objects_error = Some(format!("{e:#}")),
            }
        }
        let done: Vec<_> =
            self.structure_tasks.iter_mut().filter_map(|(k, t)| t.poll().map(|r| (k.clone(), r))).collect();
        for (key, result) in done {
            self.structure_tasks.remove(&key);
            if let Ok(st) = result {
                self.catalog.structures.insert(key, st);
            }
        }
    }

    pub fn is_loading_objects(&self) -> bool {
        self.objects_task.is_some()
    }

    fn alloc_tab_id(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id
    }

    pub fn active_object(&self) -> Option<&SchemaObject> {
        self.active_tab.and_then(|i| self.tabs.get(i)).and_then(Tab::object)
    }

    /// Opens (or focuses) the data/structure tab for a relation, or a definition tab for routines.
    pub fn open_object(&mut self, spawner: &Spawner, object: &SchemaObject, structure: bool, page_size: usize) {
        if let Some(i) = self.tabs.iter().position(|t| t.object() == Some(object)) {
            self.active_tab = Some(i);
            if let TabKind::Table(t) = &mut self.tabs[i].kind
                && structure
            {
                t.show_structure();
            }
            return;
        }
        let id = self.alloc_tab_id();
        let kind = if object.kind.is_relation() {
            let mut tab = TableTab::new(object.clone(), page_size, self.conn.clone(), spawner);
            if structure {
                tab.show_structure();
            }
            TabKind::Table(Box::new(tab))
        } else {
            let conn = self.conn.clone();
            let obj = object.clone();
            TabKind::Definition(DefinitionTab {
                object: object.clone(),
                ddl: None,
                task: Some(spawner.spawn(async move { conn.object_definition(&obj).await })),
            })
        };
        self.tabs.push(Tab { id, kind });
        self.active_tab = Some(self.tabs.len() - 1);
    }

    pub fn open_definition(&mut self, spawner: &Spawner, object: &SchemaObject) {
        let id = self.alloc_tab_id();
        let conn = self.conn.clone();
        let obj = object.clone();
        self.tabs.push(Tab {
            id,
            kind: TabKind::Definition(DefinitionTab {
                object: object.clone(),
                ddl: None,
                task: Some(spawner.spawn(async move { conn.object_definition(&obj).await })),
            }),
        });
        self.active_tab = Some(self.tabs.len() - 1);
    }

    pub fn new_query(&mut self, sql: Option<String>) {
        self.query_counter += 1;
        let id = self.alloc_tab_id();
        let title = format!("Query {}", self.query_counter);
        self.tabs.push(Tab { id, kind: TabKind::Query(Box::new(QueryTab::new(title, sql.unwrap_or_default()))) });
        self.active_tab = Some(self.tabs.len() - 1);
    }

    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.active_tab = match self.active_tab {
            _ if self.tabs.is_empty() => None,
            Some(a) if a > index => Some(a - 1),
            Some(a) if a == index => Some(index.min(self.tabs.len() - 1)),
            other => other,
        };
    }

    pub fn display_name(&self) -> String {
        self.config.display_name()
    }
}
