use std::{thread, time::Duration};

use anyhow::{Context, Result, anyhow};
use hidapi::{HidApi, HidDevice};

use crate::gwolves::{
    ConnectionKind, ModelInfo, PollingRate, ProtocolKind, build_eeprom16_get_rate,
    build_eeprom16_set_rate, build_feature64_get_rate, build_feature64_set_rate, find_model,
    format_supported_rates,
};

const DEFAULT_PROFILE: u8 = 1;
const FEATURE_REPORT_ID: u8 = 0;
const REPORT8_ID: u8 = 8;
const FEATURE_PAYLOAD_LEN: usize = 64;
const FEATURE_BUFFER_LEN: usize = FEATURE_PAYLOAD_LEN + 1;
const REPORT8_PAYLOAD_LEN: usize = 16;
const REPORT8_BUFFER_LEN: usize = REPORT8_PAYLOAD_LEN + 1;

#[derive(Clone, Debug)]
pub struct DeviceSnapshot {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub product_name: Option<String>,
    pub model_name: &'static str,
    pub connection: ConnectionKind,
    pub protocol: ProtocolKind,
    pub supported_rates: Vec<PollingRate>,
    pub current_rate: Option<PollingRate>,
    pub read_error: Option<String>,
}

impl DeviceSnapshot {
    pub fn supported_rates_text(&self) -> String {
        format_supported_rates(&self.supported_rates)
    }
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

            let current_rate = match info.open_device(&self.api) {
                Ok(device) => {
                    let live = GwolvesDevice::new(device, model, connection);
                    match live.read_rate() {
                        Ok(rate) => (Some(rate), None),
                        Err(err) => (None, Some(err.to_string())),
                    }
                }
                Err(err) => (None, Some(err.to_string())),
            };

            let snapshot = DeviceSnapshot {
                path: info.path().to_string_lossy().into_owned(),
                vid,
                pid,
                product_name: info.product_string().map(ToOwned::to_owned),
                model_name: model.name,
                connection,
                protocol: model.protocol,
                supported_rates: model.supported_rates(connection).to_vec(),
                current_rate: current_rate.0,
                read_error: current_rate.1,
            };

            merge_snapshot(&mut devices, snapshot);
        }
        Ok(devices)
    }

    pub fn open_first_supported(&self) -> Result<GwolvesDevice> {
        for info in self.api.device_list() {
            let vid = info.vendor_id();
            let pid = info.product_id();
            let Some((model, connection)) = find_model(vid, pid) else {
                continue;
            };
            if let Ok(device) = info.open_device(&self.api) {
                let live = GwolvesDevice::new(device, model, connection);
                if live.read_rate().is_ok() {
                    return Ok(live);
                }
            }
        }
        Err(anyhow!("no supported G-Wolves Fenrir-family device found"))
    }

    pub fn open_by_vid_pid(&self, target_vid: u16, target_pid: u16) -> Result<GwolvesDevice> {
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
            let device = info
                .open_device(&self.api)
                .with_context(|| format!("failed to open {:04x}:{:04x}", vid, pid))?;
            let live = GwolvesDevice::new(device, model, connection);
            match live.read_rate() {
                Ok(_) => return Ok(live),
                Err(err) => last_error = Some(err.to_string()),
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
    }
}

pub struct GwolvesDevice {
    device: HidDevice,
    model: &'static ModelInfo,
    connection: ConnectionKind,
}

impl GwolvesDevice {
    pub fn new(device: HidDevice, model: &'static ModelInfo, connection: ConnectionKind) -> Self {
        Self {
            device,
            model,
            connection,
        }
    }

    pub fn model(&self) -> &'static ModelInfo {
        self.model
    }

    pub fn connection(&self) -> ConnectionKind {
        self.connection
    }

    pub fn read_rate(&self) -> Result<PollingRate> {
        match self.model.protocol {
            ProtocolKind::Feature64 { .. } => self.read_feature64_rate(),
            ProtocolKind::Eeprom16 => self.read_eeprom16_rate(),
        }
    }

    pub fn set_rate(&self, rate: PollingRate) -> Result<()> {
        self.model.require_rate(self.connection, rate)?;
        match self.model.protocol {
            ProtocolKind::Feature64 { .. } => self.set_feature64_rate(rate),
            ProtocolKind::Eeprom16 => self.set_eeprom16_rate(rate),
        }
    }

    fn read_feature64_rate(&self) -> Result<PollingRate> {
        let report =
            build_feature64_get_rate(self.model.protocol, self.connection, DEFAULT_PROFILE);
        self.send_feature_payload(&report)?;
        thread::sleep(Duration::from_millis(20));
        let response = self.get_feature_payload()?;
        let code = self.extract_feature64_rate_code(&response)?;
        self.model
            .protocol
            .rate_from_code(code)
            .ok_or_else(|| anyhow!("device returned unknown polling-rate code {code}"))
    }

    fn set_feature64_rate(&self, rate: PollingRate) -> Result<()> {
        let report =
            build_feature64_set_rate(self.model.protocol, self.connection, DEFAULT_PROFILE, rate);
        self.send_feature_payload(&report)?;
        thread::sleep(Duration::from_millis(20));
        let _ = self.get_feature_payload();
        Ok(())
    }

    fn read_eeprom16_rate(&self) -> Result<PollingRate> {
        let response = self.transact_report8(build_eeprom16_get_rate(), 8, 50, 5)?;
        let code = *response
            .get(5)
            .ok_or_else(|| anyhow!("short report8 response while reading polling rate"))?;
        self.model
            .protocol
            .rate_from_code(code)
            .ok_or_else(|| anyhow!("device returned unknown polling-rate code {code}"))
    }

    fn set_eeprom16_rate(&self, rate: PollingRate) -> Result<()> {
        let report = build_eeprom16_set_rate(rate);
        let _ = self.transact_report8(report, 7, 50, 5)?;
        Ok(())
    }

    fn extract_feature64_rate_code(&self, response: &[u8]) -> Result<u8> {
        let hid_index = if response.first().copied().unwrap_or_default() >= 0xA0 {
            1
        } else {
            0
        };
        match self.model.protocol {
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
            ProtocolKind::Eeprom16 => unreachable!("feature64 parser called for eeprom protocol"),
        }
    }

    fn send_feature_payload(&self, payload: &[u8; FEATURE_PAYLOAD_LEN]) -> Result<()> {
        let mut buffer = [0; FEATURE_BUFFER_LEN];
        buffer[0] = FEATURE_REPORT_ID;
        buffer[1..].copy_from_slice(payload);
        self.device
            .send_feature_report(&buffer)
            .context("failed to send feature report")
    }

    fn get_feature_payload(&self) -> Result<Vec<u8>> {
        let mut buffer = [0; FEATURE_BUFFER_LEN];
        buffer[0] = FEATURE_REPORT_ID;
        let len = self
            .device
            .get_feature_report(&mut buffer)
            .context("failed to receive feature report")?;
        Ok(normalize_feature_payload(&buffer[..len]))
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

pub fn normalize_feature_payload(raw: &[u8]) -> Vec<u8> {
    if raw.len() == FEATURE_BUFFER_LEN && raw[0] == FEATURE_REPORT_ID {
        raw[1..].to_vec()
    } else {
        raw.to_vec()
    }
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
        let mut raw = vec![0; FEATURE_BUFFER_LEN];
        raw[1] = 0xA1;
        raw[9] = 1;
        let payload = normalize_feature_payload(&raw);
        assert_eq!(payload.len(), FEATURE_PAYLOAD_LEN);
        assert_eq!(payload[0], 0xA1);
        assert_eq!(payload[8], 1);
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
}
