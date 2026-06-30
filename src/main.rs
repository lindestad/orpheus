use std::{path::PathBuf, str::FromStr, thread, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use orpheus::{
    config::{PollMonitorConfig, default_config_path},
    devices::{PollingRate, format_supported_rates},
    hid_device::HidPollMonitor,
    tui::run_tui,
    watcher::run_watch,
};

#[derive(Debug, Parser)]
#[command(name = "orpheus")]
#[command(about = "Monitor and switch mouse polling rates")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the interactive terminal UI.
    Tui,
    /// List supported devices and their current configured rate.
    List,
    /// Set the first supported device to a polling rate, e.g. 1000 or 8k.
    Set { rate: String },
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
        Command::Tui => run_tui(),
        Command::List => list_devices(),
        Command::Set { rate } => set_rate(&rate),
        Command::InitConfig { path } => init_config(path),
        Command::Watch {
            config,
            dry_run,
            once,
        } => watch(config.unwrap_or_else(default_config_path), dry_run, once),
    }
}

fn list_devices() -> Result<()> {
    let monitor = HidPollMonitor::new()?;
    let devices = monitor.scan()?;
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
