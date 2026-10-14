//! User preferences, persisted as `settings.json` in the config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::ThemeChoice;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Rows per page in the table browser.
    pub page_size: usize,
    /// Maximum rows kept from one SQL editor statement (0 = unlimited).
    pub query_row_limit: usize,
    pub editor_font_size: f32,
    /// Width of the sidebar in points.
    pub sidebar_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            page_size: 300,
            query_row_limit: 100_000,
            editor_font_size: 13.0,
            sidebar_width: 250.0,
        }
    }
}

impl Settings {
    fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    pub fn load(dir: &Path) -> Self {
        std::fs::read(Self::path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(Self::path(dir), bytes);
        }
    }

    pub fn row_limit(&self) -> Option<usize> {
        (self.query_row_limit > 0).then_some(self.query_row_limit)
    }
}
