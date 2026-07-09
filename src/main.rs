use std::{io, path::PathBuf, str::FromStr, thread, time::Duration};

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use orpheus::{
    config::{PollMonitorConfig, default_config_path},
    devices::{BatteryStatus, PollingRate, format_supported_rates},
    gui::{GuiOptions, run_gui},
    hid_device::{DeviceDiagnostic, DeviceSnapshot, HidPollMonitor, ProbeOutcome},
    protocols::write_report8_crc,
    tui::run_tui,
    watcher::run_watch,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "orpheus")]
#[command(about = "Monitor and switch mouse polling rates")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the native GUI.
    Gui {
        /// Prefer Windows software/WARP rendering to avoid NVIDIA windowed VRR capture.
        #[arg(long)]
        software_renderer: bool,
        /// Repaint at 60 Hz while focused if VRR still captures the window.
        #[arg(long)]
        steady_repaint: bool,
    },
    /// Launch the interactive terminal UI.
    Tui,
    /// List supported devices and their current configured rate.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Probe supported HID interfaces and print read-only protocol diagnostics.
    Diagnose {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set the first supported device to a polling rate, e.g. 1000 or 8k.
    Set { rate: String },
    /// Set the active DPI level on the first supported mouse.
    Dpi {
        value: u16,
        /// Open this HID interface number directly instead of probing first.
        #[arg(long)]
        interface: Option<i32>,
    },
    /// Send a raw report8 packet to the first supported device.
    #[command(hide = true)]
    Report8 {
        /// Hex or decimal payload bytes. Missing bytes are padded with zero.
        bytes: Vec<String>,
        /// Do not compute the report8 checksum byte.
        #[arg(long)]
        no_crc: bool,
        /// Per-read timeout in milliseconds.
        #[arg(long, default_value_t = 50)]
        timeout_ms: i32,
        /// Number of input reads to attempt after the write.
        #[arg(long, default_value_t = 5)]
        reads: usize,
        /// Open this HID interface number directly instead of probing first.
        #[arg(long)]
        interface: Option<i32>,
    },
    /// Write an example app-rule config file.
    InitConfig {
        #[arg(default_value = "orpheus.toml")]
        path: PathBuf,
    },
    /// Watch running processes and apply configured app rules.
    Watch {
        #[arg(short, long, value_name = "PATH")]
        config: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        once: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui) {
        Command::Gui {
            software_renderer,
            steady_repaint,
        } => run_gui(GuiOptions {
            software_renderer,
            steady_repaint,
        }),
        Command::Tui => run_tui(),
        Command::List { json } => list_devices(json),
        Command::Diagnose { json } => diagnose_devices(json),
        Command::Set { rate } => set_rate(&rate),
        Command::Dpi { value, interface } => set_dpi(value, interface),
        Command::Report8 {
            bytes,
            no_crc,
            timeout_ms,
            reads,
            interface,
        } => report8(bytes, !no_crc, timeout_ms, reads, interface),
        Command::InitConfig { path } => init_config(path),
        Command::Watch {
            config,
            dry_run,
            once,
        } => watch(config.unwrap_or_else(default_config_path), dry_run, once),
    }
}

fn list_devices(json: bool) -> Result<()> {
    let monitor = HidPollMonitor::new()?;
    let devices = monitor.scan()?;
    if json {
        write_json(&ListJson {
            devices: devices.iter().map(ListDeviceJson::from_snapshot).collect(),
        })?;
        return Ok(());
    }

    if devices.is_empty() {
        println!("no supported polling-rate devices found");
        return Ok(());
    }

    for device in devices {
        let current = device
            .current_rate
            .map(|rate| rate.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "{:04x}:{:04x} {:<9} {:<18} {:<8} current: {:<8} battery: {:<24} supported: {}",
            device.vid,
            device.pid,
            device.vendor_name,
            device.model_name,
            device.connection,
            current,
            device.battery_text(),
            format_supported_rates(&device.supported_rates)
        );
        if let Some(error) = device.read_error {
            println!("  read error: {error}");
        }
        if let Some(error) = device.battery_error {
            println!("  battery error: {error}");
        }
    }
    Ok(())
}

fn diagnose_devices(json: bool) -> Result<()> {
    let monitor = HidPollMonitor::new()?;
    let diagnostics = monitor.diagnose();
    if json {
        write_json(&DiagnoseJson {
            interfaces: diagnostics
                .iter()
                .map(DiagnoseInterfaceJson::from_diagnostic)
                .collect(),
        })?;
        return Ok(());
    }

    if diagnostics.is_empty() {
        println!("no supported HID interfaces found");
        return Ok(());
    }

    for device in diagnostics {
        print_diagnostic(&device);
    }
    Ok(())
}

