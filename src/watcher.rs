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
    let process_interval = Duration::from_millis(config.scan_interval_ms.max(250));
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
    let mut device_scheduler = DevicePollScheduler::from_config(&config);
    let mut pending_rate_changes = PendingRateQueue::default();
    let mut pending_first_device = None;

    loop {
        let now = Instant::now();
        let active = scanner.active_rule(&config.rules);
        if let Some(rule) = active.as_ref() {
            pending_restore = rule.restore;
        }
        let desired = desired_rate(&config, active.as_ref(), pending_restore);
        let desired_changed = last_desired.as_ref() != Some(&desired);

        match config.power_policy {
            PowerPolicy::FirstDevice => {
                if desired_changed {
                    pending_first_device = Some(desired.clone());
                }
                if let Some(pending) = pending_first_device.as_ref() {
                    let should_try = once
                        || desired_changed
                        || device_scheduler.should_poll(now, active.is_some(), true, false);
                    if should_try {
                        match apply_desired(monitor.as_ref(), dry_run, pending) {
                            Ok(()) => pending_first_device = None,
                            Err(err) => println!("queued {pending}: {err}"),
                        }
                        device_scheduler.record_poll(now);
                    }
                }
            }
            PowerPolicy::ActiveNonCharging => {
                let should_poll_devices = once
                    || device_scheduler.should_poll(
                        now,
                        active.is_some(),
                        pending_rate_changes.has_pending(),
                        desired_changed,
                    );
                if should_poll_devices {
                    let mut state = PowerPolicyState {
                        battery_trends: &mut battery_trends,
                        snapshot_cache: &mut snapshot_cache,
                        pending_changes: &mut pending_rate_changes,
                    };
                    apply_power_policy(
                        monitor.as_ref(),
                        dry_run,
                        &config,
                        &mut state,
                        active.as_ref(),
                        &desired,
                    )
                    .with_context(|| format!("failed while applying power policy for {desired}"))?;
                    device_scheduler.record_poll(now);
                }
            }
        }
        last_desired = Some(desired);

        if once {
            break;
        }

        let has_pending = pending_first_device.is_some() || pending_rate_changes.has_pending();
        thread::sleep(device_scheduler.sleep_interval(process_interval, has_pending));
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PowerClass {
    Charging,
    Discharging,
    Unknown,
}

#[derive(Debug)]
struct DevicePollScheduler {
    pending_retry_interval: Duration,
    active_poll_interval: Duration,
    background_poll_interval: Duration,
    last_poll: Option<Instant>,
}

impl DevicePollScheduler {
    fn from_config(config: &PollMonitorConfig) -> Self {
        Self {
            pending_retry_interval: Duration::from_millis(
                config.pending_retry_interval_ms.max(250),
            ),
            active_poll_interval: Duration::from_millis(
                config.active_device_poll_interval_ms.max(1_000),
            ),
            background_poll_interval: Duration::from_millis(
                config.background_device_poll_interval_ms.max(60_000),
            ),
            last_poll: None,
        }
    }

    fn should_poll(
        &self,
        now: Instant,
        active: bool,
        has_pending: bool,
        desired_changed: bool,
    ) -> bool {
        if desired_changed {
            return true;
        }

        let Some(last_poll) = self.last_poll else {
            return true;
        };

        now.duration_since(last_poll) >= self.interval_for(active, has_pending)
    }

    fn record_poll(&mut self, now: Instant) {
        self.last_poll = Some(now);
    }

    fn sleep_interval(&self, process_interval: Duration, has_pending: bool) -> Duration {
        if has_pending {
            process_interval.min(self.pending_retry_interval)
        } else {
            process_interval
        }
    }

    fn interval_for(&self, active: bool, has_pending: bool) -> Duration {
        if has_pending {
            self.pending_retry_interval
        } else if active {
            self.active_poll_interval
        } else {
            self.background_poll_interval
        }
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingRateChange {
    target: PollingRate,
    reason: String,
    attempts: u32,
    last_error: Option<String>,
}

#[derive(Debug, Default)]
struct PendingRateQueue {
    changes: HashMap<DeviceKey, PendingRateChange>,
}

impl PendingRateQueue {
    fn has_pending(&self) -> bool {
        !self.changes.is_empty()
    }

    fn queue(
        &mut self,
        device: &DeviceSnapshot,
        target: PollingRate,
        reason: &str,
        error: Option<String>,
    ) -> bool {
        let key = DeviceKey::from_snapshot(device);
        let next = PendingRateChange {
            target,
            reason: reason.to_string(),
            attempts: 1,
            last_error: error,
        };

        match self.changes.get_mut(&key) {
            Some(existing)
                if existing.target == next.target
                    && existing.reason == next.reason
                    && existing.last_error == next.last_error =>
            {
                existing.attempts = existing.attempts.saturating_add(1);
                false
            }
            Some(existing) => {
                *existing = next;
                true
            }
            None => {
                self.changes.insert(key, next);
                true
            }
        }
    }

    fn clear(&mut self, device: &DeviceSnapshot) -> Option<PendingRateChange> {
        self.changes.remove(&DeviceKey::from_snapshot(device))
    }

    fn retain_visible(&mut self, devices: &[DeviceSnapshot]) {
        let visible = devices
            .iter()
            .map(DeviceKey::from_snapshot)
            .collect::<HashSet<_>>();
        self.changes.retain(|key, _| visible.contains(key));
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

struct PowerPolicyState<'a> {
    battery_trends: &'a mut BatteryTrendTracker,
    snapshot_cache: &'a mut DeviceSnapshotCache,
    pending_changes: &'a mut PendingRateQueue,
}

fn apply_power_policy(
    monitor: Option<&HidPollMonitor>,
    dry_run: bool,
    config: &PollMonitorConfig,
    state: &mut PowerPolicyState<'_>,
    active: Option<&ActiveRule>,
    desired: &DesiredRate,
) -> Result<()> {
    let monitor = monitor.expect("power-aware watcher must have a monitor");
    let mut devices = monitor.scan()?;
    state.snapshot_cache.apply(&mut devices);
    state.pending_changes.retain_visible(&devices);
    if devices.is_empty() {
        if dry_run {
            println!("dry run: no supported device visible for {desired}");
            return Ok(());
        }
        return Ok(());
    }
    state.battery_trends.record(&devices);

    let idle_rate = active
        .and_then(|rule| rule.restore)
        .or(config.restore_rate)
        .unwrap_or(config.default_rate);
    let classes = devices
        .iter()
        .map(|device| classify_power(device, config, state.battery_trends))
        .collect::<Vec<_>>();
    let has_known_discharging = classes.contains(&PowerClass::Discharging);

    for (device, class) in devices.iter().zip(classes) {
        let (target, reason) = power_target_rate(
            active,
            desired.rate,
            idle_rate,
            class,
            has_known_discharging,
            config.allow_unknown_power_active,
        );
        apply_snapshot_rate(
            monitor,
            dry_run,
            state.pending_changes,
            device,
            target,
            reason,
        )?;
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
    pending_changes: &mut PendingRateQueue,
    snapshot: &DeviceSnapshot,
    target: PollingRate,
    reason: &str,
) -> Result<()> {
    if !snapshot.supported_rates.contains(&target) {
        println!(
            "{} {} {:04x}:{:04x}: skipping unsupported target {} ({reason})",
            snapshot.vendor_name, snapshot.model_name, snapshot.vid, snapshot.pid, target
        );
        pending_changes.clear(snapshot);
        return Ok(());
    }

    if snapshot.current_rate == Some(target) {
        if let Some(pending) = pending_changes.clear(snapshot) {
            println!(
                "{} {} {:04x}:{:04x}: queued target {} is now satisfied ({})",
                snapshot.vendor_name,
                snapshot.model_name,
                snapshot.vid,
                snapshot.pid,
                pending.target,
                pending.reason
            );
        }
        return Ok(());
    }

    if snapshot.cached_rate || snapshot.current_rate.is_none() {
        let detail = if snapshot.cached_rate {
            "latest rate read is cached"
        } else {
            "current rate is unknown"
        };
        if dry_run {
            println!(
                "dry run: would queue {} {} {:04x}:{:04x} {} -> {} ({reason}, {detail}, battery {})",
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
        } else if pending_changes.queue(snapshot, target, reason, Some(detail.to_string())) {
            println!(
                "queued {} {} {:04x}:{:04x} {} -> {} ({reason}, {detail}, battery {})",
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
    match monitor
        .open_by_vid_pid(snapshot.vid, snapshot.pid)
        .and_then(|device| {
            let before = device.read_rate().ok().or(snapshot.current_rate);
            if before != Some(target) {
                device.set_rate(target)?;
                thread::sleep(Duration::from_millis(80));
            }
            let after = device.read_rate().ok();
            Ok((before, after))
        }) {
        Ok((before, after)) => {
            let confirmed = after == Some(target);
            if confirmed {
                pending_changes.clear(snapshot);
            } else {
                let detail = after
                    .map(|rate| format!("device confirmed {rate} after write"))
                    .unwrap_or_else(|| "post-write read did not confirm target".to_string());
                pending_changes.queue(snapshot, target, reason, Some(detail));
            }
            println!(
                "{} {} {:04x}:{:04x}: {} -> {} ({reason}, battery {}, {} ms{})",
                snapshot.vendor_name,
                snapshot.model_name,
                snapshot.vid,
                snapshot.pid,
                before
                    .map(|rate| rate.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                after
                    .map(|rate| rate.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                snapshot.battery_text(),
                started.elapsed().as_millis(),
                if confirmed { "" } else { ", queued" }
            );
        }
        Err(err) => {
            let detail = err.to_string();
            if pending_changes.queue(snapshot, target, reason, Some(detail.clone())) {
                println!(
                    "queued {} {} {:04x}:{:04x} {} -> {} ({reason}, {detail}, battery {})",
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
        }
    }
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
    fn device_scheduler_uses_expected_poll_intervals() {
        let config = PollMonitorConfig::default();
        let mut scheduler = DevicePollScheduler::from_config(&config);
        let now = Instant::now();

        assert!(scheduler.should_poll(now, false, false, false));
        scheduler.record_poll(now);

        assert!(scheduler.should_poll(now + Duration::from_secs(1), false, true, false));
        assert!(!scheduler.should_poll(now + Duration::from_secs(4), true, false, false));
        assert!(scheduler.should_poll(now + Duration::from_secs(5), true, false, false));
        assert!(!scheduler.should_poll(now + Duration::from_secs(599), false, false, false));
        assert!(scheduler.should_poll(now + Duration::from_secs(600), false, false, false));
        assert!(scheduler.should_poll(now + Duration::from_millis(1), false, false, true));
    }

    #[test]
    fn pending_rate_queue_updates_target_and_retain_visible() {
        let device = test_device(
            IPI_VENDOR_ID,
            0x1014,
            ProtocolKind::IpiPixV1 { report_id: 3 },
            BatteryStatus::level_only(80),
        );
        let mut queue = PendingRateQueue::default();

        assert!(queue.queue(
            &device,
            PollingRate::Hz4000,
            "active non-charging",
            Some("latest rate read is cached".to_string())
        ));
        assert!(queue.has_pending());
        assert!(!queue.queue(
            &device,
            PollingRate::Hz4000,
            "active non-charging",
            Some("latest rate read is cached".to_string())
        ));
        assert!(queue.queue(&device, PollingRate::Hz1000, "idle", None));

        let pending = queue.clear(&device).unwrap();
        assert_eq!(pending.target, PollingRate::Hz1000);
        assert_eq!(pending.reason, "idle");

        queue.queue(&device, PollingRate::Hz4000, "active non-charging", None);
        queue.retain_visible(&[]);
        assert!(!queue.has_pending());
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
        tracker.record_at(std::slice::from_ref(&device), now);
        device.battery = Some(BatteryStatus::level_only(81));
        tracker.record_at(std::slice::from_ref(&device), now + Duration::from_secs(60));

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
        tracker.record_at(std::slice::from_ref(&device), now);
        device.battery = Some(BatteryStatus::with_raw_state(
            76,
            ChargeState::Discharging,
            0,
        ));
        tracker.record_at(std::slice::from_ref(&device), now + Duration::from_secs(60));

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
        tracker.record_at(std::slice::from_ref(&device), now);
        tracker.record_at(std::slice::from_ref(&device), now + Duration::from_secs(60));

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
