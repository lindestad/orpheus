use std::{fmt, str::FromStr};

use anyhow::{Result, anyhow, bail};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as DeError, Visitor},
};

pub const GWOLVES_VENDOR_ID: u16 = 0x33E4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PollingRate {
    Hz125,
    Hz250,
    Hz500,
    Hz1000,
    Hz2000,
    Hz4000,
    Hz8000,
}

impl PollingRate {
    pub const fn hz(self) -> u16 {
        match self {
            Self::Hz125 => 125,
            Self::Hz250 => 250,
            Self::Hz500 => 500,
            Self::Hz1000 => 1000,
            Self::Hz2000 => 2000,
            Self::Hz4000 => 4000,
            Self::Hz8000 => 8000,
        }
    }

    pub const fn feature64_code(self) -> u8 {
        match self {
            Self::Hz125 => 8,
            Self::Hz250 => 4,
            Self::Hz500 => 2,
            Self::Hz1000 => 1,
            Self::Hz2000 => 32,
            Self::Hz4000 => 64,
            Self::Hz8000 => 128,
        }
    }

    pub const fn eeprom16_code(self) -> u8 {
        match self {
            Self::Hz125 => 8,
            Self::Hz250 => 4,
            Self::Hz500 => 2,
            Self::Hz1000 => 1,
            Self::Hz2000 => 16,
            Self::Hz4000 => 32,
            Self::Hz8000 => 64,
        }
    }

    pub fn from_feature64_code(code: u8) -> Option<Self> {
        match code {
            8 => Some(Self::Hz125),
            4 => Some(Self::Hz250),
            2 => Some(Self::Hz500),
            1 => Some(Self::Hz1000),
            32 => Some(Self::Hz2000),
            64 => Some(Self::Hz4000),
            128 => Some(Self::Hz8000),
            _ => None,
        }
    }

    pub fn from_eeprom16_code(code: u8) -> Option<Self> {
        match code {
            8 => Some(Self::Hz125),
            4 => Some(Self::Hz250),
            2 => Some(Self::Hz500),
            1 => Some(Self::Hz1000),
            16 => Some(Self::Hz2000),
            32 => Some(Self::Hz4000),
            64 => Some(Self::Hz8000),
            _ => None,
        }
    }
}

impl fmt::Display for PollingRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz", self.hz())
    }
}

impl Serialize for PollingRate {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u16(self.hz())
    }
}

impl<'de> Deserialize<'de> for PollingRate {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PollingRateVisitor;

