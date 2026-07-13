use std::{thread, time::Duration};

use anyhow::{Result, anyhow};

use crate::devices::{
    BatteryStatus, ConnectionKind, DpiConfig, EEPROM16_DPI_ENTRY_LEN, EEPROM16_DPI_TABLE_OFFSET,
    EEPROM16_MAX_TRANSFER_LEN, IPI_PIX_DPI_COLOR_BYTES, IPI_PIX_DPI_LEVELS, ModelInfo, PollingRate,
    ProtocolKind, build_eeprom16_dpi_entry, build_eeprom16_get_battery, build_eeprom16_get_rate,
    build_eeprom16_read, build_eeprom16_set_rate, build_eeprom16_write,
    build_feature64_get_battery, build_feature64_get_rate, build_feature64_set_rate,
    build_ipi_pix_v1_get_basic_info, build_ipi_pix_v1_get_dpi_config, build_ipi_pix_v1_get_rate,
    build_ipi_pix_v1_set_current_dpi, build_ipi_pix_v1_set_rate, eeprom16_block_checksum,
    eeprom16_raw_to_dpi, gwolves_charge_state_from_status, ipi_pix_v1_rate_from_sensor_byte,
    ipi_pix_v1_raw_to_dpi,
};

const DEFAULT_PROFILE: u8 = 1;
pub const FEATURE_REPORT_ID: u8 = 0;
pub const REPORT8_ID: u8 = 8;
pub const FEATURE_PAYLOAD_LEN: usize = 64;
pub const REPORT8_PAYLOAD_LEN: usize = 16;
pub const REPORT8_BUFFER_LEN: usize = REPORT8_PAYLOAD_LEN + 1;
pub const IPI_PIX_PAYLOAD_LEN: usize = 63;
pub const RAZER_V1_REPORT_ID: u8 = 0;
pub const RAZER_V1_PAYLOAD_LEN: usize = 90;
pub const HIDPP_SHORT_ID: u8 = 0x10;
pub const HIDPP_LONG_ID: u8 = 0x11;
pub const HIDPP_SHORT_PAYLOAD_LEN: usize = 6;
pub const HIDPP_LONG_PAYLOAD_LEN: usize = 19;

const HIDPP_DEVICE_INDEX: u8 = 0x01;
const HIDPP_ROOT_FEATURE_INDEX: u8 = 0x00;
const HIDPP_SW_ID: u8 = 0x0D;
const HIDPP_REPORT_RATE_FEATURE_ID: u16 = 0x8060;
const HIDPP_UNIFIED_BATTERY_FEATURE_ID: u16 = 0x1004;
const HIDPP_BATTERY_STATUS_FEATURE_ID: u16 = 0x1000;
const HIDPP_REPORT_RATE_FALLBACK_INDEX: u8 = 0x0A;
const HIDPP_SCAN_DISCOVERY_TIMEOUT_MS: i32 = 500;
const HIDPP_BATTERY_TIMEOUT_MS: i32 = 1_500;

const RAZER_STATUS_NEW: u8 = 0x00;
const RAZER_STATUS_BUSY: u8 = 0x01;
const RAZER_STATUS_SUCCESS: u8 = 0x02;
const RAZER_STATUS_FAILURE: u8 = 0x03;

pub trait DeviceTransport {
    fn send_feature_payload(&self, report_id: u8, payload: &[u8]) -> Result<()>;
    fn get_feature_payload(&self, report_id: u8, payload_len: usize) -> Result<Vec<u8>>;
    fn write_output_payload(&self, report_id: u8, payload: &[u8]) -> Result<usize>;
    fn read_input_payload(
        &self,
        report_id: u8,
        payload_len: usize,
        timeout_ms: i32,
    ) -> Result<Vec<u8>>;

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

pub struct ProtocolDevice<'a, T: DeviceTransport + ?Sized> {
    transport: &'a T,
    model: &'static ModelInfo,
    connection: ConnectionKind,
    protocol: ProtocolKind,
}

impl<'a, T: DeviceTransport + ?Sized> ProtocolDevice<'a, T> {
    pub const fn new(
        transport: &'a T,
        model: &'static ModelInfo,
        connection: ConnectionKind,
        protocol: ProtocolKind,
    ) -> Self {
        Self {
            transport,
            model,
            connection,
            protocol,
        }
    }

    pub fn read_rate(&self) -> Result<PollingRate> {
        match self.protocol {
            ProtocolKind::Feature64 { .. } => self.read_feature64_rate(),
            ProtocolKind::Eeprom16 => self.read_eeprom16_rate(),
            ProtocolKind::IpiPixV1 { .. } => self.read_ipi_pix_v1_rate(),
            ProtocolKind::LogitechHidpp => Err(anyhow!(
                "current polling-rate read is not implemented for logitech hid++ protocol"
            )),
            ProtocolKind::RazerV1 { .. } => Err(anyhow!(
                "current polling-rate read is not implemented for razer v1 protocol"
            )),
        }
    }

    pub fn read_battery(&self) -> Result<BatteryStatus> {
        match self.protocol {
            ProtocolKind::Feature64 { .. } => self.read_feature64_battery(),
            ProtocolKind::Eeprom16 => self.read_eeprom16_battery(),
            ProtocolKind::IpiPixV1 { .. } => self.read_ipi_pix_v1_battery(),
            ProtocolKind::LogitechHidpp => self.read_logitech_hidpp_battery(),
            ProtocolKind::RazerV1 { .. } => self.read_razer_v1_battery(),
        }
    }

