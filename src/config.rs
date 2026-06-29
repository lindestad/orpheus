use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::devices::PollingRate;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PollMonitorConfig {
    pub default_rate: PollingRate,
    pub restore_rate: Option<PollingRate>,
    pub scan_interval_ms: u64,
    pub power_policy: PowerPolicy,
    pub battery_trend_window_ms: u64,
    pub battery_trend_min_delta: u8,
    pub assume_wired_is_charging: bool,
    pub assume_wireless_is_discharging: bool,
    pub allow_unknown_power_active: bool,
    pub rules: Vec<AppRule>,
}

impl Default for PollMonitorConfig {
    fn default() -> Self {
        Self {
            default_rate: PollingRate::Hz8000,
            restore_rate: Some(PollingRate::Hz8000),
            scan_interval_ms: 1_000,
            power_policy: PowerPolicy::FirstDevice,
            battery_trend_window_ms: 600_000,
            battery_trend_min_delta: 1,
            assume_wired_is_charging: false,
            assume_wireless_is_discharging: false,
            allow_unknown_power_active: true,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerPolicy {
    #[default]
    FirstDevice,
    ActiveNonCharging,
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
default_rate = 1000
restore_rate = 1000
scan_interval_ms = 1000
power_policy = "active-non-charging"
battery_trend_window_ms = 600000
battery_trend_min_delta = 1
assume_wired_is_charging = true
assume_wireless_is_discharging = false
allow_unknown_power_active = true

[[rules]]
exe = "problem-game.exe"
rate = 4000
restore = 1000
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
power_policy = "first-device"

[[rules]]
exe = "game.exe"
rate = 1000
"#,
        )
        .unwrap();

        assert_eq!(config.default_rate, PollingRate::Hz8000);
        assert_eq!(config.restore_rate, Some(PollingRate::Hz4000));
        assert_eq!(config.power_policy, PowerPolicy::FirstDevice);
        assert_eq!(config.rules[0].rate, PollingRate::Hz1000);
    }

    #[test]
    fn parses_power_policy() {
        let config: PollMonitorConfig = toml::from_str(
            r#"
power_policy = "active-non-charging"
battery_trend_window_ms = 600000
battery_trend_min_delta = 1
assume_wired_is_charging = true
assume_wireless_is_discharging = true

[[rules]]
exe = "game.exe"
rate = "4k"
"#,
        )
        .unwrap();

        assert_eq!(config.power_policy, PowerPolicy::ActiveNonCharging);
        assert_eq!(config.battery_trend_window_ms, 600_000);
        assert_eq!(config.battery_trend_min_delta, 1);
        assert!(config.assume_wired_is_charging);
        assert!(config.assume_wireless_is_discharging);
        assert_eq!(config.rules[0].rate, PollingRate::Hz4000);
    }
}
