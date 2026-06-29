use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::gwolves::PollingRate;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PollMonitorConfig {
    pub default_rate: PollingRate,
    pub restore_rate: Option<PollingRate>,
    pub scan_interval_ms: u64,
    pub rules: Vec<AppRule>,
}

impl Default for PollMonitorConfig {
    fn default() -> Self {
        Self {
            default_rate: PollingRate::Hz8000,
            restore_rate: Some(PollingRate::Hz8000),
            scan_interval_ms: 1_000,
            rules: Vec::new(),
        }
    }
}

impl PollMonitorConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("failed to parse config {}", path.display()))
    }

    pub fn write_example(path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, EXAMPLE_CONFIG)
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppRule {
    pub exe: String,
    pub rate: PollingRate,
    pub restore: Option<PollingRate>,
}

pub fn default_config_path() -> PathBuf {
    PathBuf::from("poll-monitor.toml")
}

pub const EXAMPLE_CONFIG: &str = r#"# poll-monitor config
default_rate = 8000
restore_rate = 8000
scan_interval_ms = 1000

[[rules]]
exe = "problem-game.exe"
rate = 1000
restore = 8000
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_rates() {
        let config: PollMonitorConfig = toml::from_str(
            r#"
default_rate = 8000
restore_rate = "4k"
scan_interval_ms = 500

[[rules]]
exe = "game.exe"
rate = 1000
"#,
        )
        .unwrap();

        assert_eq!(config.default_rate, PollingRate::Hz8000);
        assert_eq!(config.restore_rate, Some(PollingRate::Hz4000));
        assert_eq!(config.rules[0].rate, PollingRate::Hz1000);
    }
}