    pub fn set_rate(&self, rate: PollingRate) -> Result<()> {
        self.model.require_rate(self.connection, rate)?;
        match self.protocol {
            ProtocolKind::Feature64 { .. } => self.set_feature64_rate(rate),
            ProtocolKind::Eeprom16 => self.set_eeprom16_rate(rate),
            ProtocolKind::IpiPixV1 { .. } => self.set_ipi_pix_v1_rate(rate),
            ProtocolKind::LogitechHidpp => self.set_logitech_hidpp_rate(rate),
            ProtocolKind::RazerV1 { .. } => self.set_razer_v1_rate(rate),
        }
    }

    pub fn read_dpi(&self) -> Result<u16> {
        match self.protocol {
            ProtocolKind::IpiPixV1 { .. } => self.read_ipi_pix_v1_dpi(),
            ProtocolKind::Eeprom16 => self.read_eeprom16_dpi(),
            ProtocolKind::Feature64 { .. }
            | ProtocolKind::LogitechHidpp
            | ProtocolKind::RazerV1 { .. } => Err(anyhow!(
                "DPI read is not implemented for {} protocol",
                self.protocol
            )),
        }
    }

    pub fn set_dpi(&self, dpi: u16) -> Result<()> {
        match self.protocol {
            ProtocolKind::IpiPixV1 { .. } => self.set_ipi_pix_v1_dpi(dpi),
            ProtocolKind::Eeprom16 => self.set_eeprom16_dpi(dpi),
            ProtocolKind::Feature64 { .. }
            | ProtocolKind::LogitechHidpp
            | ProtocolKind::RazerV1 { .. } => Err(anyhow!(
                "DPI writes are not implemented for {} protocol",
                self.protocol
            )),
        }
    }

    pub fn probe_control(&self) -> Result<()> {
        match self.protocol {
            ProtocolKind::Feature64 { .. }
            | ProtocolKind::Eeprom16
            | ProtocolKind::IpiPixV1 { .. } => self.read_rate().map(|_| ()),
            ProtocolKind::LogitechHidpp => self
                .discover_logitech_hidpp_feature(
                    HIDPP_REPORT_RATE_FEATURE_ID,
                    HIDPP_SCAN_DISCOVERY_TIMEOUT_MS,
                )
                .map(|_| ())
                .ok_or_else(|| anyhow!("logitech hid++ control feature did not answer")),
            ProtocolKind::RazerV1 { tx_id, .. } => {
                let command = build_razer_v1_command(tx_id, 0x07, 0x80, 0x02, &[0x00, 0x00]);
                self.send_razer_v1_and_recv(&command).map(|_| ())
            }
        }
    }

    fn read_feature64_rate(&self) -> Result<PollingRate> {
        let report = build_feature64_get_rate(self.protocol, self.connection, DEFAULT_PROFILE);
        self.transport
            .send_feature_payload(FEATURE_REPORT_ID, &report)?;
        self.transport.sleep(Duration::from_millis(20));
        let response = self
            .transport
            .get_feature_payload(FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN)?;
        let code = self.extract_feature64_rate_code(&response)?;
        self.protocol
            .rate_from_code(code)
            .ok_or_else(|| anyhow!("device returned unknown polling-rate code {code}"))
    }

    fn set_feature64_rate(&self, rate: PollingRate) -> Result<()> {
        let report =
            build_feature64_set_rate(self.protocol, self.connection, DEFAULT_PROFILE, rate);
        self.transport
            .send_feature_payload(FEATURE_REPORT_ID, &report)?;
        self.transport.sleep(Duration::from_millis(20));
        let _ = self
            .transport
            .get_feature_payload(FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN);
        Ok(())
    }

