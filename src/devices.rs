use std::{fmt, str::FromStr};

use anyhow::{Result, anyhow, bail};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as DeError, Visitor},
};

pub const GWOLVES_VENDOR_ID: u16 = 0x33E4;
pub const IPI_VENDOR_ID: u16 = 0x372E;
pub const LOGITECH_VENDOR_ID: u16 = 0x046D;
pub const RAZER_VENDOR_ID: u16 = 0x1532;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
    Unknown,
    Raw(u8),
}

impl ChargeState {
    pub const fn is_charging_like(self) -> Option<bool> {
        match self {
            Self::Charging | Self::Full => Some(true),
            Self::Discharging => Some(false),
            Self::Unknown | Self::Raw(_) => None,
        }
    }
}

impl fmt::Display for ChargeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discharging => write!(f, "discharging"),
            Self::Charging => write!(f, "charging"),
            Self::Full => write!(f, "full"),
            Self::Unknown => write!(f, "unknown"),
            Self::Raw(raw) => write!(f, "raw {raw}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatteryStatus {
    pub level_percent: Option<u8>,
    pub charge_state: ChargeState,
    pub raw_state: Option<u8>,
}

impl BatteryStatus {
    pub const fn level_only(level_percent: u8) -> Self {
        Self {
            level_percent: Some(level_percent),
            charge_state: ChargeState::Unknown,
            raw_state: None,
        }
    }

    pub const fn with_raw_state(
        level_percent: u8,
        charge_state: ChargeState,
        raw_state: u8,
    ) -> Self {
        Self {
            level_percent: Some(level_percent),
            charge_state,
            raw_state: Some(raw_state),
        }
    }

    pub const fn is_charging_like(self) -> Option<bool> {
        self.charge_state.is_charging_like()
    }
}

impl fmt::Display for BatteryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.level_percent, self.charge_state, self.raw_state) {
            (Some(level), ChargeState::Unknown, None) => write!(f, "{level}%"),
            (Some(level), ChargeState::Raw(raw), _) => write!(f, "{level}% raw {raw}"),
            (Some(level), state, Some(raw)) => write!(f, "{level}% {state} (raw {raw})"),
            (Some(level), state, None) => write!(f, "{level}% {state}"),
            (None, ChargeState::Unknown, None) => write!(f, "unknown"),
            (None, ChargeState::Raw(raw), _) => write!(f, "raw {raw}"),
            (None, state, Some(raw)) => write!(f, "{state} (raw {raw})"),
            (None, state, None) => write!(f, "{state}"),
        }
    }
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

    pub const fn ipi_pix_v1_code(self) -> u8 {
        match self {
            Self::Hz1000 => 0,
            Self::Hz500 => 1,
            Self::Hz250 => 2,
            Self::Hz125 => 3,
            Self::Hz8000 => 4,
            Self::Hz4000 => 5,
            Self::Hz2000 => 6,
        }
    }

    pub fn from_ipi_pix_v1_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Hz1000),
            1 => Some(Self::Hz500),
            2 => Some(Self::Hz250),
            3 => Some(Self::Hz125),
            4 => Some(Self::Hz8000),
            5 => Some(Self::Hz4000),
            6 => Some(Self::Hz2000),
            _ => None,
        }
    }

    pub const fn logitech_hidpp_code(self) -> Option<u8> {
        match self {
            Self::Hz1000 => Some(0x01),
            Self::Hz500 => Some(0x02),
            Self::Hz250 => Some(0x04),
            Self::Hz125 => Some(0x08),
            Self::Hz2000 | Self::Hz4000 | Self::Hz8000 => None,
        }
    }

    pub const fn razer_v1_mask(self, reversed: bool) -> u8 {
        match (self, reversed) {
            (Self::Hz125, false) => 0x01,
            (Self::Hz250, false) => 0x02,
            (Self::Hz500, false) => 0x04,
            (Self::Hz1000, false) => 0x08,
            (Self::Hz2000, false) => 0x10,
            (Self::Hz4000, false) => 0x20,
            (Self::Hz8000, false) => 0x40,
            (Self::Hz125, true) => 0x40,
            (Self::Hz250, true) => 0x20,
            (Self::Hz500, true) => 0x10,
            (Self::Hz1000, true) => 0x08,
            (Self::Hz2000, true) => 0x04,
            (Self::Hz4000, true) => 0x02,
            (Self::Hz8000, true) => 0x01,
        }
    }
}

