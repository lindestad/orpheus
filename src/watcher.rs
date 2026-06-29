use std::{
    fmt, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    config::{PollMonitorConfig, PowerPolicy},
    devices::{ConnectionKind, PollingRate},
    hid_device::{DeviceSnapshot, HidPollMonitor},
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
    let needs_monitor = !dry_run || config.power_policy == PowerPolicy::ActiveNonCharging;
    let monitor = if needs_monitor {
        Some(HidPollMonitor::new()?)
    } else {
        None
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

        match config.power_policy {
            PowerPolicy::FirstDevice => {
                if last_desired.as_ref() != Some(&desired) {
                    apply_desired(monitor.as_ref(), dry_run, &desired)
                        .with_context(|| format!("failed while applying {desired}"))?;
                    last_desired = Some(desired);
                }
            }
            PowerPolicy::ActiveNonCharging => {
                apply_power_policy(
                    monitor.as_ref(),
                    dry_run,
                    &config,
                    active.as_ref(),
                    &desired,
                )
                .with_context(|| format!("failed while applying power policy for {desired}"))?;
                last_desired = Some(desired);
            }
        }

        if once {
            break;
        }

        thread::sleep(interval);
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PowerClass {
    Charging,
    Discharging,
    Unknown,
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

fn apply_power_policy(
    monitor: Option<&HidPollMonitor>,
    dry_run: bool,
    config: &PollMonitorConfig,
    active: Option<&ActiveRule>,
    desired: &DesiredRate,
) -> Result<()> {
    let monitor = monitor.expect("power-aware watcher must have a monitor");
    let devices = monitor.scan()?;
    if devices.is_empty() {
        if dry_run {
            println!("dry run: no supported device visible for {desired}");
            return Ok(());
        }
        return Ok(());
    }

    let idle_rate = active
        .and_then(|rule| rule.restore)
        .or(config.restore_rate)
        .unwrap_or(config.default_rate);
    let classes = devices
        .iter()
        .map(|device| classify_power(device, config))
        .collect::<Vec<_>>();
    let has_known_discharging = classes.contains(&PowerClass::Discharging);

    for (device, class) in devices.iter().zip(classes) {
        if device.current_rate.is_none() {
            continue;
        }
        let (target, reason) = power_target_rate(
            active,
            desired.rate,
            idle_rate,
            class,
            has_known_discharging,
            config.allow_unknown_power_active,
        );
        apply_snapshot_rate(monitor, dry_run, device, target, reason)?;
    }

    Ok(())
}

fn classify_power(device: &DeviceSnapshot, config: &PollMonitorConfig) -> PowerClass {
    if let Some(charging_like) = device
        .battery
        .and_then(|battery| battery.is_charging_like())
    {
        return if charging_like {
            PowerClass::Charging
        } else {
            PowerClass::Discharging
        };
    }

    match device.connection {
        ConnectionKind::Wired if config.assume_wired_is_charging => PowerClass::Charging,
        ConnectionKind::Wireless | ConnectionKind::Receiver
            if config.assume_wireless_is_discharging =>
        {
            PowerClass::Discharging
        }
        _ => PowerClass::Unknown,
    }
}

fn power_target_rate(
    active: Option<&ActiveRule>,
    active_rate: PollingRate,
    idle_rate: PollingRate,
    class: PowerClass,
    has_known_discharging: bool,
    allow_unknown_active: bool,
) -> (PollingRate, &'static str) {
    if active.is_none() {
        return (idle_rate, "idle");
    }

    match class {
        PowerClass::Discharging => (active_rate, "active non-charging"),
        PowerClass::Charging => (idle_rate, "charging idle"),
        PowerClass::Unknown if !has_known_discharging && allow_unknown_active => {
            (active_rate, "active unknown-power fallback")
        }
        PowerClass::Unknown => (idle_rate, "unknown-power idle"),
    }
}

fn apply_snapshot_rate(
    monitor: &HidPollMonitor,
    dry_run: bool,
    snapshot: &DeviceSnapshot,
    target: PollingRate,
    reason: &str,
) -> Result<()> {
    if !snapshot.supported_rates.contains(&target) {
        println!(
            "{} {} {:04x}:{:04x}: skipping unsupported target {} ({reason})",
            snapshot.vendor_name, snapshot.model_name, snapshot.vid, snapshot.pid, target
        );
        return Ok(());
    }

    if snapshot.current_rate == Some(target) {
        return Ok(());
    }

    if dry_run {
        println!(
            "dry run: would set {} {} {:04x}:{:04x} {} -> {} ({reason}, battery {})",
            snapshot.vendor_name,
            snapshot.model_name,
            snapshot.vid,
            snapshot.pid,
            snapshot
                .current_rate
                .map(|rate| rate.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            target,
            snapshot.battery_text()
        );
        return Ok(());
    }

    let started = Instant::now();
    let device = monitor.open_by_vid_pid(snapshot.vid, snapshot.pid)?;
    let before = device.read_rate().ok().or(snapshot.current_rate);
    if before != Some(target) {
        device.set_rate(target)?;
        thread::sleep(Duration::from_millis(80));
    }
    let after = device.read_rate().unwrap_or(target);

    println!(
        "{} {} {:04x}:{:04x}: {} -> {} ({reason}, battery {}, {} ms)",
        snapshot.vendor_name,
        snapshot.model_name,
        snapshot.vid,
        snapshot.pid,
        before
            .map(|rate| rate.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        after,
        snapshot.battery_text(),
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

    #[test]
    fn active_power_policy_targets_discharging_device() {
        let active = ActiveRule {
            exe: "game.exe".to_string(),
            rate: PollingRate::Hz4000,
            restore: Some(PollingRate::Hz1000),
        };

        assert_eq!(
            power_target_rate(
                Some(&active),
                PollingRate::Hz4000,
                PollingRate::Hz1000,
                PowerClass::Discharging,
                true,
                true,
            ),
            (PollingRate::Hz4000, "active non-charging")
        );
        assert_eq!(
            power_target_rate(
                Some(&active),
                PollingRate::Hz4000,
                PollingRate::Hz1000,
                PowerClass::Charging,
                true,
                true,
            ),
            (PollingRate::Hz1000, "charging idle")
        );
    }

    #[test]
    fn active_power_policy_uses_unknown_only_without_known_discharging() {
        let active = ActiveRule {
            exe: "game.exe".to_string(),
            rate: PollingRate::Hz4000,
            restore: Some(PollingRate::Hz1000),
        };

        assert_eq!(
            power_target_rate(
                Some(&active),
                PollingRate::Hz4000,
                PollingRate::Hz1000,
                PowerClass::Unknown,
                false,
                true,
            ),
            (PollingRate::Hz4000, "active unknown-power fallback")
        );
        assert_eq!(
            power_target_rate(
                Some(&active),
                PollingRate::Hz4000,
                PollingRate::Hz1000,
                PowerClass::Unknown,
                true,
                true,
            ),
            (PollingRate::Hz1000, "unknown-power idle")
        );
    }
}