    fn read_feature64_battery(&self) -> Result<BatteryStatus> {
        let report = build_feature64_get_battery(self.protocol, self.connection);
        self.transport
            .send_feature_payload(FEATURE_REPORT_ID, &report)?;
        let delay = match self.protocol {
            ProtocolKind::Feature64 {
                new_protocol: true, ..
            } => 100,
            ProtocolKind::Feature64 {
                new_protocol: false,
                ..
            } => 50,
            ProtocolKind::Eeprom16
            | ProtocolKind::IpiPixV1 { .. }
            | ProtocolKind::LogitechHidpp
            | ProtocolKind::RazerV1 { .. } => unreachable!(),
        };
        self.transport.sleep(Duration::from_millis(delay));
        let response = self
            .transport
            .get_feature_payload(FEATURE_REPORT_ID, FEATURE_PAYLOAD_LEN)?;
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

    fn read_eeprom16_battery(&self) -> Result<BatteryStatus> {
        let response = self.transact_report8(build_eeprom16_get_battery(), 4, 50, 5)?;
        let level = response
            .get(5)
            .copied()
            .ok_or_else(|| anyhow!("short report8 response while reading battery level"))?
            .min(100);
        let raw_state = response
            .get(6)
            .copied()
            .ok_or_else(|| anyhow!("short report8 response while reading charging state"))?;
        let charge_state = match raw_state {
            0 => crate::devices::ChargeState::Discharging,
            1 => crate::devices::ChargeState::Charging,
            raw => crate::devices::ChargeState::Raw(raw),
        };
        Ok(BatteryStatus::with_raw_state(
            level,
            charge_state,
            raw_state,
        ))
    }

    fn read_eeprom16_dpi(&self) -> Result<u16> {
        let (active_index, dpi_levels) = self.read_eeprom16_dpi_config()?;
        dpi_levels
            .get(active_index)
            .copied()
            .ok_or_else(|| anyhow!("missing active report8 DPI level {}", active_index + 1))
    }

    fn set_eeprom16_dpi(&self, dpi: u16) -> Result<()> {
        let (active_index, _) = self.read_eeprom16_dpi_config()?;
        let entry = build_eeprom16_dpi_entry(dpi)?;
        let offset = EEPROM16_DPI_TABLE_OFFSET
            .checked_add((active_index as u8).saturating_mul(EEPROM16_DPI_ENTRY_LEN))
            .ok_or_else(|| anyhow!("report8 DPI table offset overflow"))?;
        let report = build_eeprom16_write(offset, &entry)?;
        if let Err(write_error) = self.transact_report8(report, 7, 50, 5) {
            return match self.read_eeprom16_dpi() {
                Ok(after) if after == dpi => Ok(()),
                Ok(after) => Err(anyhow!(
                    "{write_error}; active DPI readback is {after}, expected {dpi}"
                )),
                Err(read_error) => Err(anyhow!(
                    "{write_error}; active DPI readback failed: {read_error}"
                )),
            };
        }
        Ok(())
    }

    fn read_eeprom16_dpi_config(&self) -> Result<(usize, Vec<u16>)> {
        let header = self.read_eeprom16_bytes(0, 6)?;
        let level_count = usize::from(
            *header
                .get(2)
                .ok_or_else(|| anyhow!("short report8 header while reading DPI level count"))?,
        );
        if level_count == 0 {
            return Err(anyhow!("device returned zero report8 DPI levels"));
        }

        let active_index = usize::from(
            *header
                .get(4)
                .ok_or_else(|| anyhow!("short report8 header while reading active DPI level"))?,
        );
        if active_index >= level_count {
            return Err(anyhow!(
                "device active report8 DPI level {} is outside {} configured level(s)",
                active_index,
                level_count
            ));
        }

        let table_len = level_count
            .checked_mul(usize::from(EEPROM16_DPI_ENTRY_LEN))
            .ok_or_else(|| anyhow!("report8 DPI table length overflow"))?;
        let table = self.read_eeprom16_bytes(EEPROM16_DPI_TABLE_OFFSET, table_len)?;
        let dpi_levels = table
            .chunks_exact(usize::from(EEPROM16_DPI_ENTRY_LEN))
            .map(decode_eeprom16_dpi_entry)
            .collect::<Result<Vec<_>>>()?;
        Ok((active_index, dpi_levels))
    }

    fn read_eeprom16_bytes(&self, offset: u8, len: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(len);
        let mut cursor = 0usize;
        while cursor < len {
            let chunk_len = (len - cursor).min(EEPROM16_MAX_TRANSFER_LEN);
            let request_offset = offset
                .checked_add(cursor as u8)
                .ok_or_else(|| anyhow!("report8 eeprom offset overflow"))?;
            let report = build_eeprom16_read(request_offset, chunk_len as u8)?;
            let response = self.transact_report8(report, 8, 50, 5)?;
            if response.get(3).copied() != Some(request_offset)
                || response.get(4).copied() != Some(chunk_len as u8)
            {
                return Err(anyhow!(
                    "report8 eeprom response did not match offset {request_offset} length {chunk_len}"
                ));
            }
            let end = 5 + chunk_len;
            if response.len() < end {
                return Err(anyhow!(
                    "short report8 eeprom response at offset {request_offset}: {} byte(s)",
                    response.len()
                ));
            }
            bytes.extend_from_slice(&response[5..end]);
            cursor += chunk_len;
        }
        Ok(bytes)
    }

    fn read_ipi_pix_v1_rate(&self) -> Result<PollingRate> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi reader called for non-ipi protocol")
        };
        let report = build_ipi_pix_v1_get_rate();
        self.transport.send_feature_payload(report_id, &report)?;
        self.transport.sleep(Duration::from_millis(50));
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
        self.transport.send_feature_payload(report_id, &report)?;
        self.transport.sleep(Duration::from_millis(60));
        let _ = self.get_ipi_pix_v1_response(report_id, &report);
        Ok(())
    }

    fn read_ipi_pix_v1_battery(&self) -> Result<BatteryStatus> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi battery reader called for non-ipi protocol")
        };
        let report = build_ipi_pix_v1_get_basic_info();
        self.transport.send_feature_payload(report_id, &report)?;
        self.transport.sleep(Duration::from_millis(100));
        let response = self.get_ipi_pix_v1_response(report_id, &report)?;
        let level = response
            .get(5)
            .copied()
            .ok_or_else(|| anyhow!("short ipi pix v1 response while reading battery"))?;
        Ok(BatteryStatus::level_only(level))
    }

    fn read_ipi_pix_v1_dpi(&self) -> Result<u16> {
        self.read_ipi_pix_v1_dpi_config()?.current_dpi()
    }

    fn set_ipi_pix_v1_dpi(&self, dpi: u16) -> Result<()> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi dpi writer called for non-ipi protocol")
        };
        let config = self.read_ipi_pix_v1_dpi_config()?;
        let color = config.current_color()?;
        let report = build_ipi_pix_v1_set_current_dpi(config.profile, dpi, color)?;
        self.transport.send_feature_payload(report_id, &report)?;
        for _ in 0..3 {
            self.transport.sleep(Duration::from_millis(20));
            if self.get_ipi_pix_v1_response(report_id, &report).is_ok() {
                return Ok(());
            }
        }
        Err(anyhow!(
            "timed out waiting for ipi pix v1 DPI write response"
        ))
    }

    fn read_ipi_pix_v1_dpi_config(&self) -> Result<DpiConfig> {
        let ProtocolKind::IpiPixV1 { report_id } = self.protocol else {
            unreachable!("ipi dpi reader called for non-ipi protocol")
        };
        let sensor_report = build_ipi_pix_v1_get_rate();
        self.transport
            .send_feature_payload(report_id, &sensor_report)?;
        self.transport.sleep(Duration::from_millis(50));
        let sensor = self.get_ipi_pix_v1_response(report_id, &sensor_report)?;
        let profile = sensor
            .get(7)
            .copied()
            .ok_or_else(|| anyhow!("short ipi pix v1 response while reading DPI profile"))?;

        let mut bytes = Vec::new();
        for page in 0..6 {
            let report = build_ipi_pix_v1_get_dpi_config(page);
            self.transport.send_feature_payload(report_id, &report)?;
            self.transport.sleep(Duration::from_millis(20));
            let response = self.get_ipi_pix_v1_response(report_id, &report)?;
            let start = if page == 0 { 6 } else { 5 };
            let chunk = response.get(start..15).ok_or_else(|| {
                anyhow!("short ipi pix v1 response while reading DPI page {page}")
            })?;
            bytes.extend_from_slice(chunk);
        }

        if bytes.len() < 16 + IPI_PIX_DPI_COLOR_BYTES {
            return Err(anyhow!(
                "short ipi pix v1 DPI config response: {} byte(s)",
                bytes.len()
            ));
        }

        let current_dpi = bytes[..16]
            .chunks_exact(2)
            .take(IPI_PIX_DPI_LEVELS)
            .map(|chunk| {
                let raw = u16::from(chunk[0]) | (u16::from(chunk[1]) << 8);
                ipi_pix_v1_raw_to_dpi(raw)
            })
            .collect();
        let dpi_color = bytes[16..16 + IPI_PIX_DPI_COLOR_BYTES]
            .chunks_exact(3)
            .map(|chunk| [chunk[0], chunk[1], chunk[2]])
            .collect();

        Ok(DpiConfig {
            profile,
            current_dpi,
            dpi_color,
        })
    }

    fn set_logitech_hidpp_rate(&self, rate: PollingRate) -> Result<()> {
        let rate_code = rate
            .logitech_hidpp_code()
            .ok_or_else(|| anyhow!("unsupported logitech hid++ polling rate {rate}"))?;
        let feature_index = self
            .discover_logitech_hidpp_feature(HIDPP_REPORT_RATE_FEATURE_ID, 2_000)
            .unwrap_or(HIDPP_REPORT_RATE_FALLBACK_INDEX);
        self.transport.write_output_payload(
            HIDPP_SHORT_ID,
            &[0xFF, feature_index, 0x2E, rate_code, 0x00, 0x00],
        )?;
        Ok(())
    }

    fn read_logitech_hidpp_battery(&self) -> Result<BatteryStatus> {
        let feature_index = self
            .discover_logitech_hidpp_feature(
                HIDPP_UNIFIED_BATTERY_FEATURE_ID,
                HIDPP_SCAN_DISCOVERY_TIMEOUT_MS,
            )
            .or_else(|| {
                self.discover_logitech_hidpp_feature(
                    HIDPP_BATTERY_STATUS_FEATURE_ID,
                    HIDPP_SCAN_DISCOVERY_TIMEOUT_MS,
                )
            })
            .ok_or_else(|| anyhow!("battery feature not found on this logitech hid++ device"))?;
        let query = [HIDPP_DEVICE_INDEX, feature_index, 0x1D, 0x00, 0x00, 0x00];
        self.transport
            .write_output_payload(HIDPP_SHORT_ID, &query)?;
        self.transport.sleep(Duration::from_millis(500));
        self.transport
            .write_output_payload(HIDPP_SHORT_ID, &query)?;

        let response = self
            .read_logitech_hidpp_matching(
                |payload| {
                    payload.first() == Some(&HIDPP_DEVICE_INDEX)
                        && payload.get(1) == Some(&feature_index)
                },
                HIDPP_BATTERY_TIMEOUT_MS,
            )?
            .ok_or_else(|| {
                anyhow!("battery query timed out; device may be asleep or out of range")
            })?;
        let level = response.get(3).copied().unwrap_or_default().min(100);
        let raw_state = response.get(5).copied().unwrap_or_default();
        Ok(BatteryStatus::with_raw_state(
            level,
            logitech_charge_state_from_status(raw_state),
            raw_state,
        ))
    }

    fn set_razer_v1_rate(&self, rate: PollingRate) -> Result<()> {
        let ProtocolKind::RazerV1 {
            tx_id,
            polling_reversed,
        } = self.protocol
        else {
            unreachable!("razer writer called for non-razer protocol")
        };
        let mask = rate.razer_v1_mask(polling_reversed);
        let command = build_razer_v1_command(tx_id, 0x00, 0x40, 0x02, &[0x01, mask]);
        self.transport
            .send_feature_payload(RAZER_V1_REPORT_ID, &command)?;
        Ok(())
    }

    fn read_razer_v1_battery(&self) -> Result<BatteryStatus> {
        let ProtocolKind::RazerV1 { tx_id, .. } = self.protocol else {
            unreachable!("razer battery reader called for non-razer protocol")
        };
        let level_command = build_razer_v1_command(tx_id, 0x07, 0x80, 0x02, &[0x00, 0x00]);
        let level_response = self.send_razer_v1_and_recv(&level_command)?;
        let raw_level = *level_response
            .get(9)
            .ok_or_else(|| anyhow!("short razer v1 battery level response"))?;
        let level = ((u16::from(raw_level) * 100 + 127) / 255) as u8;

        self.transport.sleep(Duration::from_millis(60));
        let charge_command = build_razer_v1_command(tx_id, 0x07, 0x84, 0x02, &[0x00, 0x00]);
        let charge_response = self.send_razer_v1_and_recv(&charge_command)?;
        let raw_state = *charge_response
            .get(9)
            .ok_or_else(|| anyhow!("short razer v1 charging-state response"))?;
        let charge_state = if raw_state == 0 {
            crate::devices::ChargeState::Discharging
        } else {
            crate::devices::ChargeState::Charging
        };
        Ok(BatteryStatus::with_raw_state(
            level,
            charge_state,
            raw_state,
        ))
    }

    fn discover_logitech_hidpp_feature(&self, feature_id: u16, timeout_ms: i32) -> Option<u8> {
        let payload = [
            HIDPP_DEVICE_INDEX,
            HIDPP_ROOT_FEATURE_INDEX,
            HIDPP_SW_ID,
            (feature_id >> 8) as u8,
            feature_id as u8,
            0x00,
        ];
        self.transport
            .write_output_payload(HIDPP_SHORT_ID, &payload)
            .ok()?;
        let response = self
            .read_logitech_hidpp_matching(
                |payload| {
                    payload.first() == Some(&HIDPP_DEVICE_INDEX)
                        && payload.get(1) == Some(&HIDPP_ROOT_FEATURE_INDEX)
                },
                timeout_ms,
            )
            .ok()??;
        match response.get(3).copied() {
            Some(0) | None => None,
            Some(index) => Some(index),
        }
    }

    fn read_logitech_hidpp_matching(
        &self,
        matches_response: impl Fn(&[u8]) -> bool,
        timeout_ms: i32,
    ) -> Result<Option<Vec<u8>>> {
        let step_ms = 50;
        let attempts = (timeout_ms.max(1) as usize).div_ceil(step_ms).max(1);
        for _ in 0..attempts {
            let response = self.transport.read_input_payload(
                HIDPP_LONG_ID,
                HIDPP_LONG_PAYLOAD_LEN,
                step_ms as i32,
            )?;
            if response.is_empty() {
                continue;
            }
            let response = normalize_logitech_hidpp_payload(&response);
            if matches_response(&response) {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    fn send_razer_v1_and_recv(&self, command: &[u8; RAZER_V1_PAYLOAD_LEN]) -> Result<Vec<u8>> {
        self.transport
            .send_feature_payload(RAZER_V1_REPORT_ID, command)?;
        for attempt in 0..10 {
            self.transport
                .sleep(Duration::from_millis(20 + attempt * 10));
            let response = self
                .transport
                .get_feature_payload(RAZER_V1_REPORT_ID, RAZER_V1_PAYLOAD_LEN)?;
            if response.is_empty() {
                continue;
            }
            match response[0] {
                RAZER_STATUS_SUCCESS => return Ok(response),
                RAZER_STATUS_FAILURE => return Err(anyhow!("razer v1 command returned failure")),
                RAZER_STATUS_BUSY | RAZER_STATUS_NEW => continue,
                status => return Err(anyhow!("razer v1 command returned unknown status {status}")),
            }
        }
        Err(anyhow!("timed out waiting for razer v1 command response"))
    }

    fn get_ipi_pix_v1_response(
        &self,
        report_id: u8,
        request: &[u8; IPI_PIX_PAYLOAD_LEN],
    ) -> Result<Vec<u8>> {
        for _ in 0..4 {
            let response = self
                .transport
                .get_feature_payload(report_id, IPI_PIX_PAYLOAD_LEN)?;
            let response = normalize_ipi_pix_v1_payload(&response);
            if response.get(3) == request.get(3) && response.len() > 6 {
                return Ok(response);
            }
            self.transport.sleep(Duration::from_millis(50));
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
            self.transport.write_output_payload(REPORT8_ID, &payload)?;
            let response = self.transport.read_input_payload(
                REPORT8_ID,
                REPORT8_PAYLOAD_LEN,
                step_delay_ms,
            )?;
            if response.first() == Some(&response_command) {
                return Ok(response);
            }
        }
        Err(anyhow!(
            "timed out waiting for report8 command {response_command} after {attempts} attempts"
        ))
    }

    fn flush_report8(&self) {
        while matches!(
            self.transport
                .read_input_payload(REPORT8_ID, REPORT8_PAYLOAD_LEN, 1),
            Ok(response) if !response.is_empty()
        ) {}
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
            ProtocolKind::Eeprom16
            | ProtocolKind::IpiPixV1 { .. }
            | ProtocolKind::LogitechHidpp
            | ProtocolKind::RazerV1 { .. } => {
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
            ProtocolKind::Eeprom16
            | ProtocolKind::IpiPixV1 { .. }
            | ProtocolKind::LogitechHidpp
            | ProtocolKind::RazerV1 { .. } => {
                unreachable!("feature64 parser called for non-feature64 protocol")
            }
        }
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

pub fn normalize_report_payload(raw: &[u8], report_id: u8, payload_len: usize) -> Vec<u8> {
    if raw.len() == payload_len + 1 && raw[0] == report_id {
        raw[1..].to_vec()
    } else {
        raw.iter().copied().take(payload_len).collect()
    }
}

pub fn normalize_report8_payload(raw: &[u8]) -> Vec<u8> {
    normalize_report_payload(raw, REPORT8_ID, REPORT8_PAYLOAD_LEN)
}

pub fn decode_eeprom16_dpi_entry(entry: &[u8]) -> Result<u16> {
    if entry.len() != usize::from(EEPROM16_DPI_ENTRY_LEN) {
        return Err(anyhow!(
            "report8 DPI entries must be {} byte(s), got {}",
            EEPROM16_DPI_ENTRY_LEN,
            entry.len()
        ));
    }
    let expected = eeprom16_block_checksum(&entry[..3]);
    if entry[3] != expected {
        return Err(anyhow!(
            "report8 DPI entry checksum mismatch: expected 0x{expected:02x}, got 0x{:02x}",
            entry[3]
        ));
    }
    if entry[0] != entry[1] {
        return Err(anyhow!(
            "report8 DPI entry has different X/Y raw values: {} != {}",
            entry[0],
            entry[1]
        ));
    }
    if entry[2] != 0 {
        return Err(anyhow!(
            "unsupported report8 DPI high-byte encoding 0x{:02x}",
            entry[2]
        ));
    }
    Ok(eeprom16_raw_to_dpi(entry[0]))
}

pub fn normalize_logitech_hidpp_payload(raw: &[u8]) -> Vec<u8> {
    if raw.first() == Some(&HIDPP_SHORT_ID) || raw.first() == Some(&HIDPP_LONG_ID) {
        raw[1..].to_vec()
    } else {
        raw.to_vec()
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

pub fn build_razer_v1_command(
    tx_id: u8,
    class: u8,
    id: u8,
    size: u8,
    args: &[u8],
) -> [u8; RAZER_V1_PAYLOAD_LEN] {
    let mut payload = [0; RAZER_V1_PAYLOAD_LEN];
    payload[0] = RAZER_STATUS_NEW;
    payload[1] = tx_id;
    payload[5] = size;
    payload[6] = class;
    payload[7] = id;
    for (index, byte) in args.iter().take(80).enumerate() {
        payload[8 + index] = *byte;
    }
    payload[88] = razer_v1_crc(&payload);
    payload
}

pub fn razer_v1_crc(payload: &[u8; RAZER_V1_PAYLOAD_LEN]) -> u8 {
    payload[2..88].iter().fold(0, |crc, byte| crc ^ byte)
}

pub fn logitech_charge_state_from_status(status: u8) -> crate::devices::ChargeState {
    match status {
        0x00 | 0x04 => crate::devices::ChargeState::Discharging,
        0x01 | 0x02 => crate::devices::ChargeState::Charging,
        0x03 => crate::devices::ChargeState::Full,
        raw => crate::devices::ChargeState::Raw(raw),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, time::Duration};

    use super::*;
    use crate::devices::{
        GWOLVES_VENDOR_ID, LOGITECH_VENDOR_ID, RATES_1K, RATES_8K, RAZER_VENDOR_ID,
    };

    static TEST_MODEL: ModelInfo = ModelInfo {
        vendor_name: "Test",
        name: "Mouse",
        vid: GWOLVES_VENDOR_ID,
        wired_pid: Some(0x1234),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::Feature64 {
            new_protocol: true,
            wired_device_id: 2,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    };

    static LOGITECH_TEST_MODEL: ModelInfo = ModelInfo {
        vendor_name: "Logitech",
        name: "Test Logitech",
        vid: LOGITECH_VENDOR_ID,
        wired_pid: Some(0xC09B),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::LogitechHidpp,
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_1K,
    };

    static RAZER_TEST_MODEL: ModelInfo = ModelInfo {
        vendor_name: "Razer",
        name: "Test Razer",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x00C0),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    };

    #[derive(Debug)]
    struct ScriptedTransport {
        feature_reads: RefCell<VecDeque<Vec<u8>>>,
        feature_writes: RefCell<Vec<(u8, Vec<u8>)>>,
        input_reads: RefCell<VecDeque<Vec<u8>>>,
        output_writes: RefCell<Vec<(u8, Vec<u8>)>>,
    }

    impl ScriptedTransport {
        fn with_feature_reads(reads: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                feature_reads: RefCell::new(reads.into_iter().collect()),
                feature_writes: RefCell::new(Vec::new()),
                input_reads: RefCell::new(VecDeque::new()),
                output_writes: RefCell::new(Vec::new()),
            }
        }
    }

    fn report8_response(offset: u8, data: &[u8]) -> Vec<u8> {
        let mut response = vec![0; REPORT8_PAYLOAD_LEN];
        response[0] = 8;
        response[3] = offset;
        response[4] = data.len() as u8;
        response[5..5 + data.len()].copy_from_slice(data);
        response
    }

    impl DeviceTransport for ScriptedTransport {
        fn send_feature_payload(&self, report_id: u8, payload: &[u8]) -> Result<()> {
            self.feature_writes
                .borrow_mut()
                .push((report_id, payload.to_vec()));
            Ok(())
        }

        fn get_feature_payload(&self, _report_id: u8, _payload_len: usize) -> Result<Vec<u8>> {
            self.feature_reads
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted feature response"))
        }

        fn write_output_payload(&self, report_id: u8, payload: &[u8]) -> Result<usize> {
            self.output_writes
                .borrow_mut()
                .push((report_id, payload.to_vec()));
            Ok(payload.len() + 1)
        }

        fn read_input_payload(
            &self,
            _report_id: u8,
            _payload_len: usize,
            _timeout_ms: i32,
        ) -> Result<Vec<u8>> {
            if let Some(response) = self.input_reads.borrow_mut().pop_front() {
                Ok(response)
            } else {
                Ok(Vec::new())
            }
        }

        fn sleep(&self, _duration: Duration) {}
    }

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
    fn strips_logitech_hidpp_report_id() {
        let payload = normalize_logitech_hidpp_payload(&[HIDPP_LONG_ID, 1, 0, 0, 7]);
        assert_eq!(payload, vec![1, 0, 0, 7]);
    }

    #[test]
    fn computes_report8_crc_like_web_driver() {
        let mut report = build_eeprom16_set_rate(PollingRate::Hz8000);
        write_report8_crc(&mut report);
        assert_eq!(report[15], 239);
    }

    #[test]
    fn simulated_feature64_rate_read_uses_protocol_transport() {
        let mut response = vec![0; FEATURE_PAYLOAD_LEN];
        response[0] = 0xA1;
        response[7] = 128;
        let transport = ScriptedTransport::with_feature_reads([response]);
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Receiver,
            ProtocolKind::Feature64 {
                new_protocol: true,
                wired_device_id: 2,
            },
        );

        assert_eq!(protocol.read_rate().unwrap(), PollingRate::Hz8000);
        let writes = transport.feature_writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, FEATURE_REPORT_ID);
        assert_eq!(&writes[0].1[2..7], &[2, 2, 1, 128, 1]);
    }

    #[test]
    fn simulated_eeprom_rate_set_writes_crc_report() {
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([Vec::new(), vec![7; REPORT8_PAYLOAD_LEN]])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::Eeprom16,
        );

        protocol.set_rate(PollingRate::Hz8000).unwrap();
        let writes = transport.output_writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, REPORT8_ID);
        assert_eq!(&writes[0].1[0..7], &[7, 0, 0, 0, 2, 64, 21]);
        assert_eq!(writes[0].1[15], 239);
    }

    #[test]
    fn report8_timeout_is_bounded_by_attempt_count() {
        let transport = ScriptedTransport::with_feature_reads(Vec::new());
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::Eeprom16,
        );

        let error = protocol.read_rate().unwrap_err().to_string();

        assert!(error.ends_with("after 5 attempts"));
        assert_eq!(transport.output_writes.borrow().len(), 5);
    }

    #[test]
    fn simulated_eeprom_battery_reads_level_and_charging_state() {
        let mut response = vec![0; REPORT8_PAYLOAD_LEN];
        response[0] = 4;
        response[4] = 2;
        response[5] = 15;
        response[6] = 0;
        response[7] = 0x0f;
        response[8] = 0x7c;
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([Vec::new(), response])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Receiver,
            ProtocolKind::Eeprom16,
        );

        let battery = protocol.read_battery().unwrap();
        assert_eq!(battery.level_percent, Some(15));
        assert_eq!(
            battery.charge_state,
            crate::devices::ChargeState::Discharging
        );
        assert_eq!(battery.raw_state, Some(0));
        let writes = transport.output_writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, REPORT8_ID);
        assert_eq!(writes[0].1[0], 4);
    }

    #[test]
    fn simulated_eeprom_dpi_read_uses_active_level() {
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([
                Vec::new(),
                report8_response(0, &[1, 0x54, 5, 0x50, 1, 0x54]),
                Vec::new(),
                report8_response(12, &[0x0f, 0x0f, 0, 0x37, 0x17, 0x17, 0, 0x27, 0x1f, 0x1f]),
                Vec::new(),
                report8_response(22, &[0, 0x17, 0x3f, 0x3f, 0, 0xd7, 0x6f, 0x6f, 0, 0x77]),
            ])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::Eeprom16,
        );

        assert_eq!(protocol.read_dpi().unwrap(), 1200);
    }

    #[test]
    fn simulated_eeprom_dpi_set_writes_active_entry() {
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([
                Vec::new(),
                report8_response(0, &[1, 0x54, 5, 0x50, 1, 0x54]),
                Vec::new(),
                report8_response(12, &[0x0f, 0x0f, 0, 0x37, 0x17, 0x17, 0, 0x27, 0x1f, 0x1f]),
                Vec::new(),
                report8_response(22, &[0, 0x17, 0x3f, 0x3f, 0, 0xd7, 0x6f, 0x6f, 0, 0x77]),
                Vec::new(),
                vec![7; REPORT8_PAYLOAD_LEN],
            ])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::Eeprom16,
        );

        protocol.set_dpi(3200).unwrap();
        let writes = transport.output_writes.borrow();
        assert_eq!(writes.len(), 4);
        assert_eq!(&writes[3].1[0..9], &[7, 0, 0, 16, 4, 63, 63, 0, 215]);
    }

    #[test]
    fn simulated_eeprom_dpi_set_accepts_matching_readback_after_missing_ack() {
        let header = &[1, 0x54, 1, 0x54, 0, 0x55];
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([
                Vec::new(),
                report8_response(0, header),
                Vec::new(),
                report8_response(12, &[0x0f, 0x0f, 0, 0x37]),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                report8_response(0, header),
                Vec::new(),
                report8_response(12, &[0x3f, 0x3f, 0, 0xd7]),
            ])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::Eeprom16,
        );

        protocol.set_dpi(3200).unwrap();
        let writes = transport.output_writes.borrow();
        let dpi_writes = writes
            .iter()
            .filter(|(_, payload)| payload.first() == Some(&7))
            .collect::<Vec<_>>();
        assert_eq!(dpi_writes.len(), 5);
        assert!(
            dpi_writes
                .iter()
                .all(|(_, payload)| payload[3] == 12 && payload[5] == 0x3f)
        );
    }

    #[test]
    fn builds_razer_v1_command_crc_like_web_driver() {
        let command = build_razer_v1_command(0x1F, 0x00, 0x40, 0x02, &[0x01, 0x01]);
        assert_eq!(&command[0..10], &[0, 0x1F, 0, 0, 0, 2, 0, 0x40, 1, 1]);
        assert_eq!(command[88], razer_v1_crc(&command));
    }

    #[test]
    fn simulated_razer_rate_set_uses_reversed_mask() {
        let transport = ScriptedTransport::with_feature_reads([]);
        let protocol = ProtocolDevice::new(
            &transport,
            &RAZER_TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::RazerV1 {
                tx_id: 0x1F,
                polling_reversed: true,
            },
        );

        protocol.set_rate(PollingRate::Hz8000).unwrap();
        let writes = transport.feature_writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, RAZER_V1_REPORT_ID);
        assert_eq!(&writes[0].1[5..10], &[2, 0, 0x40, 1, 1]);
        assert_eq!(
            writes[0].1[88],
            razer_v1_crc(writes[0].1.as_slice().try_into().unwrap())
        );
    }

    #[test]
    fn simulated_razer_battery_reads_level_and_charging() {
        let mut level = vec![0; RAZER_V1_PAYLOAD_LEN];
        level[0] = RAZER_STATUS_SUCCESS;
        level[9] = 204;
        let mut charging = vec![0; RAZER_V1_PAYLOAD_LEN];
        charging[0] = RAZER_STATUS_SUCCESS;
        charging[9] = 1;
        let transport = ScriptedTransport::with_feature_reads([level, charging]);
        let protocol = ProtocolDevice::new(
            &transport,
            &RAZER_TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::RazerV1 {
                tx_id: 0x1F,
                polling_reversed: true,
            },
        );

        let battery = protocol.read_battery().unwrap();
        assert_eq!(battery.level_percent, Some(80));
        assert_eq!(battery.charge_state, crate::devices::ChargeState::Charging);
        assert_eq!(transport.feature_writes.borrow().len(), 2);
    }

    #[test]
    fn simulated_logitech_rate_set_discovers_report_rate_feature() {
        let mut report_rate_feature = vec![0; HIDPP_LONG_PAYLOAD_LEN];
        report_rate_feature[0] = HIDPP_DEVICE_INDEX;
        report_rate_feature[1] = HIDPP_ROOT_FEATURE_INDEX;
        report_rate_feature[3] = 0x0A;
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([report_rate_feature])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &LOGITECH_TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::LogitechHidpp,
        );

        protocol.set_rate(PollingRate::Hz1000).unwrap();
        let writes = transport.output_writes.borrow();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, HIDPP_SHORT_ID);
        assert_eq!(
            writes[0].1,
            vec![HIDPP_DEVICE_INDEX, 0, 0x0D, 0x80, 0x60, 0]
        );
        assert_eq!(writes[1].1, vec![0xFF, 0x0A, 0x2E, 0x01, 0, 0]);
    }

    #[test]
    fn simulated_logitech_battery_discovers_unified_feature() {
        let mut battery_feature = vec![0; HIDPP_LONG_PAYLOAD_LEN];
        battery_feature[0] = HIDPP_DEVICE_INDEX;
        battery_feature[1] = HIDPP_ROOT_FEATURE_INDEX;
        battery_feature[3] = 0x07;
        let mut battery_response = vec![0; HIDPP_LONG_PAYLOAD_LEN];
        battery_response[0] = HIDPP_DEVICE_INDEX;
        battery_response[1] = 0x07;
        battery_response[3] = 84;
        battery_response[5] = 1;
        let transport = ScriptedTransport {
            feature_reads: RefCell::new(VecDeque::new()),
            feature_writes: RefCell::new(Vec::new()),
            input_reads: RefCell::new(VecDeque::from([battery_feature, battery_response])),
            output_writes: RefCell::new(Vec::new()),
        };
        let protocol = ProtocolDevice::new(
            &transport,
            &LOGITECH_TEST_MODEL,
            ConnectionKind::Wired,
            ProtocolKind::LogitechHidpp,
        );

        let battery = protocol.read_battery().unwrap();
        assert_eq!(battery.level_percent, Some(84));
        assert_eq!(battery.charge_state, crate::devices::ChargeState::Charging);
        let writes = transport.output_writes.borrow();
        assert_eq!(writes.len(), 3);
        assert_eq!(
            writes[0].1,
            vec![HIDPP_DEVICE_INDEX, 0, 0x0D, 0x10, 0x04, 0]
        );
        assert_eq!(writes[1].1, vec![HIDPP_DEVICE_INDEX, 0x07, 0x1D, 0, 0, 0]);
        assert_eq!(writes[2].1, vec![HIDPP_DEVICE_INDEX, 0x07, 0x1D, 0, 0, 0]);
    }
}