pub const fn gwolves_charge_state_from_status(status: u8) -> ChargeState {
    match status {
        0 => ChargeState::Discharging,
        1 => ChargeState::Charging,
        2 => ChargeState::Full,
        raw => ChargeState::Raw(raw),
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
    IpiPixV1 {
        report_id: u8,
    },
    LogitechHidpp,
    RazerV1 {
        tx_id: u8,
        polling_reversed: bool,
    },
}

impl ProtocolKind {
    pub const fn rate_code(self, rate: PollingRate) -> u8 {
        match self {
            Self::Feature64 { .. } => rate.feature64_code(),
            Self::Eeprom16 => rate.eeprom16_code(),
            Self::IpiPixV1 { .. } => rate.ipi_pix_v1_code(),
            Self::LogitechHidpp => match rate.logitech_hidpp_code() {
                Some(code) => code,
                None => 0,
            },
            Self::RazerV1 {
                polling_reversed, ..
            } => rate.razer_v1_mask(polling_reversed),
        }
    }

    pub fn rate_from_code(self, code: u8) -> Option<PollingRate> {
        match self {
            Self::Feature64 { .. } => PollingRate::from_feature64_code(code),
            Self::Eeprom16 => PollingRate::from_eeprom16_code(code),
            Self::IpiPixV1 { .. } => PollingRate::from_ipi_pix_v1_code(code),
            Self::LogitechHidpp | Self::RazerV1 { .. } => None,
        }
    }

    pub const fn supports_rate_read(self) -> bool {
        match self {
            Self::Feature64 { .. } | Self::Eeprom16 | Self::IpiPixV1 { .. } => true,
            Self::LogitechHidpp | Self::RazerV1 { .. } => false,
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
            Self::IpiPixV1 { .. } => write!(f, "ipi pix v1"),
            Self::LogitechHidpp => write!(f, "logitech hid++"),
            Self::RazerV1 {
                polling_reversed: true,
                ..
            } => write!(f, "razer v1 reversed"),
            Self::RazerV1 {
                polling_reversed: false,
                ..
            } => write!(f, "razer v1"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
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
    pub vendor_name: &'static str,
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
        vendor_name: "G-Wolves",
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
        vendor_name: "G-Wolves",
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
        vendor_name: "G-Wolves",
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
        vendor_name: "G-Wolves",
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

pub const IPI_PIAO_MODELS: &[ModelInfo] = &[
    ModelInfo {
        vendor_name: "IPI",
        name: "Piao",
        vid: IPI_VENDOR_ID,
        wired_pid: Some(0x1015),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::IpiPixV1 { report_id: 3 },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "IPI",
        name: "Piao",
        vid: IPI_VENDOR_ID,
        wired_pid: Some(0x1028),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::IpiPixV1 { report_id: 3 },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "IPI",
        name: "Piao",
        vid: IPI_VENDOR_ID,
        wired_pid: Some(0x1056),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::IpiPixV1 { report_id: 3 },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "IPI",
        name: "Piao",
        vid: IPI_VENDOR_ID,
        wired_pid: None,
        wireless_pid: Some(0x1014),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::IpiPixV1 { report_id: 3 },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
];

pub const LOGITECH_MODELS: &[ModelInfo] = &[
    ModelInfo {
        vendor_name: "Logitech",
        name: "Lightspeed Receiver",
        vid: LOGITECH_VENDOR_ID,
        wired_pid: None,
        wireless_pid: None,
        receiver_pid: Some(0xC547),
        receiver_idvd_pid: None,
        protocol: ProtocolKind::LogitechHidpp,
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_1K,
    },
    ModelInfo {
        vendor_name: "Logitech",
        name: "PRO X SUPERLIGHT",
        vid: LOGITECH_VENDOR_ID,
        wired_pid: Some(0xC094),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::LogitechHidpp,
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_1K,
    },
    ModelInfo {
        vendor_name: "Logitech",
        name: "PRO X SUPERLIGHT 2",
        vid: LOGITECH_VENDOR_ID,
        wired_pid: Some(0xC09B),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::LogitechHidpp,
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_1K,
    },
    ModelInfo {
        vendor_name: "Logitech",
        name: "PRO X Superlight Wireless",
        vid: LOGITECH_VENDOR_ID,
        wired_pid: None,
        wireless_pid: Some(0x4093),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::LogitechHidpp,
        wired_rates: RATES_1K,
        wireless_rates: RATES_1K,
        receiver_rates: RATES_1K,
    },
];

pub const RAZER_MODELS: &[ModelInfo] = &[
    ModelInfo {
        vendor_name: "Razer",
        name: "Viper V3 Pro",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x00C0),
        wireless_pid: Some(0x00C1),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "Viper V3 HyperSpeed",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x00B6),
        wireless_pid: Some(0x00B8),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "Viper V2 Pro",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x00A5),
        wireless_pid: Some(0x00A6),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "DeathAdder V4 Pro",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x00BE),
        wireless_pid: Some(0x00BF),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "DeathAdder V3",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x0090),
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
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "DeathAdder V3 Pro",
        vid: RAZER_VENDOR_ID,
        wired_pid: None,
        wireless_pid: Some(0x0092),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "DeathAdder V2",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x007A),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0xFF,
            polling_reversed: false,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "DeathAdder V2 Pro",
        vid: RAZER_VENDOR_ID,
        wired_pid: None,
        wireless_pid: Some(0x007C),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x3F,
            polling_reversed: false,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "Basilisk V3",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x0099),
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
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "Basilisk V3 Pro",
        vid: RAZER_VENDOR_ID,
        wired_pid: None,
        wireless_pid: Some(0x008E),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: true,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "Basilisk Ultimate",
        vid: RAZER_VENDOR_ID,
        wired_pid: None,
        wireless_pid: Some(0x0078),
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: false,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
    ModelInfo {
        vendor_name: "Razer",
        name: "Naga X",
        vid: RAZER_VENDOR_ID,
        wired_pid: Some(0x0086),
        wireless_pid: None,
        receiver_pid: None,
        receiver_idvd_pid: None,
        protocol: ProtocolKind::RazerV1 {
            tx_id: 0x1F,
            polling_reversed: false,
        },
        wired_rates: RATES_8K,
        wireless_rates: RATES_8K,
        receiver_rates: RATES_8K,
    },
];

