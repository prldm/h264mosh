//! What the app remembers between runs: recent projects and choices. Stored as JSON in
//! the user's configuration folder.

use crate::export::ExportSettings;
use crate::import::ImportSettings;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 10;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// most recent first
    pub recent: Vec<PathBuf>,
    pub auto_render: bool,
    pub export: ExportSettings,
    /// how newly imported clips are encoded
    pub import: ImportSettings,
    /// newly imported clips keep their coding (no re-encoding; experimental)
    pub import_as_coded: bool,
    /// deleted frames are replaced by repeats of the next one
    pub delete_keeps_length: bool,
    /// no sound while playing
    pub mute: bool,
    /// where it is stored; None keeps it in memory only (tests)
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            recent: Vec::new(),
            auto_render: false,
            export: ExportSettings::default(),
            import: ImportSettings::default(),
            import_as_coded: false,
            delete_keeps_length: true,
            mute: false,
            path: None,
        }
    }
}

/// The user's configuration folder for h264mosh.
fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::home_dir().map(|h| h.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
    };
    Some(base?.join("h264mosh"))
}

impl Settings {
    /// The stored settings, or the defaults.
    pub fn load() -> Self {
        let path = config_dir().map(|d| d.join("settings.json"));
        let mut s: Settings =
            path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        s.path = path;
        s
    }

    /// Settings that are not stored.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Store them; failures only cost the remembered choices.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let (Some(dir), Ok(json)) = (path.parent(), serde_json::to_string_pretty(self)) {
            let _ = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(path, json));
        }
    }

    pub fn add_recent(&mut self, file: &Path) {
        self.recent.retain(|p| p != file);
        self.recent.insert(0, file.to_path_buf());
        self.recent.truncate(MAX_RECENT);
        self.save();
    }
}
