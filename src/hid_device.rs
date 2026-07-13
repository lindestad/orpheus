use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use hidapi::{HidApi, HidDevice};

use crate::devices::{
    BatteryStatus, ConnectionKind, ModelInfo, PollingRate, ProtocolKind, find_model,
    format_supported_rates,
};
use crate::protocols::{
    DeviceTransport, ProtocolDevice, REPORT8_ID, REPORT8_PAYLOAD_LEN, normalize_feature_payload,
    normalize_report_payload, write_report8_crc,
};

const REPORT8_CONTROL_USAGE_PAGE: u16 = 0xFF02;
const REPORT8_CONTROL_USAGE: u16 = 0x0002;

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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HidAccessCandidate {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub vendor_name: &'static str,
    pub model_name: &'static str,
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
                if device.current_rate.is_none()
                    && let Some(rate) = cached.current_rate
                {
                    device.current_rate = Some(rate);
                    device.cached_rate = true;
                }
                if device.battery.is_none()
                    && let Some(battery) = cached.battery
                {
                    device.battery = Some(battery);
                    device.cached_battery = true;
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
        let mut seen_paths = HashSet::new();
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if !is_control_interface(info, model.protocol) {
                continue;
            }
            let path = info.path().to_string_lossy().into_owned();
            if !seen_paths.insert(path.clone()) {
                continue;
            }

            let probe = self.probe_info(info, model, connection);

            let snapshot = DeviceSnapshot {
                path,
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

    pub fn diagnose(&self) -> Vec<DeviceDiagnostic> {
        let mut diagnostics = Vec::new();
        let mut seen_paths = HashSet::new();
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if !is_control_interface(info, model.protocol) {
                continue;
            }
            let path = info.path().to_string_lossy().into_owned();
            if !seen_paths.insert(path.clone()) {
                continue;
            }

            let mut protocol_results = Vec::new();
            for protocol in model.protocol_candidates() {
                let started = Instant::now();
                let device = match info.open_device(&self.api) {
                    Ok(device) => device,
                    Err(err) => {
                        protocol_results.push(ProtocolDiagnostic {
                            protocol,
                            open: ProbeOutcome::error_timed(format!("{err:#}"), started),
                            control_probe: ProbeOutcome::skipped("open failed"),
                            rate_read: ProbeOutcome::skipped("open failed"),
                            battery_read: ProbeOutcome::skipped("open failed"),
                        });
                        continue;
                    }
                };

                let live = PollingDevice::new(device, model, connection, protocol);
                let control_probe = timed_probe(|| {
                    live.probe_control()?;
                    Ok("control interface answered".to_string())
                });
                let rate_read = if live.supports_rate_read() {
                    timed_probe(|| live.read_rate().map(|rate| rate.to_string()))
                } else {
                    ProbeOutcome::skipped("protocol does not support current-rate read")
                };
                let battery_read = if live.supports_battery_read() {
                    timed_probe(|| live.read_battery().map(|battery| battery.to_string()))
                } else {
                    ProbeOutcome::skipped("protocol does not support battery telemetry")
                };

                protocol_results.push(ProtocolDiagnostic {
                    protocol,
                    open: ProbeOutcome::ok_timed("opened", started),
                    control_probe,
                    rate_read,
                    battery_read,
                });
            }

            diagnostics.push(DeviceDiagnostic {
                path,
                vid,
                pid,
                manufacturer: info.manufacturer_string().map(ToOwned::to_owned),
                product: info.product_string().map(ToOwned::to_owned),
                serial: info.serial_number().map(ToOwned::to_owned),
                release_number: info.release_number(),
                usage_page: device_usage_page(info),
                usage: device_usage(info),
                interface_number: info.interface_number(),
                bus_type: format!("{:?}", info.bus_type()),
                vendor_name: model.vendor_name,
                model_name: model.name,
                connection,
                supported_rates: model.supported_rates(connection).to_vec(),
                protocols: protocol_results,
            });
        }
        diagnostics
    }

    pub fn hid_access_candidates(&self) -> Vec<HidAccessCandidate> {
        #[cfg(target_os = "linux")]
        {
            let mut candidates = Vec::new();
            let mut seen = HashSet::new();
            for info in self.api.device_list() {
                let vid = info.vendor_id();
                let pid = info.product_id();
                let Some((model, _)) = find_model(vid, pid) else {
                    continue;
                };
                if !is_control_interface(info, model.protocol) {
                    continue;
                }

                let Err(err) = info.open_device(&self.api) else {
                    continue;
                };
                let error = format!("{err:#}");
                if !is_hid_permission_error(&error) {
                    continue;
                }

                let path = info.path().to_string_lossy().into_owned();
                if !path.starts_with("/dev/hidraw") || !seen.insert(path.clone()) {
                    continue;
                }

                candidates.push(HidAccessCandidate {
                    path,
                    vid,
                    pid,
                    vendor_name: model.vendor_name,
                    model_name: model.name,
                });
            }
            candidates
        }

        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
    }

    pub fn open_first_supported(&self) -> Result<PollingDevice> {
        let mut seen_paths = HashSet::new();
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if !is_control_interface(info, model.protocol)
                || !seen_paths.insert(info.path().to_owned())
            {
                continue;
            }
            for protocol in model.protocol_candidates() {
                if let Ok(device) = info.open_device(&self.api) {
                    let live = PollingDevice::new(device, model, connection, protocol);
                    if self.probe_control_interface(&live).is_ok() {
                        return Ok(live);
                    }
                }
            }
        }
        Err(anyhow!("no supported polling-rate device found"))
    }

    pub fn open_first_supported_unprobed(
        &self,
        interface_number: Option<i32>,
    ) -> Result<PollingDevice> {
        let mut seen_paths = HashSet::new();
        for info in self.api.device_list() {
            if interface_number.is_some_and(|wanted| info.interface_number() != wanted) {
                continue;
            }
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if interface_number.is_none() && !is_control_interface(info, model.protocol) {
                continue;
            }
            if !seen_paths.insert(info.path().to_owned()) {
                continue;
            }
            let device = info
                .open_device(&self.api)
                .with_context(|| format!("failed to open {:04x}:{:04x}", vid, pid))?;
            return Ok(PollingDevice::new(
                device,
                model,
                connection,
                model.protocol,
            ));
        }
        if let Some(interface_number) = interface_number {
            Err(anyhow!(
                "no supported HID interface {interface_number} found"
            ))
        } else {
            Err(anyhow!("no supported HID interface found"))
        }
    }

    pub fn open_by_vid_pid(&self, target_vid: u16, target_pid: u16) -> Result<PollingDevice> {
        let mut last_error = None;
        let mut seen_paths = HashSet::new();
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            if vid != target_vid || pid != target_pid {
                continue;
            }
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if !is_control_interface(info, model.protocol)
                || !seen_paths.insert(info.path().to_owned())
            {
                continue;
            }
            for protocol in model.protocol_candidates() {
                let device = info
                    .open_device(&self.api)
                    .with_context(|| format!("failed to open {:04x}:{:04x}", vid, pid))?;
                let live = PollingDevice::new(device, model, connection, protocol);
                match self.probe_control_interface(&live) {
                    Ok(()) => return Ok(live),
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

    pub fn open_by_path(&self, target_path: &str) -> Result<PollingDevice> {
        let mut last_error = None;
        for info in self.api.device_list() {
            if info.path().to_string_lossy() != target_path {
                continue;
            }
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if !is_control_interface(info, model.protocol) {
                continue;
            }
            for protocol in model.protocol_candidates() {
                let device = info
                    .open_device(&self.api)
                    .with_context(|| format!("failed to open {target_path}"))?;
                let live = PollingDevice::new(device, model, connection, protocol);
                match self.probe_control_interface(&live) {
                    Ok(()) => return Ok(live),
                    Err(err) => last_error = Some(format!("{protocol}: {err}")),
                }
            }
        }

        if let Some(error) = last_error {
            Err(anyhow!(
                "found {target_path}, but its control interface did not answer: {error}"
            ))
        } else {
            Err(anyhow!(
                "no supported control interface found at {target_path}"
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
            if live.supports_rate_read() {
                match live.read_rate() {
                    Ok(rate) => {
                        let battery = self.probe_battery_if_supported(&live);
                        return DeviceProbe {
                            protocol,
                            current_rate: Some(rate),
                            battery: battery.battery,
                            battery_error: battery.error,
                            read_error: None,
                        };
                    }
                    Err(err) => {
                        let read_error = err.to_string();
                        let battery = self.probe_battery_if_supported(&live);
                        if battery.battery.is_some() {
                            return DeviceProbe {
                                protocol,
                                current_rate: None,
                                battery: battery.battery,
                                battery_error: battery.error,
                                read_error: Some(read_error),
                            };
                        }
                        errors.push(format!(
                            "{protocol}: {read_error}; battery: {}",
                            battery.error.unwrap_or_else(|| "not available".to_string())
                        ));
                    }
                }
            } else {
                let battery = self.probe_battery_if_supported(&live);
                if battery.battery.is_some() {
                    return DeviceProbe {
                        protocol,
                        current_rate: None,
                        battery: battery.battery,
                        battery_error: battery.error,
                        read_error: None,
                    };
                }
                errors.push(format!(
                    "{protocol}: {}",
                    battery
                        .error
                        .unwrap_or_else(|| "device did not answer protocol probe".to_string())
                ));
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

    fn probe_battery_if_supported(&self, live: &PollingDevice) -> BatteryProbe {
        if live.supports_battery_read() {
            self.probe_battery(live)
        } else {
            BatteryProbe {
                battery: None,
                error: None,
            }
        }
    }

    fn probe_control_interface(&self, live: &PollingDevice) -> Result<()> {
        live.probe_control()
    }
}

fn is_hid_permission_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("permission denied")
        || error.contains("access denied")
        || error.contains("operation not permitted")
        || error.contains("os error 13")
}

#[derive(Clone, Debug)]
pub struct DeviceDiagnostic {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub release_number: u16,
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
    pub interface_number: i32,
    pub bus_type: String,
    pub vendor_name: &'static str,
    pub model_name: &'static str,
    pub connection: ConnectionKind,
    pub supported_rates: Vec<PollingRate>,
    pub protocols: Vec<ProtocolDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct ProtocolDiagnostic {
    pub protocol: ProtocolKind,
    pub open: ProbeOutcome,
    pub control_probe: ProbeOutcome,
    pub rate_read: ProbeOutcome,
    pub battery_read: ProbeOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeOutcome {
    Ok { detail: String, elapsed_ms: u128 },
    Skipped { reason: String },
    Error { error: String, elapsed_ms: u128 },
}

impl ProbeOutcome {
    pub fn ok_timed(detail: impl Into<String>, started: Instant) -> Self {
        Self::Ok {
            detail: detail.into(),
            elapsed_ms: started.elapsed().as_millis(),
        }
    }

    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    pub fn error_timed(error: impl Into<String>, started: Instant) -> Self {
        Self::Error {
            error: error.into(),
            elapsed_ms: started.elapsed().as_millis(),
        }
    }

    pub const fn status(&self) -> &'static str {
        match self {
            Self::Ok { .. } => "ok",
            Self::Skipped { .. } => "skipped",
            Self::Error { .. } => "error",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Ok { detail, .. } => Some(detail),
            Self::Skipped { .. } | Self::Error { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Skipped { reason } => Some(reason),
            Self::Ok { .. } | Self::Error { .. } => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { error, .. } => Some(error),
            Self::Ok { .. } | Self::Skipped { .. } => None,
        }
    }

    pub const fn elapsed_ms(&self) -> Option<u128> {
        match self {
            Self::Ok { elapsed_ms, .. } | Self::Error { elapsed_ms, .. } => Some(*elapsed_ms),
            Self::Skipped { .. } => None,
        }
    }
}

fn timed_probe(probe: impl FnOnce() -> Result<String>) -> ProbeOutcome {
    let started = Instant::now();
    match probe() {
        Ok(detail) => ProbeOutcome::ok_timed(detail, started),
        Err(err) => ProbeOutcome::error_timed(format!("{err:#}"), started),
    }
}

fn device_usage_page(info: &hidapi::DeviceInfo) -> Option<u16> {
    Some(info.usage_page())
}

fn device_usage(info: &hidapi::DeviceInfo) -> Option<u16> {
    Some(info.usage())
}

fn is_control_interface(info: &hidapi::DeviceInfo, protocol: ProtocolKind) -> bool {
    match protocol {
        ProtocolKind::Eeprom16 => {
            info.usage_page() == REPORT8_CONTROL_USAGE_PAGE && info.usage() == REPORT8_CONTROL_USAGE
        }
        ProtocolKind::Feature64 { .. }
        | ProtocolKind::IpiPixV1 { .. }
        | ProtocolKind::LogitechHidpp
        | ProtocolKind::RazerV1 { .. } => true,
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
        existing.read_error = snapshot.read_error;
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

    pub fn supports_rate_read(&self) -> bool {
        self.protocol.supports_rate_read()
    }

    pub fn supports_battery_read(&self) -> bool {
        self.protocol.supports_battery_read()
    }

    pub fn supports_dpi(&self) -> bool {
        self.protocol.supports_dpi()
    }

    pub fn probe_control(&self) -> Result<()> {
        self.protocol_device().probe_control()
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

    pub fn read_dpi(&self) -> Result<u16> {
        self.protocol_device().read_dpi()
    }

    pub fn set_dpi(&self, dpi: u16) -> Result<()> {
        self.protocol_device().set_dpi(dpi)
    }

    pub fn report8_exchange(
        &self,
        mut payload: [u8; REPORT8_PAYLOAD_LEN],
        write_crc: bool,
        timeout_ms: i32,
        reads: usize,
    ) -> Result<Vec<Vec<u8>>> {
        while matches!(
            self.device
                .read_input_payload(REPORT8_ID, REPORT8_PAYLOAD_LEN, 1),
            Ok(response) if !response.is_empty()
        ) {}

        if write_crc {
            write_report8_crc(&mut payload);
        }
        self.device.write_output_payload(REPORT8_ID, &payload)?;

        let mut responses = Vec::new();
        for _ in 0..reads {
            let response =
                self.device
                    .read_input_payload(REPORT8_ID, REPORT8_PAYLOAD_LEN, timeout_ms)?;
            if !response.is_empty() {
                responses.push(response);
            }
        }
        Ok(responses)
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
        self.send_output_report(&buffer)
            .context("failed to send output report")?;
        Ok(buffer.len())
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

    #[test]
    fn merge_snapshot_promotes_battery_only_control_interface() {
        let mut devices = Vec::new();
        let mut dead_interface = test_snapshot(None, None);
        dead_interface.read_error = Some("wrong interface".to_string());
        merge_snapshot(&mut devices, dead_interface);

        let live_interface = test_snapshot(None, Some(BatteryStatus::level_only(88)));
        merge_snapshot(&mut devices, live_interface);

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].battery, Some(BatteryStatus::level_only(88)));
        assert_eq!(devices[0].read_error, None);
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