pub fn find_model(vid: u16, pid: u16) -> Option<(&'static ModelInfo, ConnectionKind)> {
    FENRIR_MODELS
        .iter()
        .chain(IPI_PIAO_MODELS)
        .chain(LOGITECH_MODELS)
        .chain(RAZER_MODELS)
        .find_map(|model| {
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
        ProtocolKind::IpiPixV1 { .. } => {}
        ProtocolKind::LogitechHidpp | ProtocolKind::RazerV1 { .. } => {}
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
        ProtocolKind::IpiPixV1 { .. } => {}
        ProtocolKind::LogitechHidpp | ProtocolKind::RazerV1 { .. } => {}
    }
    report
}

pub fn build_feature64_get_battery(protocol: ProtocolKind, connection: ConnectionKind) -> [u8; 64] {
    let mut report = [0; 64];
    match protocol {
        ProtocolKind::Feature64 {
            new_protocol: true,
            wired_device_id,
        } => {
            report[2] = wired_device_id;
            report[3] = 2;
            report[5] = 131;
        }
        ProtocolKind::Feature64 {
            new_protocol: false,
            ..
        } => {
            report[1] = 2;
            report[2] = 143;
            if !connection.is_wired() {
                report[3] = 1;
            }
        }
        ProtocolKind::Eeprom16
        | ProtocolKind::IpiPixV1 { .. }
        | ProtocolKind::LogitechHidpp
        | ProtocolKind::RazerV1 { .. } => {}
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

pub fn build_ipi_pix_v1_get_rate() -> [u8; 63] {
    let mut report = [0; 63];
    report[0..6].copy_from_slice(&[0, 80, 0, 10, 79, 64]);
    write_ipi_checksum(&mut report);
    report
}

pub fn build_ipi_pix_v1_get_basic_info() -> [u8; 63] {
    let mut report = [0; 63];
    report[0..6].copy_from_slice(&[0, 80, 0, 2, 79, 129]);
    write_ipi_checksum(&mut report);
    report
}

pub fn build_ipi_pix_v1_set_rate(connection: ConnectionKind, rate: PollingRate) -> [u8; 63] {
    let mut report = [0; 63];
    let code = rate.ipi_pix_v1_code();
    let encoded = if connection == ConnectionKind::Wireless && code >= 4 {
        code
    } else {
        code << 4 | code
    };
    report[0..7].copy_from_slice(&[0, 80, 1, 49, 53, 1, encoded]);
    write_ipi_checksum(&mut report);
    report
}

pub fn ipi_pix_v1_rate_from_sensor_byte(
    connection: ConnectionKind,
    raw: u8,
) -> Option<PollingRate> {
    let code = if connection == ConnectionKind::Wireless {
        raw & 0x07
    } else {
        (raw & 0x70) >> 4
    };
    PollingRate::from_ipi_pix_v1_code(code)
}

pub fn write_ipi_checksum(payload: &mut [u8]) {
    payload[0] = payload
        .iter()
        .skip(1)
        .fold(0u8, |checksum, byte| checksum.wrapping_add(*byte));
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
    fn maps_ipi_pix_v1_rate_codes() {
        assert_eq!(PollingRate::Hz1000.ipi_pix_v1_code(), 0);
        assert_eq!(PollingRate::Hz8000.ipi_pix_v1_code(), 4);
        assert_eq!(
            PollingRate::from_ipi_pix_v1_code(6),
            Some(PollingRate::Hz2000)
        );
    }

    #[test]
    fn maps_logitech_hidpp_rate_codes() {
        assert_eq!(PollingRate::Hz1000.logitech_hidpp_code(), Some(0x01));
        assert_eq!(PollingRate::Hz500.logitech_hidpp_code(), Some(0x02));
        assert_eq!(PollingRate::Hz250.logitech_hidpp_code(), Some(0x04));
        assert_eq!(PollingRate::Hz125.logitech_hidpp_code(), Some(0x08));
        assert_eq!(PollingRate::Hz4000.logitech_hidpp_code(), None);
    }

    #[test]
    fn maps_razer_v1_polling_masks() {
        assert_eq!(PollingRate::Hz125.razer_v1_mask(false), 0x01);
        assert_eq!(PollingRate::Hz8000.razer_v1_mask(false), 0x40);
        assert_eq!(PollingRate::Hz125.razer_v1_mask(true), 0x40);
        assert_eq!(PollingRate::Hz8000.razer_v1_mask(true), 0x01);
        assert_eq!(PollingRate::Hz1000.razer_v1_mask(true), 0x08);
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
    fn finds_ipi_piao_models() {
        let (model, connection) = find_model(IPI_VENDOR_ID, 0x1014).unwrap();
        assert_eq!(model.vendor_name, "IPI");
        assert_eq!(model.name, "Piao");
        assert_eq!(connection, ConnectionKind::Wireless);
    }

    #[test]
    fn finds_logitech_models() {
        let (model, connection) = find_model(LOGITECH_VENDOR_ID, 0xC547).unwrap();
        assert_eq!(model.name, "Lightspeed Receiver");
        assert_eq!(connection, ConnectionKind::Receiver);
        assert_eq!(model.protocol, ProtocolKind::LogitechHidpp);

        let (model, connection) = find_model(LOGITECH_VENDOR_ID, 0xC09B).unwrap();
        assert_eq!(model.name, "PRO X SUPERLIGHT 2");
        assert_eq!(connection, ConnectionKind::Wired);
    }

    #[test]
    fn finds_razer_models() {
        let (model, connection) = find_model(RAZER_VENDOR_ID, 0x00C1).unwrap();
        assert_eq!(model.name, "Viper V3 Pro");
        assert_eq!(connection, ConnectionKind::Wireless);
        assert_eq!(
            model.protocol,
            ProtocolKind::RazerV1 {
                tx_id: 0x1F,
                polling_reversed: true
            }
        );

        let (model, connection) = find_model(RAZER_VENDOR_ID, 0x007C).unwrap();
        assert_eq!(model.name, "DeathAdder V2 Pro");
        assert_eq!(connection, ConnectionKind::Wireless);
        assert_eq!(
            model.protocol,
            ProtocolKind::RazerV1 {
                tx_id: 0x3F,
                polling_reversed: false
            }
        );
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
    fn builds_feature64_battery_reports() {
        let new_report = build_feature64_get_battery(
            ProtocolKind::Feature64 {
                new_protocol: true,
                wired_device_id: 2,
            },
            ConnectionKind::Receiver,
        );
        assert_eq!(&new_report[2..6], &[2, 2, 0, 131]);

        let old_report = build_feature64_get_battery(
            ProtocolKind::Feature64 {
                new_protocol: false,
                wired_device_id: 2,
            },
            ConnectionKind::Receiver,
        );
        assert_eq!(&old_report[1..4], &[2, 143, 1]);
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

    #[test]
    fn builds_ipi_pix_v1_rate_reports() {
        let get_report = build_ipi_pix_v1_get_rate();
        assert_eq!(&get_report[0..6], &[233, 80, 0, 10, 79, 64]);

        let set_wired = build_ipi_pix_v1_set_rate(ConnectionKind::Wired, PollingRate::Hz8000);
        assert_eq!(&set_wired[0..7], &[252, 80, 1, 49, 53, 1, 68]);

        let set_wireless = build_ipi_pix_v1_set_rate(ConnectionKind::Wireless, PollingRate::Hz8000);
        assert_eq!(&set_wireless[0..7], &[188, 80, 1, 49, 53, 1, 4]);
    }

    #[test]
    fn builds_ipi_pix_v1_basic_info_report() {
        let report = build_ipi_pix_v1_get_basic_info();
        assert_eq!(&report[0..6], &[34, 80, 0, 2, 79, 129]);
    }

    #[test]
    fn parses_ipi_pix_v1_sensor_rate_byte() {
        assert_eq!(
            ipi_pix_v1_rate_from_sensor_byte(ConnectionKind::Wired, 0x40),
            Some(PollingRate::Hz8000)
        );
        assert_eq!(
            ipi_pix_v1_rate_from_sensor_byte(ConnectionKind::Wireless, 4),
            Some(PollingRate::Hz8000)
        );
    }
}
