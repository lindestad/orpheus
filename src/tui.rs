use std::{
    io,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};

use crate::{
    devices::{BatteryStatus, ChargeState, ConnectionKind, PollingRate},
    hid_device::{DeviceSnapshot, DeviceSnapshotCache, HidPollMonitor},
};

const FOCUSED_REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);
const UNFOCUSED_REFRESH_INTERVAL: Duration = Duration::from_millis(5_000);
const PENDING_RETRY_INTERVAL: Duration = Duration::from_millis(1_000);
const CHARGING_ICON: &str = "⚡";

pub fn run_tui() -> Result<()> {
    enable_raw_mode().context("failed to enable raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableFocusChange)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let result = run_app(&mut terminal);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableFocusChange,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let monitor = HidPollMonitor::new()?;
    let mut app = TuiApp::new();
    app.refresh(&monitor);

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        if app.should_refresh() {
            app.refresh(&monitor);
        }

        if event::poll(Duration::from_millis(150))? {
            match event::read()? {
                Event::FocusGained => app.set_focused(true),
                Event::FocusLost => app.set_focused(false),
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('r') => app.refresh(&monitor),
                        KeyCode::Up | KeyCode::Char('k') => app.move_device(-1),
                        KeyCode::Down | KeyCode::Char('j') => app.move_device(1),
                        KeyCode::Left | KeyCode::Char('h') => app.move_rate(-1),
                        KeyCode::Right | KeyCode::Char('l') => app.move_rate(1),
                        KeyCode::Enter => app.apply_rate(&monitor),
                        KeyCode::Char(' ') => app.sync_target_to_current(),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
struct TuiApp {
    devices: Vec<DeviceSnapshot>,
    snapshot_cache: DeviceSnapshotCache,
    pending_rate: Option<PendingTuiRateChange>,
    selected_device: usize,
    target_rate: Option<PollingRate>,
    target_dirty: bool,
    focused: bool,
    status: String,
    last_refresh: Instant,
}

impl TuiApp {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            snapshot_cache: DeviceSnapshotCache::default(),
            pending_rate: None,
            selected_device: 0,
            target_rate: None,
            target_dirty: false,
            focused: true,
            status: "scanning".to_string(),
            last_refresh: Instant::now() - FOCUSED_REFRESH_INTERVAL,
        }
    }

    fn should_refresh(&self) -> bool {
        self.last_refresh.elapsed() >= self.refresh_interval()
    }

    fn refresh_interval(&self) -> Duration {
        if self.pending_rate.is_some() {
            PENDING_RETRY_INTERVAL
        } else if self.focused {
            FOCUSED_REFRESH_INTERVAL
        } else {
            UNFOCUSED_REFRESH_INTERVAL
        }
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn refresh(&mut self, monitor: &HidPollMonitor) {
        self.last_refresh = Instant::now();
        match monitor.scan() {
            Ok(mut devices) => {
                self.snapshot_cache.apply(&mut devices);
                self.devices = devices;
                if self.selected_device >= self.devices.len() {
                    self.selected_device = 0;
                    self.target_dirty = false;
                }
                self.normalize_target();
                self.status = if self.devices.is_empty() {
                    "no supported device found".to_string()
                } else {
                    format!("{} device(s), refreshed now", self.devices.len())
                };
                self.try_pending_rate(monitor);
            }
            Err(err) => {
                self.devices.clear();
                self.status = format!("scan failed: {err}");
            }
        }
    }

    fn selected_device(&self) -> Option<&DeviceSnapshot> {
        self.devices.get(self.selected_device)
    }

    fn move_device(&mut self, delta: isize) {
        if self.devices.is_empty() {
            return;
        }
        self.selected_device = wrap_index(self.selected_device, self.devices.len(), delta);
        self.target_dirty = false;
        self.normalize_target();
    }

    fn move_rate(&mut self, delta: isize) {
        let Some(device) = self.selected_device() else {
            return;
        };
        if device.supported_rates.is_empty() {
            return;
        }
        let current = self
            .target_rate
            .and_then(|target| {
                device
                    .supported_rates
                    .iter()
                    .position(|rate| *rate == target)
            })
            .unwrap_or(0);
        let next = wrap_index(current, device.supported_rates.len(), delta);
        self.target_rate = device.supported_rates.get(next).copied();
        self.target_dirty = true;
    }

    fn sync_target_to_current(&mut self) {
        if let Some(current) = self
            .selected_device()
            .and_then(|device| device.current_rate)
        {
            self.target_rate = Some(current);
            self.target_dirty = false;
        }
    }

    fn apply_rate(&mut self, monitor: &HidPollMonitor) {
        let Some(device) = self.selected_device().cloned() else {
            self.status = "no device selected".to_string();
            return;
        };
        let Some(rate) = self.target_rate else {
            self.status = "no target rate selected".to_string();
            return;
        };

        if device.current_rate == Some(rate) {
            self.target_dirty = false;
            self.status = if device.cached_rate {
                format!("current rate cached as {rate}; no write needed")
            } else {
                format!("already at {rate}")
            };
            self.clear_pending_for(&device);
            return;
        }

        if device.cached_rate || device.current_rate.is_none() {
            self.queue_pending_rate(&device, rate);
            let current = device
                .current_rate
                .map(|rate| rate.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            self.status = format!("set queued: latest rate read is {current}");
            return;
        }

        match monitor
            .open_by_vid_pid(device.vid, device.pid)
            .and_then(|live| {
                live.set_rate(rate)?;
                std::thread::sleep(Duration::from_millis(80));
                live.read_rate().map(|after| (live.connection(), after))
            }) {
            Ok((connection, after)) => {
                if after == rate {
                    self.clear_pending_for(&device);
                    self.target_dirty = false;
                } else {
                    self.queue_pending_rate(&device, rate);
                }
                let status = if after == rate {
                    format!("set {connection} to {after}")
                } else {
                    format!("set queued: {connection} is still {after}")
                };
                self.refresh(monitor);
                self.status = status;
            }
            Err(err) => {
                self.queue_pending_rate(&device, rate);
                self.status = format!("set queued: {err}");
            }
        }
    }

    fn normalize_target(&mut self) {
        let Some(device) = self.selected_device() else {
            self.target_rate = None;
            self.target_dirty = false;
            return;
        };

        let supported = &device.supported_rates;
        if supported.is_empty() {
            self.target_rate = None;
            self.target_dirty = false;
            return;
        }

        if self.target_dirty
            && self
                .target_rate
                .is_some_and(|rate| supported.contains(&rate))
        {
            return;
        }

        self.target_rate = device
            .current_rate
            .filter(|rate| supported.contains(rate))
            .or_else(|| {
                supported
                    .iter()
                    .copied()
                    .find(|rate| *rate == PollingRate::Hz1000)
            })
            .or_else(|| supported.first().copied());
        self.target_dirty = false;
    }

    fn try_pending_rate(&mut self, monitor: &HidPollMonitor) {
        let Some(pending) = self.pending_rate.clone() else {
            return;
        };
        let Some(device) = self
            .devices
            .iter()
            .find(|device| pending.matches(device))
            .cloned()
        else {
            self.status = format!("queued {}: device not visible", pending.target);
            return;
        };

        if device.current_rate == Some(pending.target) {
            self.pending_rate = None;
            self.status = format!("queued target {} is now satisfied", pending.target);
            return;
        }

        if device.cached_rate || device.current_rate.is_none() {
            let current = device
                .current_rate
                .map(|rate| rate.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            self.status = format!("queued {}: latest rate read is {current}", pending.target);
            return;
        }

        match monitor
            .open_by_vid_pid(device.vid, device.pid)
            .and_then(|live| {
                live.set_rate(pending.target)?;
                std::thread::sleep(Duration::from_millis(80));
                live.read_rate().map(|after| (live.connection(), after))
            }) {
            Ok((connection, after)) => {
                if after == pending.target {
                    self.pending_rate = None;
                    self.target_dirty = false;
                    self.status = format!("set queued {connection} target to {after}");
                } else {
                    self.status =
                        format!("queued {}: {connection} is still {after}", pending.target);
                }
            }
            Err(err) => {
                self.status = format!("queued {}: {err}", pending.target);
            }
        }
    }

    fn queue_pending_rate(&mut self, device: &DeviceSnapshot, target: PollingRate) {
        self.pending_rate = Some(PendingTuiRateChange {
            vid: device.vid,
            pid: device.pid,
            connection: device.connection,
            target,
        });
    }

    fn clear_pending_for(&mut self, device: &DeviceSnapshot) {
        if self
            .pending_rate
            .as_ref()
            .is_some_and(|pending| pending.matches(device))
        {
            self.pending_rate = None;
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingTuiRateChange {
    vid: u16,
    pid: u16,
    connection: ConnectionKind,
    target: PollingRate,
}

impl PendingTuiRateChange {
    fn matches(&self, device: &DeviceSnapshot) -> bool {
        self.vid == device.vid && self.pid == device.pid && self.connection == device.connection
    }
}

fn wrap_index(current: usize, len: usize, delta: isize) -> usize {
    let len = len as isize;
    (current as isize + delta).rem_euclid(len) as usize
}

fn draw(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_devices(frame, chunks[1], app);
    draw_rate_panel(frame, chunks[2], app);
    draw_footer(frame, chunks[3], app);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = Line::from(vec![
        Span::styled(
            "poll monitor",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(&app.status, Style::default().fg(Color::Gray)),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_devices(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if app.devices.is_empty() {
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new("No supported polling-rate device is visible.")
                .wrap(Wrap { trim: true })
                .block(Block::default().title("Devices").borders(Borders::ALL)),
            area,
        );
        return;
    }

    let rows = app.devices.iter().enumerate().map(|(idx, device)| {
        let current = current_rate_summary(device);
        let power = power_summary(device);
        let status = if device.cached_rate || device.cached_battery {
            "cached"
        } else {
            device
                .read_error
                .as_ref()
                .map(|_| "read error")
                .or_else(|| device.battery_error.as_ref().map(|_| "battery"))
                .unwrap_or("ok")
        };
        let style = if idx == app.selected_device {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if device.read_error.is_some()
            || device.battery_error.is_some()
            || device.cached_rate
            || device.cached_battery
        {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };

        Row::new([
            Cell::from(format!("{} {}", idx + 1, device.vendor_name)),
            Cell::from(device.model_name),
            Cell::from(format!("{:04x}:{:04x}", device.vid, device.pid)),
            Cell::from(device.connection.to_string()),
            Cell::from(device.protocol.to_string()),
            Cell::from(current),
            Cell::from(power),
            Cell::from(device.supported_rates_text()),
            Cell::from(status),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Length(14),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Min(24),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new([
            "Device",
            "Model",
            "VID:PID",
            "Mode",
            "Protocol",
            "Rate",
            "Charge",
            "Supported",
            "Status",
        ])
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::default().title("Devices").borders(Borders::ALL));

    frame.render_widget(table, area);
}

fn draw_rate_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let Some(device) = app.selected_device() else {
        frame.render_widget(
            Paragraph::new("Target: -").block(Block::default().title("Rate").borders(Borders::ALL)),
            area,
        );
        return;
    };

    let current = current_rate_detail(device);
    let target = app
        .target_rate
        .map(|rate| rate.to_string())
        .unwrap_or_else(|| "none".to_string());
    let power = power_detail(device);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("current ", Style::default().fg(Color::Gray)),
        Span::styled(current, Style::default().fg(Color::Green)),
        Span::raw("   "),
        Span::styled("target ", Style::default().fg(Color::Gray)),
        Span::styled(
            target,
            Style::default()
                .fg(if app.target_dirty {
                    Color::Yellow
                } else {
                    Color::Green
                })
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("charge ", Style::default().fg(Color::Gray)),
        Span::styled(power, Style::default().fg(Color::White)),
    ]));

    lines.push(Line::raw(""));
    lines.push(rate_line(&device.supported_rates, app.target_rate));

    if let Some(error) = &device.read_error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("read error ", Style::default().fg(Color::Yellow)),
            Span::raw(error),
        ]));
    }
    if let Some(error) = &device.battery_error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("battery error ", Style::default().fg(Color::Yellow)),
            Span::raw(error),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Rate").borders(Borders::ALL)),
        area,
    );
}

fn power_summary(device: &DeviceSnapshot) -> String {
    let Some(battery) = device.battery else {
        return if device.battery_error.is_some() {
            "error".to_string()
        } else {
            "-".to_string()
        };
    };

    let mut text = battery_level_text(battery);
    match battery.charge_state {
        ChargeState::Charging => {
            text.push(' ');
            text.push_str(CHARGING_ICON);
        }
        ChargeState::Full => text.push_str(" full"),
        ChargeState::Discharging | ChargeState::Unknown => {}
        ChargeState::Raw(raw) => {
            if battery.level_percent.is_none() {
                text = format!("raw {raw}");
            }
        }
    }
    text
}

fn power_detail(device: &DeviceSnapshot) -> String {
    let Some(battery) = device.battery else {
        return device.battery_text();
    };

    let mut text = match battery.charge_state {
        ChargeState::Charging => {
            format!("{} {CHARGING_ICON} charging", battery_level_text(battery))
        }
        ChargeState::Full => format!("{} full", battery_level_text(battery)),
        ChargeState::Discharging => format!("{} discharging", battery_level_text(battery)),
        ChargeState::Unknown => {
            if battery.level_percent.is_some() {
                format!("{} (state unknown)", battery_level_text(battery))
            } else {
                "state unknown".to_string()
            }
        }
        ChargeState::Raw(raw) => format!("{} raw state {raw}", battery_level_text(battery)),
    };
    if device.cached_battery {
        text.push_str(" (cached)");
    }
    text
}

fn battery_level_text(battery: BatteryStatus) -> String {
    battery
        .level_percent
        .map(|level| format!("{level}%"))
        .unwrap_or_else(|| "level unknown".to_string())
}

fn current_rate_summary(device: &DeviceSnapshot) -> String {
    let Some(rate) = device.current_rate else {
        return "-".to_string();
    };
    if device.cached_rate {
        format!("{}*", rate.hz())
    } else {
        rate.hz().to_string()
    }
}

fn current_rate_detail(device: &DeviceSnapshot) -> String {
    let Some(rate) = device.current_rate else {
        return "unknown".to_string();
    };
    if device.cached_rate {
        format!("{rate} (cached)")
    } else {
        rate.to_string()
    }
}

fn rate_line(rates: &[PollingRate], selected: Option<PollingRate>) -> Line<'static> {
    let mut spans = Vec::new();
    for rate in rates {
        let is_selected = Some(*rate) == selected;
        let style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(format!(" {} ", rate.hz()), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let footer = Line::from(vec![
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::raw(" quit  "),
        Span::styled("r", Style::default().fg(Color::Cyan)),
        Span::raw(" refresh  "),
        Span::styled("up/down", Style::default().fg(Color::Cyan)),
        Span::raw(" or "),
        Span::styled("j/k", Style::default().fg(Color::Cyan)),
        Span::raw(" device  "),
        Span::styled("left/right", Style::default().fg(Color::Cyan)),
        Span::raw(" or "),
        Span::styled("h/l", Style::default().fg(Color::Cyan)),
        Span::raw(" rate  "),
        Span::styled("enter", Style::default().fg(Color::Cyan)),
        Span::raw(" set  "),
        Span::styled("space", Style::default().fg(Color::Cyan)),
        Span::raw(" sync"),
    ]);

    let focus = if app.focused { "focused" } else { "unfocused" };
    let refresh = format!(
        "auto-refresh {} ms ({focus})",
        app.refresh_interval().as_millis().saturating_sub(
            app.last_refresh
                .elapsed()
                .as_millis()
                .min(app.refresh_interval().as_millis())
        )
    );

    frame.render_widget(
        Paragraph::new(vec![
            footer,
            Line::from(Span::styled(refresh, Style::default().fg(Color::Gray))),
        ])
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{BatteryStatus, ConnectionKind, ProtocolKind};

    #[test]
    fn compact_power_summary_marks_charging_only() {
        let mut device = test_device(Some(BatteryStatus::with_raw_state(
            75,
            ChargeState::Discharging,
            0,
        )));
        assert_eq!(power_summary(&device), "75%");

        device.battery = Some(BatteryStatus::with_raw_state(80, ChargeState::Charging, 1));
        assert_eq!(power_summary(&device), "80% ⚡");
    }

    #[test]
    fn power_detail_keeps_state_text() {
        let device = test_device(Some(BatteryStatus::level_only(100)));
        assert_eq!(power_detail(&device), "100% (state unknown)");
    }

    #[test]
    fn refresh_interval_tracks_focus_and_pending_rate() {
        let mut app = TuiApp::new();
        assert_eq!(app.refresh_interval(), FOCUSED_REFRESH_INTERVAL);

        app.set_focused(false);
        assert_eq!(app.refresh_interval(), UNFOCUSED_REFRESH_INTERVAL);

        app.pending_rate = Some(PendingTuiRateChange {
            vid: 0x1234,
            pid: 0x5678,
            connection: ConnectionKind::Wireless,
            target: PollingRate::Hz4000,
        });

        assert_eq!(app.refresh_interval(), PENDING_RETRY_INTERVAL);
    }

    fn test_device(battery: Option<BatteryStatus>) -> DeviceSnapshot {
        DeviceSnapshot {
            path: "test".to_string(),
            vid: 0x1234,
            pid: 0x5678,
            product_name: None,
            vendor_name: "Test",
            model_name: "Mouse",
            connection: ConnectionKind::Wireless,
            protocol: ProtocolKind::IpiPixV1 { report_id: 3 },
            supported_rates: vec![PollingRate::Hz1000],
            current_rate: Some(PollingRate::Hz1000),
            battery,
            cached_rate: false,
            cached_battery: false,
            battery_error: None,
            read_error: None,
        }
    }
}
