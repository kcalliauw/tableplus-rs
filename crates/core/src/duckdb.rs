//! What a DuckDB connection's file path points at. Shared by the app, which decides which
//! sessions share a helper process, and by the helper, which opens the database.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::ConnectionConfig;

/// The path that opens a database that lives in memory only.
pub const MEMORY: &str = ":memory:";

/// File extensions that open as a view over the file instead of as a database.
pub const DATA_FILE_EXTENSIONS: &[&str] = &["parquet", "csv", "tsv", "json", "jsonl", "ndjson"];

/// File extensions of DuckDB database files.
pub const DATABASE_EXTENSIONS: &[&str] = &["duckdb", "ddb", "db"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuckDbTarget {
    Memory,
    Database(PathBuf),
    /// A Parquet, CSV or JSON file, or a glob over several, read through a view in an in-memory
    /// database.
    DataFile {
        path: PathBuf,
        reader: DataReader,
        view: String,
    },
}

/// The DuckDB table function that reads a data file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataReader {
    Parquet,
    Csv,
    Json,
}

impl DataReader {
    pub fn function(self) -> &'static str {
        match self {
            DataReader::Parquet => "read_parquet",
            DataReader::Csv => "read_csv",
            DataReader::Json => "read_json",
        }
    }

    fn of_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "parquet" => Some(DataReader::Parquet),
            "csv" | "tsv" => Some(DataReader::Csv),
            "json" | "jsonl" | "ndjson" => Some(DataReader::Json),
            _ => None,
        }
    }
}

impl DuckDbTarget {
    pub fn of(config: &ConnectionConfig) -> Result<Self> {
        let raw = config.path.trim();
        if raw.is_empty() {
            bail!("No DuckDB database file selected");
        }
        if raw == MEMORY {
            return Ok(DuckDbTarget::Memory);
        }
        let path = crate::tunnel::expand_tilde(raw);
        let reader = path.extension().and_then(|e| e.to_str()).and_then(DataReader::of_extension);
        Ok(match reader {
            Some(reader) => DuckDbTarget::DataFile { view: view_name(&path), path, reader },
            None => DuckDbTarget::Database(path),
        })
    }

    /// Data files and globs are only read.
    pub fn is_read_only(&self, config: &ConnectionConfig) -> bool {
        matches!(self, DuckDbTarget::DataFile { .. }) || (config.read_only && !matches!(self, DuckDbTarget::Memory))
    }
}

/// The view's name: the file name without its extension, or the folder's name for a glob such
/// as `/data/orders/*.parquet`.
fn view_name(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.contains(['*', '?', '[']) {
        if let Some(dir) = path.parent().and_then(|d| d.file_name()) {
            return dir.to_string_lossy().into_owned();
        }
        return "data".into();
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DriverKind;

    fn target(path: &str) -> DuckDbTarget {
        let mut c = ConnectionConfig::new(DriverKind::DuckDb);
        c.path = path.into();
        DuckDbTarget::of(&c).unwrap()
    }

    #[test]
    fn classifies_paths() {
        assert_eq!(target(":memory:"), DuckDbTarget::Memory);
        assert_eq!(target("/d/shop.duckdb"), DuckDbTarget::Database("/d/shop.duckdb".into()));
        assert_eq!(target("/d/shop"), DuckDbTarget::Database("/d/shop".into()));
        assert_eq!(
            target("/d/orders.parquet"),
            DuckDbTarget::DataFile {
                path: "/d/orders.parquet".into(),
                reader: DataReader::Parquet,
                view: "orders".into()
            }
        );
        assert_eq!(
            target("/d/events/*.JSONL"),
            DuckDbTarget::DataFile {
                path: "/d/events/*.JSONL".into(),
                reader: DataReader::Json,
                view: "events".into()
            }
        );
        let DuckDbTarget::DataFile { reader, view, .. } = target("/d/My Sales 2024.csv") else { panic!() };
        assert_eq!((reader, view.as_str()), (DataReader::Csv, "My Sales 2024"));
        let mut c = ConnectionConfig::new(DriverKind::DuckDb);
        assert!(DuckDbTarget::of(&c).is_err(), "an empty path is refused");
        c.path = "/d/x.csv".into();
        assert!(DuckDbTarget::of(&c).unwrap().is_read_only(&c));
    }
}
