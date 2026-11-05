//! Engines that run in a separate helper program instead of inside the app, and where the app
//! finds each helper.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::config::ConnectionConfig;
use crate::duckdb::DuckDbTarget;

pub struct HelperEngine {
    /// Identifies the engine in the handshake, for example `duckdb`.
    pub name: &'static str,
    /// For people, for example `DuckDB`.
    pub label: &'static str,
    /// The helper's file name.
    pub binary: &'static str,
    /// Overrides where the helper is, for development and tests.
    pub env_var: &'static str,
    /// The engine version the helper was released with; installs go into a folder named after it.
    pub version: &'static str,
    /// The installed helper's size, shown before it is installed.
    pub size_label: &'static str,
    /// The file a connection holds open, with its access mode, when sessions on it must share
    /// one helper process. `None` gives the connection a helper of its own.
    pub shared_file: fn(&ConnectionConfig) -> Option<(PathBuf, bool)>,
}

pub static DUCKDB: HelperEngine = HelperEngine {
    name: "duckdb",
    label: "DuckDB",
    binary: "tableplus-duckdb",
    env_var: "TABLEPLUS_DUCKDB_HELPER",
    version: "1.5.6",
    size_label: "36 MB",
    shared_file: duckdb_shared_file,
};

fn duckdb_shared_file(config: &ConnectionConfig) -> Option<(PathBuf, bool)> {
    match DuckDbTarget::of(config).ok()? {
        DuckDbTarget::Database(path) => {
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            Some((path, config.read_only))
        }
        DuckDbTarget::Memory | DuckDbTarget::DataFile { .. } => None,
    }
}

/// Where the installed copy of an engine's helper lives.
pub fn install_dir(engine: &HelperEngine) -> PathBuf {
    crate::store::config_dir().join("engines").join(engine.name).join(engine.version)
}

/// Whether the app can start the engine's helper.
pub fn is_available(engine: &HelperEngine) -> bool {
    locate(engine).is_ok()
}

/// Finds the helper: the path in its environment variable, then a binary next to the app's own
/// executable (where `cargo build` puts it), then the installed copy.
pub fn locate(engine: &HelperEngine) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(engine.env_var) {
        return Ok(PathBuf::from(path));
    }
    let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join(engine.binary)));
    if let Some(path) = beside.filter(|p| p.is_file()) {
        return Ok(path);
    }
    let installed = install_dir(engine).join(engine.binary);
    if installed.is_file() {
        return Ok(installed);
    }
    bail!("{} support isn't installed.", engine.label)
}
