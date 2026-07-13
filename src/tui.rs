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
    hid_device::{DeviceSnapshot, DeviceSnapshotCache, HidAccessCandidate, HidPollMonitor},
};

const FOCUSED_REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);
const UNFOCUSED_REFRESH_INTERVAL: Duration = Duration::from_millis(5_000);
const UNAVAILABLE_REFRESH_INTERVAL: Duration = Duration::from_millis(5_000);
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
                    if app.access_prompt.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Enter => {
                                enable_hid_access(terminal, &mut app, &monitor)?;
                            }
                            KeyCode::Char('n') | KeyCode::Esc => app.dismiss_access_prompt(),
                            KeyCode::Char('q') => break,
                            _ => {}
                        }
                        continue;
                    }
                    if app.dpi_prompt.is_some() {
                        match key.code {
                            KeyCode::Enter => app.apply_dpi(&monitor),
                            KeyCode::Esc => app.dismiss_dpi_prompt(),
                            KeyCode::Backspace => app.remove_dpi_digit(),
                            KeyCode::Delete => app.clear_dpi_input(),
                            KeyCode::Char(digit) if digit.is_ascii_digit() => {
                                app.push_dpi_digit(digit)
                            }
                            _ => {}
                        }
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
                        KeyCode::Char('d') => app.open_dpi_prompt(&monitor),
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
    access_prompt: Option<AccessPrompt>,
    access_prompt_dismissed: bool,
    dpi_prompt: Option<DpiPrompt>,
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
            access_prompt: None,
            access_prompt_dismissed: false,
            dpi_prompt: None,
        }
    }

    fn should_refresh(&self) -> bool {
        self.last_refresh.elapsed() >= self.refresh_interval()
    }

    fn refresh_interval(&self) -> Duration {
        if self.pending_rate.is_some() {
            PENDING_RETRY_INTERVAL
        } else if self.devices.iter().any(device_is_unavailable) {
            UNAVAILABLE_REFRESH_INTERVAL
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
                self.refresh_access_prompt(monitor);
                self.try_pending_rate(monitor);
            }
            Err(err) => {
                self.devices.clear();
                self.status = format!("scan failed: {err}");
            }
        }
        self.last_refresh = Instant::now();
    }

    fn refresh_access_prompt(&mut self, monitor: &HidPollMonitor) {
        if self.access_prompt.is_some() || self.access_prompt_dismissed {
            return;
        }
        let candidates = monitor.hid_access_candidates();
        if candidates.is_empty() {
            return;
        }
        self.status = format!("{} supported hidraw path(s) need access", candidates.len());
        self.access_prompt = Some(AccessPrompt { candidates });
    }

    fn dismiss_access_prompt(&mut self) {
        self.access_prompt = None;
        self.access_prompt_dismissed = true;
        self.status = "hidraw access setup skipped".to_string();
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

    fn open_dpi_prompt(&mut self, monitor: &HidPollMonitor) {
        let Some(device) = self.selected_device().cloned() else {
            self.status = "no device selected".to_string();
            return;
        };
        if !device.protocol.supports_dpi() {
            self.status = format!("DPI is not supported for {}", device.protocol);
            return;
        }

        match monitor
            .open_by_path(&device.path)
            .and_then(|live| live.read_dpi())
        {
            Ok(current) => {
                self.dpi_prompt = Some(DpiPrompt {
                    current,
                    input: current.to_string(),
                    error: None,
                    replace_on_input: true,
                });
                self.status = format!("{} DPI", current);
                self.last_refresh = Instant::now();
            }
            Err(err) => self.status = format!("DPI read failed: {err}"),
        }
    }

    fn dismiss_dpi_prompt(&mut self) {
        self.dpi_prompt = None;
        self.status = "DPI change cancelled".to_string();
    }

    fn push_dpi_digit(&mut self, digit: char) {
        let Some(prompt) = self.dpi_prompt.as_mut() else {
            return;
        };
        if prompt.replace_on_input {
            prompt.input.clear();
            prompt.replace_on_input = false;
        }
        if prompt.input.len() < 5 {
            prompt.input.push(digit);
        }
        prompt.error = None;
    }

    fn remove_dpi_digit(&mut self) {
        let Some(prompt) = self.dpi_prompt.as_mut() else {
            return;
        };
        if prompt.replace_on_input {
            prompt.input.clear();
            prompt.replace_on_input = false;
        } else {
            prompt.input.pop();
        }
        prompt.error = None;
    }

    fn clear_dpi_input(&mut self) {
        let Some(prompt) = self.dpi_prompt.as_mut() else {
            return;
        };
        prompt.input.clear();
        prompt.replace_on_input = false;
        prompt.error = None;
    }

    fn apply_dpi(&mut self, monitor: &HidPollMonitor) {
        let Some(prompt) = self.dpi_prompt.as_ref() else {
            return;
        };
        let requested = match prompt.input.parse::<u16>() {
            Ok(dpi) if dpi > 0 => dpi,
            _ => {
                if let Some(prompt) = self.dpi_prompt.as_mut() {
                    prompt.error = Some("enter a positive whole-number DPI".to_string());
                    prompt.replace_on_input = true;
                }
                return;
            }
        };
        let Some(device) = self.selected_device().cloned() else {
            self.dpi_prompt = None;
            self.status = "selected device disappeared".to_string();
            return;
        };

        let result = monitor.open_by_path(&device.path).and_then(|live| {
            live.set_dpi(requested)?;
            std::thread::sleep(Duration::from_millis(50));
            live.read_dpi()
        });

        match result {
            Ok(after) if after == requested => {
                self.dpi_prompt = None;
                self.status = format!("set {} to {after} DPI (verified)", device.model_name);
                self.last_refresh = Instant::now();
            }
            Ok(after) => {
                if let Some(prompt) = self.dpi_prompt.as_mut() {
                    prompt.error = Some(format!(
                        "verification failed: requested {requested}, device reports {after}"
                    ));
                    prompt.current = after;
                    prompt.replace_on_input = true;
                }
            }
            Err(err) => {
                if let Some(prompt) = self.dpi_prompt.as_mut() {
                    prompt.error = Some(err.to_string());
                    prompt.replace_on_input = true;
                }
            }
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
struct AccessPrompt {
    candidates: Vec<HidAccessCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DpiPrompt {
    current: u16,
    input: String,
    error: Option<String>,
    replace_on_input: bool,
}

fn enable_hid_access(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
    monitor: &HidPollMonitor,
) -> Result<()> {
    let Some(prompt) = app.access_prompt.clone() else {
        return Ok(());
    };

    match run_hid_access_setup(terminal, &prompt.candidates) {
        Ok(message) => {
            app.access_prompt = None;
            app.access_prompt_dismissed = false;
            app.refresh(monitor);
            app.status = message;
        }
        Err(err) => {
            app.status = format!("hidraw access setup failed: {err:#}");
        }
    }
    Ok(())
}

fn run_hid_access_setup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    candidates: &[HidAccessCandidate],
) -> Result<String> {
    suspend_terminal(terminal, || {
        #[cfg(target_os = "linux")]
        {
            install_linux_hid_access(candidates)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = candidates;
            anyhow::bail!("hidraw access setup is only available on Linux")
        }
    })
}

fn suspend_terminal<T>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableFocusChange,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();

    let result = action();

    println!();
    println!("Press Enter to return to Orpheus...");
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();

    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableFocusChange
    )
    .ok();
    enable_raw_mode().ok();
    terminal.clear().ok();

    result
}

#[cfg(target_os = "linux")]
fn install_linux_hid_access(candidates: &[HidAccessCandidate]) -> Result<String> {
    use std::{
        collections::BTreeSet,
        env,
        io::Write,
        process::{Command, Stdio},
    };

    const RULE_PATH: &str = "/etc/udev/rules.d/70-orpheus-hidraw.rules";

    if candidates.is_empty() {
        anyhow::bail!("no hidraw access candidates found")
    }

    let rules = linux_hidraw_udev_rules(candidates);
    println!("Installing {RULE_PATH}");
    let mut tee = Command::new("sudo")
        .arg("tee")
        .arg(RULE_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("failed to start sudo tee")?;
    tee.stdin
        .as_mut()
        .context("failed to open sudo tee stdin")?
        .write_all(rules.as_bytes())
        .context("failed to write udev rules")?;
    let status = tee.wait().context("failed to wait for sudo tee")?;
    if !status.success() {
        anyhow::bail!("sudo tee failed with {status}");
    }

    run_sudo(["udevadm", "control", "--reload-rules"])?;
    run_sudo(["udevadm", "trigger", "--subsystem-match=hidraw"])?;

    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .context("failed to determine current user")?;
    let paths = candidates
        .iter()
        .map(|candidate| candidate.path.as_str())
        .collect::<BTreeSet<_>>();
    if !paths.is_empty() {
        let mut command = Command::new("sudo");
        command.arg("setfacl").arg("-m").arg(format!("u:{user}:rw"));
        for path in &paths {
            command.arg(path);
        }
        let status = command.status().context("failed to start sudo setfacl")?;
        if !status.success() {
            anyhow::bail!("sudo setfacl failed with {status}");
        }
    }

    let count = paths.len();
    Ok(format!("enabled hidraw access for {count} path(s)"))
}

#[cfg(target_os = "linux")]
fn run_sudo<const N: usize>(args: [&str; N]) -> Result<()> {
    let status = std::process::Command::new("sudo")
        .args(args)
        .status()
        .with_context(|| format!("failed to start sudo {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("sudo {} failed with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_hidraw_udev_rules(candidates: &[HidAccessCandidate]) -> String {
    let mut pairs = std::collections::BTreeSet::new();
    for candidate in candidates {
        pairs.insert((candidate.vid, candidate.pid));
    }

    let mut rules = String::from(
        "# Generated by Orpheus.\n# Grants logged-in users access to supported mouse HID control interfaces.\n",
    );
    for (vid, pid) in pairs {
        rules.push_str(&format!(
            "SUBSYSTEM==\"hidraw\", ATTRS{{idVendor}}==\"{vid:04x}\", ATTRS{{idProduct}}==\"{pid:04x}\", TAG+=\"uaccess\"\n"
        ));
    }
    rules
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
    if let Some(prompt) = &app.access_prompt {
        draw_access_prompt(frame, area, prompt);
    } else if let Some(prompt) = &app.dpi_prompt {
        draw_dpi_prompt(frame, area, prompt);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = Line::from(vec![
        Span::styled(
            "Orpheus",
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

fn device_is_unavailable(device: &DeviceSnapshot) -> bool {
    !device.connection.is_wired()
        && device
            .read_error
            .as_deref()
            .is_some_and(|error| error.contains("timed out waiting for"))
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
        } else if device_is_unavailable(device) {
            "asleep"
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

    if device_is_unavailable(device) {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("unavailable ", Style::default().fg(Color::Yellow)),
            Span::raw("mouse may be asleep; move it or press r to retry"),
        ]));
    } else if let Some(error) = &device.read_error {
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

fn draw_access_prompt(frame: &mut Frame<'_>, area: Rect, prompt: &AccessPrompt) {
    let modal = centered_rect(78, 13, area);
    let mut lines = vec![
        Line::from(Span::styled(
            "Linux hidraw access is needed",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::raw(
            "Supported devices are visible, but one or more hidraw nodes could not be opened.",
        ),
        Line::raw("Install a narrow udev rule and apply ACLs to current nodes? This runs sudo."),
        Line::raw(""),
    ];

    for candidate in prompt.candidates.iter().take(4) {
        lines.push(Line::raw(format!(
            "- {} {} {:04x}:{:04x} {}",
            candidate.vendor_name,
            candidate.model_name,
            candidate.vid,
            candidate.pid,
            candidate.path
        )));
    }
    if prompt.candidates.len() > 4 {
        lines.push(Line::raw(format!(
            "- ...and {} more",
            prompt.candidates.len() - 4
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("enter/y", Style::default().fg(Color::Cyan)),
        Span::raw(" enable  "),
        Span::styled("n/esc", Style::default().fg(Color::Cyan)),
        Span::raw(" skip  "),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::raw(" quit"),
    ]));

    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Access").borders(Borders::ALL)),
        modal,
    );
}

fn draw_dpi_prompt(frame: &mut Frame<'_>, area: Rect, prompt: &DpiPrompt) {
    let modal = centered_rect(62, 10, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("current ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{} DPI", prompt.current),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("new DPI ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!(" {} ", prompt.input),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::raw("Type a value supported by the mouse (PIAO11 uses 50-DPI steps)."),
    ];
    if let Some(error) = &prompt.error {
        lines.push(Line::from(vec![
            Span::styled("error ", Style::default().fg(Color::Red)),
            Span::raw(error),
        ]));
    } else {
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(vec![
        Span::styled("enter", Style::default().fg(Color::Cyan)),
        Span::raw(" set and verify  "),
        Span::styled("esc", Style::default().fg(Color::Cyan)),
        Span::raw(" cancel  "),
        Span::styled("delete", Style::default().fg(Color::Cyan)),
        Span::raw(" clear"),
    ]));

    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("DPI").borders(Borders::ALL)),
        modal,
    );
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area.width.saturating_mul(percent_x).saturating_div(100);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height: height.min(area.height),
    }
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
        Span::raw(" device  "),
        Span::styled("left/right", Style::default().fg(Color::Cyan)),
        Span::raw(" rate  "),
        Span::styled("enter", Style::default().fg(Color::Cyan)),
        Span::raw(" set rate  "),
        Span::styled("d", Style::default().fg(Color::Cyan)),
        Span::raw(" dpi"),
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

        let mut unavailable = test_device(None);
        unavailable.read_error =
            Some("timed out waiting for report8 command 8 after 5 attempts".to_string());
        app.devices.push(unavailable);
        assert_eq!(app.refresh_interval(), UNAVAILABLE_REFRESH_INTERVAL);

        app.devices.clear();
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

    #[test]
    fn dpi_prompt_replaces_current_value_on_first_digit() {
        let mut app = TuiApp::new();
        app.dpi_prompt = Some(DpiPrompt {
            current: 800,
            input: "800".to_string(),
            error: None,
            replace_on_input: true,
        });

        app.push_dpi_digit('3');
        app.push_dpi_digit('2');
        app.push_dpi_digit('0');
        app.push_dpi_digit('0');

        assert_eq!(app.dpi_prompt.as_ref().unwrap().input, "3200");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_hidraw_rules_are_narrow_and_deduplicated() {
        let candidates = vec![
            HidAccessCandidate {
                path: "/dev/hidraw1".to_string(),
                vid: 0x3554,
                pid: 0xF514,
                vendor_name: "Compx",
                model_name: "PIAO11",
            },
            HidAccessCandidate {
                path: "/dev/hidraw2".to_string(),
                vid: 0x3554,
                pid: 0xF514,
                vendor_name: "Compx",
                model_name: "PIAO11",
            },
        ];

        let rules = linux_hidraw_udev_rules(&candidates);
        assert_eq!(
            rules
                .lines()
                .filter(|line| line.contains("3554") && line.contains("f514"))
                .count(),
            1
        );
        assert!(rules.contains("SUBSYSTEM==\"hidraw\""));
        assert!(rules.contains("TAG+=\"uaccess\""));
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
