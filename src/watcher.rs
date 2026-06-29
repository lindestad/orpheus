use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    config::{PollMonitorConfig, PowerPolicy},
    devices::{ConnectionKind, PollingRate},
    hid_device::{DeviceSnapshot, DeviceSnapshotCache, HidPollMonitor},
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
    let mut battery_trends = BatteryTrendTracker::from_config(&config);
    let mut snapshot_cache = DeviceSnapshotCache::default();

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
                    &mut battery_trends,
                    &mut snapshot_cache,
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DeviceKey {
    path: String,
    vid: u16,
    pid: u16,
}

impl DeviceKey {
    fn from_snapshot(device: &DeviceSnapshot) -> Self {
        Self {
            path: device.path.clone(),
            vid: device.vid,
            pid: device.pid,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct BatterySample {
    at: Instant,
    level: u8,
}

#[derive(Debug)]
struct BatteryTrendTracker {
    histories: HashMap<DeviceKey, VecDeque<BatterySample>>,
    window: Duration,
    min_delta: u8,
}

impl BatteryTrendTracker {
    fn from_config(config: &PollMonitorConfig) -> Self {
        Self {
            histories: HashMap::new(),
            window: Duration::from_millis(config.battery_trend_window_ms.max(1_000)),
            min_delta: config.battery_trend_min_delta.max(1),
        }
    }

    fn record(&mut self, devices: &[DeviceSnapshot]) {
        self.record_at(devices, Instant::now());
    }

    fn record_at(&mut self, devices: &[DeviceSnapshot], now: Instant) {
        let mut visible = HashSet::new();
        for device in devices {
            let key = DeviceKey::from_snapshot(device);
            visible.insert(key.clone());
            let Some(level) = device.battery.and_then(|battery| battery.level_percent) else {
                continue;
            };
            let history = self.histories.entry(key).or_default();
            history.push_back(BatterySample { at: now, level });
            while history
                .front()
                .is_some_and(|sample| now.duration_since(sample.at) > self.window)
            {
                history.pop_front();
            }
        }
        self.histories.retain(|key, _| visible.contains(key));
    }

    fn classify(&self, device: &DeviceSnapshot) -> Option<PowerClass> {
        let history = self.histories.get(&DeviceKey::from_snapshot(device))?;
        let first = history.front()?;
        let last = history.back()?;
        if history.len() < 2 {
            return None;
        }

        if last.level >= first.level.saturating_add(self.min_delta) {
            return Some(PowerClass::Charging);
        }
        if first.level >= last.level.saturating_add(self.min_delta) {
            return Some(PowerClass::Discharging);
        }
        None
    }
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
    battery_trends: &mut BatteryTrendTracker,
    snapshot_cache: &mut DeviceSnapshotCache,
    active: Option<&ActiveRule>,
    desired: &DesiredRate,
) -> Result<()> {
    let monitor = monitor.expect("power-aware watcher must have a monitor");
    let mut devices = monitor.scan()?;
    snapshot_cache.apply(&mut devices);
    if devices.is_empty() {
        if dry_run {
            println!("dry run: no supported device visible for {desired}");
            return Ok(());
        }
        return Ok(());
    }
    battery_trends.record(&devices);

    let idle_rate = active
        .and_then(|rule| rule.restore)
        .or(config.restore_rate)
        .unwrap_or(config.default_rate);
    let classes = devices
        .iter()
        .map(|device| classify_power(device, config, battery_trends))
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

fn classify_power(
    device: &DeviceSnapshot,
    config: &PollMonitorConfig,
    battery_trends: &BatteryTrendTracker,
) -> PowerClass {
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

    if device.battery.is_some()
        && device
            .battery
            .is_some_and(|battery| battery.is_charging_like().is_none())
    {
        if device.battery.and_then(|battery| battery.level_percent) == Some(100) {
            return PowerClass::Charging;
        }
        if let Some(class) = battery_trends.classify(device) {
            return class;
        }
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

    if snapshot.cached_rate {
        if dry_run {
            println!(
                "dry run: would defer setting {} {} {:04x}:{:04x} {} -> {} ({reason}, cached report, battery {})",
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
        }
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
    use crate::{
        config::AppRule,
        devices::{BatteryStatus, ChargeState, GWOLVES_VENDOR_ID, IPI_VENDOR_ID, ProtocolKind},
    };

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

    #[test]
    fn battery_trend_marks_unknown_state_charging() {
        let config = PollMonitorConfig {
            battery_trend_window_ms: 600_000,
            battery_trend_min_delta: 1,
            assume_wireless_is_discharging: false,
            ..PollMonitorConfig::default()
        };
        let now = Instant::now();
        let mut tracker = BatteryTrendTracker::from_config(&config);
        let mut device = test_device(
            IPI_VENDOR_ID,
            0x1014,
            ProtocolKind::IpiPixV1 { report_id: 3 },
            BatteryStatus::level_only(80),
        );
        tracker.record_at(&[device.clone()], now);
        device.battery = Some(BatteryStatus::level_only(81));
        tracker.record_at(&[device.clone()], now + Duration::from_secs(60));

        assert_eq!(
            classify_power(&device, &config, &tracker),
            PowerClass::Charging
        );
    }

    #[test]
    fn explicit_charge_state_wins_over_battery_trend() {
        let config = PollMonitorConfig {
            battery_trend_window_ms: 600_000,
            battery_trend_min_delta: 1,
            ..PollMonitorConfig::default()
        };
        let now = Instant::now();
        let mut tracker = BatteryTrendTracker::from_config(&config);
        let mut device = test_device(
            GWOLVES_VENDOR_ID,
            0x3517,
            ProtocolKind::Feature64 {
                new_protocol: false,
                wired_device_id: 2,
            },
            BatteryStatus::with_raw_state(75, ChargeState::Discharging, 0),
        );
        tracker.record_at(&[device.clone()], now);
        device.battery = Some(BatteryStatus::with_raw_state(
            76,
            ChargeState::Discharging,
            0,
        ));
        tracker.record_at(&[device.clone()], now + Duration::from_secs(60));

        assert_eq!(
            classify_power(&device, &config, &tracker),
            PowerClass::Discharging
        );
    }

    #[test]
    fn flat_unknown_battery_stays_unknown() {
        let config = PollMonitorConfig {
            battery_trend_window_ms: 600_000,
            battery_trend_min_delta: 1,
            assume_wireless_is_discharging: false,
            ..PollMonitorConfig::default()
        };
        let now = Instant::now();
        let mut tracker = BatteryTrendTracker::from_config(&config);
        let device = test_device(
            IPI_VENDOR_ID,
            0x1014,
            ProtocolKind::IpiPixV1 { report_id: 3 },
            BatteryStatus::level_only(80),
        );
        tracker.record_at(&[device.clone()], now);
        tracker.record_at(&[device.clone()], now + Duration::from_secs(60));

        assert_eq!(
            classify_power(&device, &config, &tracker),
            PowerClass::Unknown
        );
    }

    #[test]
    fn full_unknown_battery_is_treated_as_charging() {
        let config = PollMonitorConfig {
            assume_wireless_is_discharging: false,
            ..PollMonitorConfig::default()
        };
        let tracker = BatteryTrendTracker::from_config(&config);
        let device = test_device(
            IPI_VENDOR_ID,
            0x1014,
            ProtocolKind::IpiPixV1 { report_id: 3 },
            BatteryStatus::level_only(100),
        );

        assert_eq!(
            classify_power(&device, &config, &tracker),
            PowerClass::Charging
        );
    }

    #[test]
    fn explicit_discharging_wins_over_full_battery_assumption() {
        let config = PollMonitorConfig::default();
        let tracker = BatteryTrendTracker::from_config(&config);
        let device = test_device(
            GWOLVES_VENDOR_ID,
            0x3517,
            ProtocolKind::Feature64 {
                new_protocol: false,
                wired_device_id: 2,
            },
            BatteryStatus::with_raw_state(100, ChargeState::Discharging, 0),
        );

        assert_eq!(
            classify_power(&device, &config, &tracker),
            PowerClass::Discharging
        );
    }

    fn test_device(
        vid: u16,
        pid: u16,
        protocol: ProtocolKind,
        battery: BatteryStatus,
    ) -> DeviceSnapshot {
        DeviceSnapshot {
            path: format!("test-{vid:04x}-{pid:04x}"),
            vid,
            pid,
            product_name: None,
            vendor_name: "Test",
            model_name: "Mouse",
            connection: ConnectionKind::Wireless,
            protocol,
            supported_rates: vec![PollingRate::Hz1000, PollingRate::Hz4000],
            current_rate: Some(PollingRate::Hz1000),
            battery: Some(battery),
            cached_rate: false,
            cached_battery: false,
            battery_error: None,
            read_error: None,
        }
    }
}
