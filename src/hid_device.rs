use std::{
    collections::{HashMap, HashSet},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use hidapi::{HidApi, HidDevice};

use crate::devices::{
    BatteryStatus, ConnectionKind, ModelInfo, PollingRate, ProtocolKind, build_eeprom16_get_rate,
    build_eeprom16_set_rate, build_feature64_get_battery, build_feature64_get_rate,
    build_feature64_set_rate, build_ipi_pix_v1_get_basic_info, build_ipi_pix_v1_get_rate,
    build_ipi_pix_v1_set_rate, find_model, format_supported_rates,
    gwolves_charge_state_from_status, ipi_pix_v1_rate_from_sensor_byte,
};

const DEFAULT_PROFILE: u8 = 1;
const FEATURE_REPORT_ID: u8 = 0;
const REPORT8_ID: u8 = 8;
const FEATURE_PAYLOAD_LEN: usize = 64;
const REPORT8_PAYLOAD_LEN: usize = 16;
const REPORT8_BUFFER_LEN: usize = REPORT8_PAYLOAD_LEN + 1;
const IPI_PIX_PAYLOAD_LEN: usize = 63;

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
        match self.protocol {
            ProtocolKind::Feature64 { .. } => self.read_feature64_rate(),
            ProtocolKind::Eeprom16 => self.read_eeprom16_rate(),
            ProtocolKind::IpiPixV1 { .. } => self.read_ipi_pix_v1_rate(),
        }
    }

    pub fn read_battery(&self) -> Result<BatteryStatus> {
        match self.protocol {
            ProtocolKind::Feature64 { .. } => self.read_feature64_battery(),
            ProtocolKind::IpiPixV1 { .. } => self.read_ipi_pix_v1_battery(),
            ProtocolKind::Eeprom16 => Err(anyhow!(
                "battery telemetry is not implemented for report8 eeprom protocol"
            )),
        }
    }

    pub fn set_rate(&self, rate: PollingRate) -> Result<()> {
        self.model.require_rate(self.connection, rate)?;
        match self.protocol {
            ProtocolKind::Feature64 { .. } => self.set_feature64_rate(rate),
            ProtocolKind::Eeprom16 => self.set_eeprom16_rate(rate),
            ProtocolKind::IpiPixV1 { .. } => self.set_ipi_pix_v1_rate(rate),
        }
    }

    fn read_feature64_rate(&self) -> Result<PollingRate> {
        let report = build_feature64_get_rate(self.protocol, self.connection, DEFAULT_PROFILE);
        self.send_feature_payload(FEATURE_REPORT_ID, &report)?;
        thread::sleep(Duration::from_millis(20));
        let response = self.get_feature_payload(FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN)?;
        let code = self.extract_feature64_rate_code(&response)?;
        self.protocol
            .rate_from_code(code)
            .ok_or_else(|| anyhow!("device returned unknown polling-rate code {code}"))
    }

    fn set_feature64_rate(&self, rate: PollingRate) -> Result<()> {
        let report =
            build_feature64_set_rate(self.protocol, self.connection, DEFAULT_PROFILE, rate);
        self.send_feature_payload(FEATURE_REPORT_ID, &report)?;
        thread::sleep(Duration::from_millis(20));
        let _ = self.get_feature_payload(FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN);
        Ok(())
    }

    fn read_feature64_battery(&self) -> Result<BatteryStatus> {
        let report = build_feature64_get_battery(self.protocol, self.connection);
        self.send_feature_payload(FEATURE_REPORT_ID, &report)?;
        let delay = match self.protocol {
            ProtocolKind::Feature64 {
                new_protocol: true, ..
            } => 100,
            ProtocolKind::Feature64 {
                new_protocol: false,
                ..
            } => 50,
            ProtocolKind::Eeprom16 | ProtocolKind::IpiPixV1 { .. } => unreachable!(),
        };
        thread::sleep(Duration::from_millis(delay));
        let response = self.get_feature_payload(FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN)?;
        let (raw_state, level) = self.extract_feature64_battery(&response)?;
        Ok(BatteryStatus::with_raw_state(
            level,
            gwolves_charge_state_from_status(raw_state),
            raw_state,
        ))
    }

    fn read_eeprom16_rate(&self) -> Result<PollingRate> {
        let response = self.transact_report8(build_eeprom16_get_rate(), 8, 50, 5)?;
        let code = *response
            .get(5)
            .ok_or_else(|| anyhow!("short report8 response while reading polling rate"))?;
        self.protocol
            .rate_from_code(code)
            .ok_or_else(|| anyhow!("device returned unknown polling-rate code {code}"))
    }

    fn set_eeprom16_rate(&self, rate: PollingRate) -> Result<()> {
        let report = build_eeprom16_set_rate(rate);
        let _ = self.transact_report8(report, 7, 50, 5)?;
        Ok(())
    }

    fn read_ipi_pix_v1_rate(&self) -> Result<PollingRate> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi reader called for non-ipi protocol")
        };
        let report = build_ipi_pix_v1_get_rate();
        self.send_feature_payload(report_id, &report)?;
        thread::sleep(Duration::from_millis(50));
        let response = self.get_ipi_pix_v1_response(report_id, &report)?;
        let raw = response
            .get(6)
            .copied()
            .ok_or_else(|| anyhow!("short ipi pix v1 response while reading polling rate"))?;
        ipi_pix_v1_rate_from_sensor_byte(self.connection, raw)
            .ok_or_else(|| anyhow!("device returned unknown ipi pix v1 polling-rate byte {raw}"))
    }

    fn set_ipi_pix_v1_rate(&self, rate: PollingRate) -> Result<()> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi writer called for non-ipi protocol")
        };
        let report = build_ipi_pix_v1_set_rate(self.connection, rate);
        self.send_feature_payload(report_id, &report)?;
        thread::sleep(Duration::from_millis(60));
        let _ = self.get_ipi_pix_v1_response(report_id, &report);
        Ok(())
    }

    fn read_ipi_pix_v1_battery(&self) -> Result<BatteryStatus> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi battery reader called for non-ipi protocol")
        };
        let report = build_ipi_pix_v1_get_basic_info();
        self.send_feature_payload(report_id, &report)?;
        thread::sleep(Duration::from_millis(100));
        let response = self.get_ipi_pix_v1_response(report_id, &report)?;
        let level = response
            .get(5)
            .copied()
            .ok_or_else(|| anyhow!("short ipi pix v1 response while reading battery"))?;
        Ok(BatteryStatus::level_only(level))
    }

    fn extract_feature64_battery(&self, response: &[u8]) -> Result<(u8, u8)> {
        match self.protocol {
            ProtocolKind::Feature64 {
                new_protocol: true, ..
            } => {
                if response.get(1) == Some(&0xA1)
                    && response.get(4) == Some(&2)
                    && response.get(6) == Some(&131)
                {
                    let state = response.get(7).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery state")
                    })?;
                    let level = response.get(8).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery level")
                    })?;
                    return Ok((state, level));
                }

                if response.first() == Some(&0xA1)
                    && response.get(3) == Some(&2)
                    && response.get(5) == Some(&131)
                {
                    let state = response.get(6).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery state")
                    })?;
                    let level = response.get(7).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery level")
                    })?;
                    return Ok((state, level));
                }
            }
            ProtocolKind::Feature64 {
                new_protocol: false,
                ..
            } => {
                if response.get(1) == Some(&0xA1)
                    && response.get(2) == Some(&2)
                    && response.get(3) == Some(&143)
                {
                    let state = response.get(5).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery state")
                    })?;
                    let level = response.get(6).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery level")
                    })?;
                    return Ok((state, level));
                }

                if response.first() == Some(&0xA1)
                    && response.get(1) == Some(&2)
                    && response.get(2) == Some(&143)
                {
                    let state = response.get(4).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery state")
                    })?;
                    let level = response.get(5).copied().ok_or_else(|| {
                        anyhow!("short feature response while reading battery level")
                    })?;
                    return Ok((state, level));
                }
            }
            ProtocolKind::Eeprom16 | ProtocolKind::IpiPixV1 { .. } => {
                unreachable!("feature64 battery parser called for non-feature64 protocol")
            }
        }

        Err(anyhow!("device returned an unrecognized battery response"))
    }

    fn extract_feature64_rate_code(&self, response: &[u8]) -> Result<u8> {
        let hid_index = if response.first().copied().unwrap_or_default() >= 0xA0 {
            1
        } else {
            0
        };
        match self.protocol {
            ProtocolKind::Feature64 {
                new_protocol: true, ..
            } => response
                .get(8 - hid_index)
                .copied()
                .ok_or_else(|| anyhow!("short feature response while reading polling rate")),
            ProtocolKind::Feature64 {
                new_protocol: false,
                ..
            } => {
                let mut code = response
                    .get(5 - hid_index)
                    .copied()
                    .ok_or_else(|| anyhow!("short feature response while reading polling rate"))?;
                if self.connection.is_wired() && code == 64 {
                    code = 1;
                }
                Ok(code)
            }
            ProtocolKind::Eeprom16 | ProtocolKind::IpiPixV1 { .. } => {
                unreachable!("feature64 parser called for non-feature64 protocol")
            }
        }
    }

    fn send_feature_payload(&self, report_id: u8, payload: &[u8]) -> Result<()> {
        let mut buffer = Vec::with_capacity(payload.len() + 1);
        buffer.push(report_id);
        buffer.extend_from_slice(payload);
        self.device
            .send_feature_report(&buffer)
            .context("failed to send feature report")
    }

    fn get_feature_payload(&self, report_id: u8, payload_len: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0; payload_len + 1];
        buffer[0] = report_id;
        let len = self
            .device
            .get_feature_report(&mut buffer)
            .context("failed to receive feature report")?;
        Ok(normalize_feature_payload(
            &buffer[..len],
            report_id,
            payload_len,
        ))
    }

    fn get_ipi_pix_v1_response(
        &self,
        report_id: u8,
        request: &[u8; IPI_PIX_PAYLOAD_LEN],
    ) -> Result<Vec<u8>> {
        for _ in 0..4 {
            let response = self.get_feature_payload(report_id, IPI_PIX_PAYLOAD_LEN)?;
            let response = normalize_ipi_pix_v1_payload(&response);
            if response.get(3) == request.get(3) && response.len() > 6 {
                return Ok(response);
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(anyhow!(
            "timed out waiting for ipi pix v1 command {}",
            request[3]
        ))
    }

    fn transact_report8(
        &self,
        mut payload: [u8; REPORT8_PAYLOAD_LEN],
        response_command: u8,
        step_delay_ms: i32,
        attempts: usize,
    ) -> Result<Vec<u8>> {
        self.flush_report8();
        write_report8_crc(&mut payload);
        for _ in 0..attempts {
            self.write_report8_payload(&payload)?;
            for _ in 0..attempts {
                let mut buffer = [0; REPORT8_BUFFER_LEN];
                let len = self
                    .device
                    .read_timeout(&mut buffer, step_delay_ms)
                    .context("failed to read report8 input report")?;
                if len == 0 {
                    continue;
                }
                let response = normalize_report8_payload(&buffer[..len]);
                if response.first() == Some(&response_command) {
                    return Ok(response);
                }
            }
        }
        Err(anyhow!(
            "timed out waiting for report8 command {response_command}"
        ))
    }

    fn write_report8_payload(&self, payload: &[u8; REPORT8_PAYLOAD_LEN]) -> Result<usize> {
        let mut buffer = [0; REPORT8_BUFFER_LEN];
        buffer[0] = REPORT8_ID;
        buffer[1..].copy_from_slice(payload);
        self.device
            .write(&buffer)
            .context("failed to write report8")
    }

    fn flush_report8(&self) {
        let mut buffer = [0; REPORT8_BUFFER_LEN];
        while matches!(self.device.read_timeout(&mut buffer, 1), Ok(len) if len > 0) {}
    }
}

pub fn normalize_feature_payload(raw: &[u8], report_id: u8, payload_len: usize) -> Vec<u8> {
    if raw.len() == payload_len + 1 && raw[0] == report_id {
        raw[1..].to_vec()
    } else {
        raw.to_vec()
    }
}

pub fn normalize_ipi_pix_v1_payload(raw: &[u8]) -> Vec<u8> {
    raw.get(1..).map(ToOwned::to_owned).unwrap_or_default()
}

pub fn normalize_report8_payload(raw: &[u8]) -> Vec<u8> {
    if raw.len() == REPORT8_BUFFER_LEN && raw[0] == REPORT8_ID {
        raw[1..].to_vec()
    } else {
        raw.iter().copied().take(REPORT8_PAYLOAD_LEN).collect()
    }
}

pub fn report8_crc(payload: &[u8; REPORT8_PAYLOAD_LEN]) -> u8 {
    let sum = payload
        .iter()
        .take(REPORT8_PAYLOAD_LEN - 1)
        .fold(0u8, |acc, byte| acc.wrapping_add(*byte));
    85u8.wrapping_sub(sum)
}

pub fn write_report8_crc(payload: &mut [u8; REPORT8_PAYLOAD_LEN]) {
    payload[15] = report8_crc(payload).wrapping_sub(REPORT8_ID);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_native_feature_report_id() {
        let mut raw = vec![0; FEATURE_PAYLOAD_LEN + 1];
        raw[1] = 0xA1;
        raw[9] = 1;
        let payload = normalize_feature_payload(&raw, FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN);
        assert_eq!(payload.len(), FEATURE_PAYLOAD_LEN);
        assert_eq!(payload[0], 0xA1);
        assert_eq!(payload[8], 1);
    }

    #[test]
    fn strips_ipi_checksum_byte() {
        let payload = normalize_ipi_pix_v1_payload(&[233, 80, 0, 10, 79, 64, 0, 68]);
        assert_eq!(payload, vec![80, 0, 10, 79, 64, 0, 68]);
    }

    #[test]
    fn strips_native_report8_id() {
        let mut raw = vec![0; REPORT8_BUFFER_LEN];
        raw[0] = REPORT8_ID;
        raw[1] = 8;
        raw[6] = 64;
        let payload = normalize_report8_payload(&raw);
        assert_eq!(payload.len(), REPORT8_PAYLOAD_LEN);
        assert_eq!(payload[0], 8);
        assert_eq!(payload[5], 64);
    }

    #[test]
    fn computes_report8_crc_like_web_driver() {
        let mut report = build_eeprom16_set_rate(PollingRate::Hz8000);
        write_report8_crc(&mut report);
        assert_eq!(report[15], 239);
    }

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