fn set_rate(raw_rate: &str) -> Result<()> {
    let rate = PollingRate::from_str(raw_rate)?;
    let monitor = HidPollMonitor::new()?;
    let device = monitor.open_first_supported()?;
    let before = device.read_rate().ok();
    device.set_rate(rate)?;
    thread::sleep(Duration::from_millis(80));
    let after = device.read_rate().ok();

    println!(
        "{} {}: {} -> {}",
        device.model().name,
        device.connection(),
        before
            .map(|rate| rate.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        after
            .map(|rate| rate.to_string())
            .unwrap_or_else(|| rate.to_string())
    );
    Ok(())
}

fn set_dpi(dpi: u16, interface: Option<i32>) -> Result<()> {
    let monitor = HidPollMonitor::new()?;
    let device = if interface.is_some() {
        monitor.open_first_supported_unprobed(interface)?
    } else {
        monitor.open_first_supported()?
    };
    let before = device.read_dpi().ok();
    device.set_dpi(dpi)?;
    thread::sleep(Duration::from_millis(80));
    let after = device.read_dpi().ok();

    println!(
        "{} {} DPI: {} -> {}",
        device.model().name,
        device.connection(),
        before
            .map(|dpi| dpi.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        after
            .map(|dpi| dpi.to_string())
            .unwrap_or_else(|| dpi.to_string())
    );
    Ok(())
}

fn report8(
    bytes: Vec<String>,
    write_crc: bool,
    timeout_ms: i32,
    reads: usize,
    interface: Option<i32>,
) -> Result<()> {
    if bytes.len() > 16 {
        bail!(
            "report8 payload accepts at most 16 byte(s), got {}",
            bytes.len()
        );
    }
    let mut payload = [0u8; 16];
    for (index, byte) in bytes.iter().enumerate() {
        payload[index] = parse_byte(byte)?;
    }
    if write_crc {
        write_report8_crc(&mut payload);
    }

    let monitor = HidPollMonitor::new()?;
    let device = if interface.is_some() {
        monitor.open_first_supported_unprobed(interface)?
    } else {
        monitor.open_first_supported()?
    };
    let responses = device.report8_exchange(payload, false, timeout_ms, reads)?;
    println!(
        "{} {} report8 tx: {}",
        device.model().name,
        device.connection(),
        format_bytes(&payload)
    );
    if responses.is_empty() {
        println!("no report8 response");
    } else {
        for response in responses {
            println!("report8 rx: {}", format_bytes(&response));
        }
    }
    Ok(())
}

fn init_config(path: PathBuf) -> Result<()> {
    PollMonitorConfig::write_example(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn watch(path: PathBuf, dry_run: bool, once: bool) -> Result<()> {
    let config = PollMonitorConfig::load(&path)?;
    println!(
        "watching {} rule(s) from {} every {} ms",
        config.rules.len(),
        path.display(),
        config.scan_interval_ms.max(250)
    );
    run_watch(config, dry_run, once)
}

fn parse_byte(raw: &str) -> Result<u8> {
    let trimmed = raw.trim();
    let value = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16)
    } else {
        trimmed.parse()
    }
    .map_err(|err| anyhow!("invalid byte {raw:?}: {err}"))?;
    Ok(value)
}

fn format_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn write_json<T: Serialize>(value: &T) -> Result<()> {
    serde_json::to_writer_pretty(io::stdout(), value)?;
    println!();
    Ok(())
}

fn print_diagnostic(device: &DeviceDiagnostic) {
    println!(
        "{:04x}:{:04x} iface {} usage {}:{} {} {} {}",
        device.vid,
        device.pid,
        device.interface_number,
        optional_hex(device.usage_page),
        optional_hex(device.usage),
        device.vendor_name,
        device.model_name,
        device.connection
    );
    println!("  path: {}", device.path);
    if let Some(product) = &device.product {
        println!("  product: {product}");
    }
    if let Some(manufacturer) = &device.manufacturer {
        println!("  manufacturer: {manufacturer}");
    }
    if let Some(serial) = &device.serial {
        println!("  serial: {serial}");
    }
    println!(
        "  release: 0x{:04x} bus: {} supported: {}",
        device.release_number,
        device.bus_type,
        format_supported_rates(&device.supported_rates)
    );

    for protocol in &device.protocols {
        println!("  protocol {}", protocol.protocol);
        print_probe("open", &protocol.open);
        print_probe("control", &protocol.control_probe);
        print_probe("rate", &protocol.rate_read);
        print_probe("battery", &protocol.battery_read);
    }
}

fn print_probe(label: &str, outcome: &ProbeOutcome) {
    match outcome {
        ProbeOutcome::Ok { detail, elapsed_ms } => {
            println!("    {label}: ok ({detail}, {elapsed_ms} ms)")
        }
        ProbeOutcome::Skipped { reason } => println!("    {label}: skipped ({reason})"),
        ProbeOutcome::Error { error, elapsed_ms } => {
            println!("    {label}: error ({error}, {elapsed_ms} ms)")
        }
    }
}

fn optional_hex(value: Option<u16>) -> String {
    value
        .map(|value| format!("0x{value:04x}"))
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Serialize)]
struct ListJson {
    devices: Vec<ListDeviceJson>,
}

#[derive(Serialize)]
struct ListDeviceJson {
    path: String,
    vid: u16,
    pid: u16,
    product_name: Option<String>,
    vendor_name: &'static str,
    model_name: &'static str,
    connection: String,
    protocol: String,
    supported_rates: Vec<u16>,
    current_rate: Option<u16>,
    cached_rate: bool,
    battery: Option<BatteryJson>,
    cached_battery: bool,
    read_error: Option<String>,
    battery_error: Option<String>,
}

impl ListDeviceJson {
    fn from_snapshot(device: &DeviceSnapshot) -> Self {
        Self {
            path: device.path.clone(),
            vid: device.vid,
            pid: device.pid,
            product_name: device.product_name.clone(),
            vendor_name: device.vendor_name,
            model_name: device.model_name,
            connection: device.connection.to_string(),
            protocol: device.protocol.to_string(),
            supported_rates: rates_hz(&device.supported_rates),
            current_rate: device.current_rate.map(PollingRate::hz),
            cached_rate: device.cached_rate,
            battery: device.battery.map(BatteryJson::from_status),
            cached_battery: device.cached_battery,
            read_error: device.read_error.clone(),
            battery_error: device.battery_error.clone(),
        }
    }
}

#[derive(Serialize)]
struct BatteryJson {
    level_percent: Option<u8>,
    charge_state: String,
    raw_state: Option<u8>,
    charging_like: Option<bool>,
    text: String,
}

impl BatteryJson {
    fn from_status(battery: BatteryStatus) -> Self {
        Self {
            level_percent: battery.level_percent,
            charge_state: battery.charge_state.to_string(),
            raw_state: battery.raw_state,
            charging_like: battery.is_charging_like(),
            text: battery.to_string(),
        }
    }
}

#[derive(Serialize)]
struct DiagnoseJson {
    interfaces: Vec<DiagnoseInterfaceJson>,
}

#[derive(Serialize)]
struct DiagnoseInterfaceJson {
    path: String,
    vid: u16,
    pid: u16,
    manufacturer: Option<String>,
    product: Option<String>,
    serial: Option<String>,
    release_number: u16,
    usage_page: Option<u16>,
    usage: Option<u16>,
    interface_number: i32,
    bus_type: String,
    vendor_name: &'static str,
    model_name: &'static str,
    connection: String,
    supported_rates: Vec<u16>,
    protocols: Vec<DiagnoseProtocolJson>,
}

impl DiagnoseInterfaceJson {
    fn from_diagnostic(device: &DeviceDiagnostic) -> Self {
        Self {
            path: device.path.clone(),
            vid: device.vid,
            pid: device.pid,
            manufacturer: device.manufacturer.clone(),
            product: device.product.clone(),
            serial: device.serial.clone(),
            release_number: device.release_number,
            usage_page: device.usage_page,
            usage: device.usage,
            interface_number: device.interface_number,
            bus_type: device.bus_type.clone(),
            vendor_name: device.vendor_name,
            model_name: device.model_name,
            connection: device.connection.to_string(),
            supported_rates: rates_hz(&device.supported_rates),
            protocols: device
                .protocols
                .iter()
                .map(|protocol| DiagnoseProtocolJson {
                    protocol: protocol.protocol.to_string(),
                    open: ProbeJson::from_outcome(&protocol.open),
                    control_probe: ProbeJson::from_outcome(&protocol.control_probe),
                    rate_read: ProbeJson::from_outcome(&protocol.rate_read),
                    battery_read: ProbeJson::from_outcome(&protocol.battery_read),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct DiagnoseProtocolJson {
    protocol: String,
    open: ProbeJson,
    control_probe: ProbeJson,
    rate_read: ProbeJson,
    battery_read: ProbeJson,
}

#[derive(Serialize)]
struct ProbeJson {
    status: &'static str,
    detail: Option<String>,
    reason: Option<String>,
    error: Option<String>,
    elapsed_ms: Option<u128>,
}

impl ProbeJson {
    fn from_outcome(outcome: &ProbeOutcome) -> Self {
        Self {
            status: outcome.status(),
            detail: outcome.detail().map(ToOwned::to_owned),
            reason: outcome.reason().map(ToOwned::to_owned),
            error: outcome.error_message().map(ToOwned::to_owned),
            elapsed_ms: outcome.elapsed_ms(),
        }
    }
}

fn rates_hz(rates: &[PollingRate]) -> Vec<u16> {
    rates.iter().map(|rate| rate.hz()).collect()
}
