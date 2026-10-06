use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct GuiSettings {
    pub start_minimized: bool,
    pub minimize_to_tray: bool,
    pub close_to_tray: bool,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            start_minimized: false,
            minimize_to_tray: true,
            close_to_tray: true,
        }
    }
}

impl GuiSettings {
    pub fn path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("user settings directory unavailable")?
            .join("Orpheus")
            .join("gui.toml"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).context("invalid GUI settings"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).context("failed to read GUI settings"),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("settings path has no parent directory")?;
        std::fs::create_dir_all(parent).context("failed to create settings directory")?;
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, toml::to_string_pretty(self)?)
            .context("failed to write GUI settings")?;
        std::fs::rename(&temp, path).context("failed to save GUI settings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_settings_keep_defaults_and_invalid_settings_report_errors() {
        let partial: GuiSettings = toml::from_str("start_minimized = true").unwrap();
        assert!(partial.start_minimized && partial.close_to_tray && partial.minimize_to_tray);
        assert!(toml::from_str::<GuiSettings>("start_minimized = 'yes'").is_err());
    }

    #[test]
    fn settings_roundtrip_and_replace_previous_values() {
        let dir = std::env::temp_dir().join(format!("orpheus-settings-{}", std::process::id()));
        let path = dir.join("gui.toml");
        assert_eq!(GuiSettings::load(&path).unwrap(), GuiSettings::default());
        let mut settings = GuiSettings {
            start_minimized: true,
            ..Default::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(GuiSettings::load(&path).unwrap(), settings);
        settings.close_to_tray = false;
        settings.save(&path).unwrap();
        assert_eq!(GuiSettings::load(&path).unwrap(), settings);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
