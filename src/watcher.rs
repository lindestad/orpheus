use std::{
    fmt, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    config::PollMonitorConfig,
    devices::PollingRate,
    hid_device::HidPollMonitor,
    process_rules::{ActiveRule, ProcessScanner},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredRate {
    pub rate: PollingRate,
    pub reason: DesiredReason,
}

impl fmt::Display for DesiredRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            DesiredReason::Rule { exe } => write!(f, "{exe} active -> {}", self.rate),
            DesiredReason::Restore => write!(f, "restore -> {}", self.rate),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesiredReason {
    Rule { exe: String },
    Restore,
}

pub fn desired_rate(
    config: &PollMonitorConfig,
    active: Option<&ActiveRule>,
    pending_restore: Option<PollingRate>,
) -> DesiredRate {
    if let Some(rule) = active {
        return DesiredRate {
            rate: rule.rate,
            reason: DesiredReason::Rule {
                exe: rule.exe.clone(),
            },
        };
    }

    DesiredRate {
        rate: pending_restore
            .or(config.restore_rate)
            .unwrap_or(config.default_rate),
        reason: DesiredReason::Restore,
    }
}

pub fn run_watch(config: PollMonitorConfig, dry_run: bool, once: bool) -> Result<()> {
    let interval = Duration::from_millis(config.scan_interval_ms.max(250));
    let monitor = if dry_run {
        None
    } else {
        Some(HidPollMonitor::new()?)
    };
    let mut scanner = ProcessScanner::new();
    let mut last_desired = None;
    let mut pending_restore = None;

    loop {
        let active = scanner.active_rule(&config.rules);
        if let Some(rule) = active.as_ref() {
            pending_restore = rule.restore;
        }
        let desired = desired_rate(&config, active.as_ref(), pending_restore);

        if last_desired.as_ref() != Some(&desired) {
            apply_desired(monitor.as_ref(), dry_run, &desired)
                .with_context(|| format!("failed while applying {desired}"))?;
            last_desired = Some(desired);
        }

        if once {
            break;
        }

        thread::sleep(interval);
    }

    Ok(())
}

fn apply_desired(
    monitor: Option<&HidPollMonitor>,
    dry_run: bool,
    desired: &DesiredRate,
) -> Result<()> {
    if dry_run {
        println!("dry run: would set {desired}");
        return Ok(());
    }

    let monitor = monitor.expect("non-dry-run watcher must have a monitor");
    let started = Instant::now();
    let device = monitor.open_first_supported()?;
    let before = device.read_rate().ok();
    if before != Some(desired.rate) {
        device.set_rate(desired.rate)?;
        thread::sleep(Duration::from_millis(80));
    }
    let after = device.read_rate().unwrap_or(desired.rate);

    println!(
        "{} {}: {} -> {} ({desired}, {} ms)",
        device.model().name,
        device.connection(),
        before
            .map(|rate| rate.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        after,
        started.elapsed().as_millis()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppRule;

    #[test]
    fn uses_active_rule_before_restore() {
        let config = PollMonitorConfig {
            restore_rate: Some(PollingRate::Hz8000),
            rules: vec![AppRule {
                exe: "game.exe".to_string(),
                rate: PollingRate::Hz1000,
                restore: Some(PollingRate::Hz4000),
            }],
            ..PollMonitorConfig::default()
        };
        let active = ActiveRule {
            exe: "game.exe".to_string(),
            rate: PollingRate::Hz1000,
            restore: Some(PollingRate::Hz4000),
        };

        assert_eq!(
            desired_rate(&config, Some(&active), None),
            DesiredRate {
                rate: PollingRate::Hz1000,
                reason: DesiredReason::Rule {
                    exe: "game.exe".to_string()
                },
            }
        );
    }

    #[test]
    fn falls_back_to_restore_or_default() {
        let config = PollMonitorConfig {
            default_rate: PollingRate::Hz4000,
            restore_rate: None,
            ..PollMonitorConfig::default()
        };

        assert_eq!(
            desired_rate(&config, None, None),
            DesiredRate {
                rate: PollingRate::Hz4000,
                reason: DesiredReason::Restore,
            }
        );
    }

    #[test]
    fn prefers_pending_rule_restore() {
        let config = PollMonitorConfig {
            default_rate: PollingRate::Hz8000,
            restore_rate: Some(PollingRate::Hz8000),
            ..PollMonitorConfig::default()
        };

        assert_eq!(
            desired_rate(&config, None, Some(PollingRate::Hz4000)),
            DesiredRate {
                rate: PollingRate::Hz4000,
                reason: DesiredReason::Restore,
            }
        );
    }
}
