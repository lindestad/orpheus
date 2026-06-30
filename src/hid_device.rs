use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use hidapi::{HidApi, HidDevice};

use crate::devices::{
    BatteryStatus, ConnectionKind, ModelInfo, PollingRate, ProtocolKind, find_model,
    format_supported_rates,
};
use crate::protocols::{
    DeviceTransport, ProtocolDevice, normalize_feature_payload, normalize_report_payload,
};

#[derive(Clone, Debug)]
pub struct DeviceSnapshot {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub product_name: Option<String>,
    pub vendor_name: &'static str,
    pub model_name: &'static str,
    pub connection: ConnectionKind,
    pub protocol: ProtocolKind,
    pub supported_rates: Vec<PollingRate>,
    pub current_rate: Option<PollingRate>,
    pub battery: Option<BatteryStatus>,
    pub cached_rate: bool,
    pub cached_battery: bool,
    pub battery_error: Option<String>,
    pub read_error: Option<String>,
}

impl DeviceSnapshot {
    pub fn supported_rates_text(&self) -> String {
        format_supported_rates(&self.supported_rates)
    }

    pub fn battery_text(&self) -> String {
        let mut text = self
            .battery
            .map(|battery| battery.to_string())
            .or_else(|| {
                self.battery_error
                    .as_ref()
                    .map(|_| "battery read error".to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        if self.cached_battery && self.battery.is_some() {
            text.push_str(" (cached)");
        }
        text
    }
}

#[derive(Debug, Default)]
pub struct DeviceSnapshotCache {
    entries: HashMap<DeviceSnapshotKey, CachedDeviceReport>,
}

impl DeviceSnapshotCache {
    pub fn apply(&mut self, devices: &mut [DeviceSnapshot]) {
        let mut visible = HashSet::new();
        for device in devices {
            let key = DeviceSnapshotKey::from_snapshot(device);
            visible.insert(key);

            if let Some(cached) = self.entries.get(&key).copied() {
                if device.current_rate.is_none() {
                    if let Some(rate) = cached.current_rate {
                        device.current_rate = Some(rate);
                        device.cached_rate = true;
                    }
                }
                if device.battery.is_none() {
                    if let Some(battery) = cached.battery {
                        device.battery = Some(battery);
                        device.cached_battery = true;
                    }
                }
            }

            let entry = self.entries.entry(key).or_default();
            if device.current_rate.is_some() && !device.cached_rate {
                entry.current_rate = device.current_rate;
            }
            if device.battery.is_some() && !device.cached_battery {
                entry.battery = device.battery;
            }
        }

        self.entries.retain(|key, _| visible.contains(key));
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DeviceSnapshotKey {
    vid: u16,
    pid: u16,
    connection: ConnectionKind,
}

impl DeviceSnapshotKey {
    fn from_snapshot(device: &DeviceSnapshot) -> Self {
        Self {
            vid: device.vid,
            pid: device.pid,
            connection: device.connection,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct CachedDeviceReport {
    current_rate: Option<PollingRate>,
    battery: Option<BatteryStatus>,
}

pub struct HidPollMonitor {
    api: HidApi,
}

impl HidPollMonitor {
    pub fn new() -> Result<Self> {
        let api = HidApi::new().context("failed to initialize HID API")?;
        Ok(Self { api })
    }

    pub fn scan(&self) -> Result<Vec<DeviceSnapshot>> {
        let mut devices = Vec::new();
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };

            let probe = self.probe_info(info, model, connection);

            let snapshot = DeviceSnapshot {
                path: info.path().to_string_lossy().into_owned(),
                vid,
                pid,
                product_name: info.product_string().map(ToOwned::to_owned),
                vendor_name: model.vendor_name,
                model_name: model.name,
                connection,
                protocol: probe.protocol,
                supported_rates: model.supported_rates(connection).to_vec(),
                current_rate: probe.current_rate,
                battery: probe.battery,
                cached_rate: false,
                cached_battery: false,
                battery_error: probe.battery_error,
                read_error: probe.read_error,
            };

            merge_snapshot(&mut devices, snapshot);
        }
        Ok(devices)
    }

    pub fn open_first_supported(&self) -> Result<PollingDevice> {
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            for protocol in model.protocol_candidates() {
                if let Ok(device) = info.open_device(&self.api) {
                    let live = PollingDevice::new(device, model, connection, protocol);
                    if live.read_rate().is_ok() {
                        return Ok(live);
                    }
                }
            }
        }
        Err(anyhow!("no supported polling-rate device found"))
    }

    pub fn open_by_vid_pid(&self, target_vid: u16, target_pid: u16) -> Result<PollingDevice> {
        let mut last_error = None;
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            if vid != target_vid || pid != target_pid {
                continue;
            }
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            for protocol in model.protocol_candidates() {
                let device = info
                    .open_device(&self.api)
                    .with_context(|| format!("failed to open {:04x}:{:04x}", vid, pid))?;
                let live = PollingDevice::new(device, model, connection, protocol);
                match live.read_rate() {
                    Ok(_) => return Ok(live),
                    Err(err) => last_error = Some(format!("{protocol}: {err}")),
                }
            }
        }
        if let Some(error) = last_error {
            Err(anyhow!(
                "found {:04x}:{:04x}, but no interface answered: {error}",
                target_vid,
                target_pid
            ))
        } else {
            Err(anyhow!(
                "no supported device found for {:04x}:{:04x}",
                target_vid,
                target_pid
            ))
        }
    }

    fn probe_info(
        &self,
        info: &hidapi::DeviceInfo,
        model: &'static ModelInfo,
        connection: ConnectionKind,
    ) -> DeviceProbe {
        let mut errors = Vec::new();
        for protocol in model.protocol_candidates() {
            let device = match info.open_device(&self.api) {
                Ok(device) => device,
                Err(err) => {
                    errors.push(format!("{protocol}: {err}"));
                    continue;
                }
            };
            let live = PollingDevice::new(device, model, connection, protocol);
            match live.read_rate() {
                Ok(rate) => {
                    let battery = self.probe_battery(&live);
                    return DeviceProbe {
                        protocol,
                        current_rate: Some(rate),
                        battery: battery.battery,
                        battery_error: battery.error,
                        read_error: None,
                    };
                }
                Err(err) => errors.push(format!("{protocol}: {err}")),
            }
        }

        DeviceProbe {
            protocol: model.protocol,
            current_rate: None,
            battery: None,
            battery_error: None,
            read_error: Some(errors.join("; ")),
        }
    }

    fn probe_battery(&self, live: &PollingDevice) -> BatteryProbe {
        match live.read_battery() {
            Ok(battery) => BatteryProbe {
                battery: Some(battery),
                error: None,
            },
            Err(err) => BatteryProbe {
                battery: None,
                error: Some(err.to_string()),
            },
        }
    }
}

struct DeviceProbe {
    protocol: ProtocolKind,
    current_rate: Option<PollingRate>,
    battery: Option<BatteryStatus>,
    battery_error: Option<String>,
    read_error: Option<String>,
}

struct BatteryProbe {
    battery: Option<BatteryStatus>,
    error: Option<String>,
}

fn merge_snapshot(devices: &mut Vec<DeviceSnapshot>, snapshot: DeviceSnapshot) {
    let Some(existing) = devices.iter_mut().find(|device| {
        device.vid == snapshot.vid
            && device.pid == snapshot.pid
            && device.connection == snapshot.connection
    }) else {
        devices.push(snapshot);
        return;
    };

    if existing.current_rate.is_none() && snapshot.current_rate.is_some() {
        *existing = snapshot;
    } else if existing.current_rate.is_none() && existing.read_error.is_none() {
        existing.read_error = snapshot.read_error;
        existing.battery_error = snapshot.battery_error;
    } else if existing.battery.is_none() && snapshot.battery.is_some() {
        existing.battery = snapshot.battery;
        existing.battery_error = None;
    } else if existing.battery.is_none() && existing.battery_error.is_none() {
        existing.battery_error = snapshot.battery_error;
    }
}

pub struct PollingDevice {
    device: HidDevice,
    model: &'static ModelInfo,
    connection: ConnectionKind,
    protocol: ProtocolKind,
}

impl PollingDevice {
    pub fn new(
        device: HidDevice,
        model: &'static ModelInfo,
        connection: ConnectionKind,
        protocol: ProtocolKind,
    ) -> Self {
        Self {
            device,
            model,
            connection,
            protocol,
        }
    }

    pub fn model(&self) -> &'static ModelInfo {
        self.model
    }

    pub fn connection(&self) -> ConnectionKind {
        self.connection
    }

    pub fn protocol(&self) -> ProtocolKind {
        self.protocol
    }

    pub fn read_rate(&self) -> Result<PollingRate> {
        self.protocol_device().read_rate()
    }

    pub fn read_battery(&self) -> Result<BatteryStatus> {
        self.protocol_device().read_battery()
    }

    pub fn set_rate(&self, rate: PollingRate) -> Result<()> {
        self.protocol_device().set_rate(rate)
    }

    fn protocol_device(&self) -> ProtocolDevice<'_, HidDevice> {
        ProtocolDevice::new(&self.device, self.model, self.connection, self.protocol)
    }
}

impl DeviceTransport for HidDevice {
    fn send_feature_payload(&self, report_id: u8, payload: &[u8]) -> Result<()> {
        let mut buffer = Vec::with_capacity(payload.len() + 1);
        buffer.push(report_id);
        buffer.extend_from_slice(payload);
        self.send_feature_report(&buffer)
            .context("failed to send feature report")
    }

    fn get_feature_payload(&self, report_id: u8, payload_len: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0; payload_len + 1];
        buffer[0] = report_id;
        let len = self
            .get_feature_report(&mut buffer)
            .context("failed to receive feature report")?;
        Ok(normalize_feature_payload(
            &buffer[..len],
            report_id,
            payload_len,
        ))
    }

    fn write_output_payload(&self, report_id: u8, payload: &[u8]) -> Result<usize> {
        let mut buffer = Vec::with_capacity(payload.len() + 1);
        buffer.push(report_id);
        buffer.extend_from_slice(payload);
        self.write(&buffer).context("failed to write output report")
    }

    fn read_input_payload(
        &self,
        report_id: u8,
        payload_len: usize,
        timeout_ms: i32,
    ) -> Result<Vec<u8>> {
        let mut buffer = vec![0; payload_len + 1];
        let len = self
            .read_timeout(&mut buffer, timeout_ms)
            .context("failed to read input report")?;
        if len == 0 {
            Ok(Vec::new())
        } else {
            Ok(normalize_report_payload(
                &buffer[..len],
                report_id,
                payload_len,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_cache_reuses_last_valid_report() {
        let mut cache = DeviceSnapshotCache::default();
        let mut fresh = vec![test_snapshot(
            Some(PollingRate::Hz4000),
            Some(BatteryStatus::level_only(82)),
        )];
        cache.apply(&mut fresh);

        let mut sleeping = vec![test_snapshot(None, None)];
        sleeping[0].read_error = Some("failed to send feature report".to_string());
        cache.apply(&mut sleeping);

        assert_eq!(sleeping[0].current_rate, Some(PollingRate::Hz4000));
        assert_eq!(sleeping[0].battery, Some(BatteryStatus::level_only(82)));
        assert!(sleeping[0].cached_rate);
        assert!(sleeping[0].cached_battery);
    }

    #[test]
    fn snapshot_cache_updates_live_rate_without_forgetting_battery() {
        let mut cache = DeviceSnapshotCache::default();
        let mut initial = vec![test_snapshot(
            Some(PollingRate::Hz1000),
            Some(BatteryStatus::level_only(70)),
        )];
        cache.apply(&mut initial);

        let mut partial = vec![test_snapshot(Some(PollingRate::Hz4000), None)];
        partial[0].battery_error = Some("battery asleep".to_string());
        cache.apply(&mut partial);

        assert_eq!(partial[0].current_rate, Some(PollingRate::Hz4000));
        assert_eq!(partial[0].battery, Some(BatteryStatus::level_only(70)));
        assert!(!partial[0].cached_rate);
        assert!(partial[0].cached_battery);

        let mut sleeping = vec![test_snapshot(None, None)];
        cache.apply(&mut sleeping);

        assert_eq!(sleeping[0].current_rate, Some(PollingRate::Hz4000));
        assert_eq!(sleeping[0].battery, Some(BatteryStatus::level_only(70)));
    }

    fn test_snapshot(
        current_rate: Option<PollingRate>,
        battery: Option<BatteryStatus>,
    ) -> DeviceSnapshot {
        DeviceSnapshot {
            path: "test".to_string(),
            vid: 0x1234,
            pid: 0x5678,
            product_name: None,
            vendor_name: "Test",
            model_name: "Mouse",
            connection: ConnectionKind::Wireless,
            protocol: ProtocolKind::IpiPixV1 { report_id: 3 },
            supported_rates: vec![PollingRate::Hz1000, PollingRate::Hz4000],
            current_rate,
            battery,
            cached_rate: false,
            cached_battery: false,
            battery_error: None,
            read_error: None,
        }
    }
}