        impl<'de> Visitor<'de> for PollingRateVisitor {
            type Value = PollingRate;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a polling rate like 1000, \"1000\", or \"8k\"")
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: DeError,
            {
                let value = u16::try_from(value)
                    .map_err(|_| E::custom(format!("polling rate is too large: {value}")))?;
                PollingRate::from_str(&value.to_string()).map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: DeError,
            {
                if value < 0 {
                    return Err(E::custom(format!("polling rate must be positive: {value}")));
                }
                self.visit_u64(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: DeError,
            {
                PollingRate::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(PollingRateVisitor)
    }
}

impl FromStr for PollingRate {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace("hz", "");
        let normalized = normalized.trim();
        match normalized {
            "125" => Ok(Self::Hz125),
            "250" => Ok(Self::Hz250),
            "500" => Ok(Self::Hz500),
            "1000" | "1k" => Ok(Self::Hz1000),
            "2000" | "2k" => Ok(Self::Hz2000),
            "4000" | "4k" => Ok(Self::Hz4000),
            "8000" | "8k" => Ok(Self::Hz8000),
            _ => Err(anyhow!("unknown polling rate: {raw}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolKind {
    Feature64 {
        new_protocol: bool,
        wired_device_id: u8,
    },
    Eeprom16,
}

impl ProtocolKind {
    pub const fn rate_code(self, rate: PollingRate) -> u8 {
        match self {
            Self::Feature64 { .. } => rate.feature64_code(),
            Self::Eeprom16 => rate.eeprom16_code(),
        }
    }

    pub fn rate_from_code(self, code: u8) -> Option<PollingRate> {
        match self {
            Self::Feature64 { .. } => PollingRate::from_feature64_code(code),
            Self::Eeprom16 => PollingRate::from_eeprom16_code(code),
        }
    }
}

impl fmt::Display for ProtocolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Feature64 {
                new_protocol: true, ..
            } => write!(f, "feature64 new"),
            Self::Feature64 {
                new_protocol: false,
                ..
            } => write!(f, "feature64 old"),
            Self::Eeprom16 => write!(f, "report8 eeprom"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionKind {
    Wired,
    Wireless,
    Receiver,
}

impl ConnectionKind {
    pub const fn is_wired(self) -> bool {
        matches!(self, Self::Wired)
    }
}

impl fmt::Display for ConnectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wired => write!(f, "wired"),
            Self::Wireless => write!(f, "wireless"),
            Self::Receiver => write!(f, "receiver"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ModelInfo {
    pub name: &'static str,
    pub vid: u16,
    pub wired_pid: Option<u16>,
    pub wireless_pid: Option<u16>,
    pub receiver_pid: Option<u16>,
    pub receiver_idvd_pid: Option<u16>,
    pub protocol: ProtocolKind,
    pub wired_rates: &'static [PollingRate],
    pub wireless_rates: &'static [PollingRate],
    pub receiver_rates: &'static [PollingRate],
}

impl ModelInfo {
    pub fn match_connection(&'static self, vid: u16, pid: u16) -> Option<ConnectionKind> {
        if vid != self.vid {
            return None;
        }
        if self.wired_pid == Some(pid) {
            return Some(ConnectionKind::Wired);
        }
        if self.receiver_pid == Some(pid) || self.receiver_idvd_pid == Some(pid) {
            return Some(ConnectionKind::Receiver);
        }
        if self.wireless_pid == Some(pid) {
            return Some(ConnectionKind::Wireless);
        }
        None
    }

    pub fn supported_rates(self, connection: ConnectionKind) -> &'static [PollingRate] {
        match connection {
            ConnectionKind::Wired => self.wired_rates,
            ConnectionKind::Wireless => self.wireless_rates,
            ConnectionKind::Receiver => self.receiver_rates,
        }
    }

    pub fn require_rate(self, connection: ConnectionKind, rate: PollingRate) -> Result<()> {
        if self.supported_rates(connection).contains(&rate) {
            Ok(())
        } else {
            bail!(
                "{} does not list {} for {} mode",
                self.name,
                rate,
                connection
            )
        }
    }

    pub fn protocol_candidates(self) -> Vec<ProtocolKind> {
        let mut protocols = vec![self.protocol];
        if let ProtocolKind::Feature64 {
            new_protocol: false,
            wired_device_id,
        } = self.protocol
        {
            protocols.push(ProtocolKind::Feature64 {
                new_protocol: true,
                wired_device_id,
            });
        }
        protocols
    }
}

pub const RATES_1K: &[PollingRate] = &[
    PollingRate::Hz125,
    PollingRate::Hz250,
    PollingRate::Hz500,
    PollingRate::Hz1000,
];

pub const RATES_4K: &[PollingRate] = &[
    PollingRate::Hz125,
    PollingRate::Hz500,
    PollingRate::Hz1000,
    PollingRate::Hz2000,
    PollingRate::Hz4000,
];

pub const RATES_8K: &[PollingRate] = &[
    PollingRate::Hz125,
    PollingRate::Hz250,
    PollingRate::Hz500,
    PollingRate::Hz1000,
    PollingRate::Hz2000,
    PollingRate::Hz4000,
    PollingRate::Hz8000,
];

pub const FENRIR_MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: "Fenrir Pro",
        vid: GWOLVES_VENDOR_ID,
        wired_pid: Some(0x3608),
        wireless_pid: Some(0x3617),
        receiver_pid: Some(0x3617),
        receiver_idvd_pid: Some(0x3817),
        protocol: ProtocolKind::Feature64 {
            new_protocol: true,
            wired_device_id: 2,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        name: "Fenrir Pro",
        vid: GWOLVES_VENDOR_ID,
        wired_pid: Some(0x3619),
        wireless_pid: Some(0x3854),
        receiver_pid: Some(0x3854),
        receiver_idvd_pid: None,
        protocol: ProtocolKind::Eeprom16,
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        name: "Fenrir",
        vid: GWOLVES_VENDOR_ID,
        wired_pid: Some(0x3508),
        wireless_pid: Some(0x3517),
        receiver_pid: Some(0x3517),
        receiver_idvd_pid: Some(0x0017),
        protocol: ProtocolKind::Feature64 {
            new_protocol: false,
            wired_device_id: 2,
        },
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        name: "Fenir Max",
        vid: GWOLVES_VENDOR_ID,
        wired_pid: Some(0x3708),
        wireless_pid: Some(0x3717),
        receiver_pid: Some(0x3717),
        receiver_idvd_pid: Some(0x0017),
        protocol: ProtocolKind::Feature64 {
            new_protocol: false,
            wired_device_id: 2,
        },
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_8K,
    },
];

pub fn find_model(vid: u16, pid: u16) -> Option<(&'static ModelInfo, ConnectionKind)> {
    FENRIR_MODELS.iter().find_map(|model| {
        model
            .match_connection(vid, pid)
            .map(|connection| (model, connection))
    })
}

pub fn format_supported_rates(rates: &[PollingRate]) -> String {
    rates
        .iter()
        .map(|rate| rate.hz().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn build_feature64_get_rate(
    protocol: ProtocolKind,
    connection: ConnectionKind,
    profile: u8,
) -> [u8; 64] {
    let mut report = [0; 64];
    match protocol {
        ProtocolKind::Feature64 {
            new_protocol: true,
            wired_device_id,
        } => {
            report[2] = wired_device_id;
            report[3] = 2;
            report[4] = 1;
            report[5] = 128;
            report[6] = profile;
        }
        ProtocolKind::Feature64 {
            new_protocol: false,
            ..
        } => {
            report[1] = 2;
            report[2] = 130;
            if !connection.is_wired() {
                report[3] = 1;
            }
        }
        ProtocolKind::Eeprom16 => {}
    }
    report
}

pub fn build_feature64_set_rate(
    protocol: ProtocolKind,
    connection: ConnectionKind,
    profile: u8,
    rate: PollingRate,
) -> [u8; 64] {
    let mut report = [0; 64];
    let code = protocol.rate_code(rate);
    match protocol {
        ProtocolKind::Feature64 {
            new_protocol: true,
            wired_device_id,
        } => {
            report[2] = wired_device_id;
            report[3] = 2;
            report[4] = 1;
            report[5] = 0;
            report[6] = profile;
            report[7] = code;
        }
        ProtocolKind::Feature64 {
            new_protocol: false,
            ..
        } => {
            report[1] = 2;
            report[2] = 2;
            if !connection.is_wired() {
                report[3] = 1;
            }
            report[4] = code;
        }
        ProtocolKind::Eeprom16 => {}
    }
    report
}

pub fn build_eeprom16_get_rate() -> [u8; 16] {
    let mut report = [0; 16];
    report[0] = 8;
    report[1] = 0;
    report[2] = 0;
    report[3] = 0;
    report[4] = 2;
    report
}

pub fn build_eeprom16_set_rate(rate: PollingRate) -> [u8; 16] {
    let mut report = [0; 16];
    let code = rate.eeprom16_code();
    report[0] = 7;
    report[1] = 0;
    report[2] = 0;
    report[3] = 0;
    report[4] = 2;
    report[5] = code;
    report[6] = 85u8.wrapping_sub(code);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rates() {
        assert_eq!("1000".parse::<PollingRate>().unwrap(), PollingRate::Hz1000);
        assert_eq!("8k".parse::<PollingRate>().unwrap(), PollingRate::Hz8000);
        assert!("1234".parse::<PollingRate>().is_err());
    }

    #[test]
    fn maps_feature64_rate_codes() {
        assert_eq!(PollingRate::Hz1000.feature64_code(), 1);
        assert_eq!(PollingRate::Hz2000.feature64_code(), 32);
        assert_eq!(PollingRate::Hz4000.feature64_code(), 64);
        assert_eq!(PollingRate::Hz8000.feature64_code(), 128);
        assert_eq!(
            PollingRate::from_feature64_code(128),
            Some(PollingRate::Hz8000)
        );
    }

    #[test]
    fn maps_eeprom16_rate_codes() {
        assert_eq!(PollingRate::Hz1000.eeprom16_code(), 1);
        assert_eq!(PollingRate::Hz2000.eeprom16_code(), 16);
        assert_eq!(PollingRate::Hz4000.eeprom16_code(), 32);
        assert_eq!(PollingRate::Hz8000.eeprom16_code(), 64);
        assert_eq!(
            PollingRate::from_eeprom16_code(64),
            Some(PollingRate::Hz8000)
        );
    }

    #[test]
    fn finds_fenrir_models() {
        let (model, connection) = find_model(GWOLVES_VENDOR_ID, 0x3854).unwrap();
        assert_eq!(model.name, "Fenrir Pro");
        assert_eq!(connection, ConnectionKind::Receiver);

        let (model, connection) = find_model(GWOLVES_VENDOR_ID, 0x3508).unwrap();
        assert_eq!(model.name, "Fenrir");
        assert_eq!(connection, ConnectionKind::Wired);
    }

    #[test]
    fn builds_feature64_new_rate_reports() {
        let report = build_feature64_set_rate(
            ProtocolKind::Feature64 {
                new_protocol: true,
                wired_device_id: 2,
            },
            ConnectionKind::Receiver,
            1,
            PollingRate::Hz8000,
        );
        assert_eq!(&report[2..8], &[2, 2, 1, 0, 1, 128]);
    }

    #[test]
    fn old_feature64_models_also_probe_new_protocol() {
        let (model, _) = find_model(GWOLVES_VENDOR_ID, 0x3517).unwrap();
        assert_eq!(
            model.protocol_candidates(),
            vec![
                ProtocolKind::Feature64 {
                    new_protocol: false,
                    wired_device_id: 2,
                },
                ProtocolKind::Feature64 {
                    new_protocol: true,
                    wired_device_id: 2,
                },
            ]
        );
    }

    #[test]
    fn builds_eeprom16_rate_reports() {
        let report = build_eeprom16_set_rate(PollingRate::Hz8000);
        assert_eq!(&report[0..7], &[7, 0, 0, 0, 2, 64, 21]);
    }
}
