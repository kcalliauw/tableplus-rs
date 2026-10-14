//! On-disk state: saved connections and query history.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ConnectionConfig;

/// Root directory for all persisted state. `TABLEPLUS_RS_HOME` overrides the platform
/// config directory (used by tests and the screenshot harness).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TABLEPLUS_RS_HOME") {
        return PathBuf::from(dir);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("tableplus-rs")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    connections: Vec<ConnectionConfig>,
}

#[derive(Debug)]
pub struct ConnectionStore {
    path: PathBuf,
    pub connections: Vec<ConnectionConfig>,
}

impl ConnectionStore {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("connections.json");
        let connections = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<ConnectionsFile>(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?
                    .connections
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, connections })
    }

    pub fn save(&self) -> Result<()> {
        let file = ConnectionsFile { connections: self.connections.clone() };
        write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?)
    }

    pub fn get(&self, id: Uuid) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Inserts or replaces (by id) and saves.
    pub fn upsert(&mut self, config: ConnectionConfig) -> Result<()> {
        match self.connections.iter_mut().find(|c| c.id == config.id) {
            Some(existing) => *existing = config,
            None => self.connections.push(config),
        }
        self.save()
    }

    pub fn remove(&mut self, id: Uuid) -> Result<()> {
        self.connections.retain(|c| c.id != id);
        self.save()
    }

    pub fn duplicate(&mut self, id: Uuid) -> Result<Option<ConnectionConfig>> {
        let Some(orig) = self.get(id).cloned() else {
            return Ok(None);
        };
        let mut copy = orig;
        copy.id = Uuid::new_v4();
        copy.name = format!("{} copy", copy.display_name());
        self.connections.push(copy.clone());
        self.save()?;
        Ok(Some(copy))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub connection_id: Uuid,
    pub connection_name: String,
    pub sql: String,
    pub executed_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// Rows returned or affected.
    pub rows: Option<u64>,
    pub error: Option<String>,
}

/// Append-only JSON-lines query history.
#[derive(Debug)]
pub struct History {
    path: PathBuf,
    pub entries: Vec<HistoryEntry>,
}

const HISTORY_IN_MEMORY: usize = 2000;

impl History {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("history.jsonl");
        let mut entries = Vec::new();
        if let Ok(f) = std::fs::File::open(&path) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(e) = serde_json::from_str::<HistoryEntry>(&line) {
                    entries.push(e);
                }
            }
        }
        if entries.len() > HISTORY_IN_MEMORY {
            entries.drain(..entries.len() - HISTORY_IN_MEMORY);
        }
        Self { path, entries }
    }

    pub fn push(&mut self, entry: HistoryEntry) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&entry)?)?;
        self.entries.push(entry);
        if self.entries.len() > HISTORY_IN_MEMORY {
            self.entries.remove(0);
        }
        Ok(())
    }

    /// Newest first, optionally limited to one connection and a case-insensitive search.
    pub fn search<'a>(
        &'a self,
        connection: Option<Uuid>,
        query: &'a str,
    ) -> impl Iterator<Item = &'a HistoryEntry> + 'a {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .rev()
            .filter(move |e| connection.is_none_or(|c| c == e.connection_id))
            .filter(move |e| q.is_empty() || e.sql.to_lowercase().contains(&q))
    }

    pub fn clear(&mut self) -> Result<()> {
        self.entries.clear();
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DriverKind;

    #[test]
    fn connections_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::load(dir.path()).unwrap();
        assert!(store.connections.is_empty());
        let mut c = ConnectionConfig::new(DriverKind::Postgres);
        c.name = "Local PG".into();
        store.upsert(c.clone()).unwrap();
        c.port = 6543;
        store.upsert(c.clone()).unwrap();
        let copy = store.duplicate(c.id).unwrap().unwrap();
        let reloaded = ConnectionStore::load(dir.path()).unwrap();
        assert_eq!(reloaded.connections.len(), 2);
        assert_eq!(reloaded.get(c.id).unwrap().port, 6543);
        assert_eq!(reloaded.get(copy.id).unwrap().name, "Local PG copy");
        let mut reloaded = reloaded;
        reloaded.remove(c.id).unwrap();
        assert_eq!(ConnectionStore::load(dir.path()).unwrap().connections.len(), 1);
    }

    #[test]
    fn history_append_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut h = History::load(dir.path());
        for sql in ["select 1", "SELECT * FROM users", "update t set a = 1"] {
            h.push(HistoryEntry {
                connection_id: id,
                connection_name: "x".into(),
                sql: sql.into(),
                executed_at: Utc::now(),
                duration_ms: 3,
                rows: Some(1),
                error: None,
            })
            .unwrap();
        }
        let h = History::load(dir.path());
        assert_eq!(h.entries.len(), 3);
        let found: Vec<_> = h.search(Some(id), "select").map(|e| e.sql.as_str()).collect();
        assert_eq!(found, vec!["SELECT * FROM users", "select 1"]);
        assert_eq!(h.search(Some(Uuid::new_v4()), "").count(), 0);
    }
}
