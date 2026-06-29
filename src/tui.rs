use std::{
    io,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
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
    devices::PollingRate,
    hid_device::{DeviceSnapshot, HidPollMonitor},
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);

pub fn run_tui() -> Result<()> {
    enable_raw_mode().context("failed to enable raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let result = run_app(&mut terminal);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
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
            let Event::Key(key) = event::read()? else {
                continue;
            };
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
    }

    Ok(())
}

#[derive(Debug)]
struct TuiApp {
    devices: Vec<DeviceSnapshot>,
    selected_device: usize,
    target_rate: Option<PollingRate>,
    target_dirty: bool,
    status: String,
    last_refresh: Instant,
}

impl TuiApp {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            selected_device: 0,
            target_rate: None,
            target_dirty: false,
            status: "scanning".to_string(),
            last_refresh: Instant::now() - REFRESH_INTERVAL,
        }
    }

    fn should_refresh(&self) -> bool {
        self.last_refresh.elapsed() >= REFRESH_INTERVAL
    }

    fn refresh(&mut self, monitor: &HidPollMonitor) {
        self.last_refresh = Instant::now();
        match monitor.scan() {
            Ok(devices) => {
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

        match monitor
            .open_by_vid_pid(device.vid, device.pid)
            .and_then(|live| {
                live.set_rate(rate)?;
                std::thread::sleep(Duration::from_millis(80));
                live.read_rate().map(|after| (live.connection(), after))
            }) {
            Ok((connection, after)) => {
                self.target_dirty = false;
                let status = format!("set {connection} to {after}");
                self.refresh(monitor);
                self.status = status;
            }
            Err(err) => {
                self.status = format!("set failed: {err}");
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
        let current = device
            .current_rate
            .map(|rate| rate.hz().to_string())
            .unwrap_or_else(|| "-".to_string());
        let status = device
            .read_error
            .as_ref()
            .map(|_| "read error")
            .unwrap_or("ok");
        let style = if idx == app.selected_device {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if device.read_error.is_some() {
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
            Constraint::Length(10),
            Constraint::Length(15),
            Constraint::Length(8),
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
            "Current",
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

    let current = device
        .current_rate
        .map(|rate| rate.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let target = app
        .target_rate
        .map(|rate| rate.to_string())
        .unwrap_or_else(|| "none".to_string());

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

    lines.push(Line::raw(""));
    lines.push(rate_line(&device.supported_rates, app.target_rate));

    if let Some(error) = &device.read_error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("read error ", Style::default().fg(Color::Yellow)),
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

    let refresh = format!(
        "auto-refresh {} ms",
        REFRESH_INTERVAL.as_millis().saturating_sub(
            app.last_refresh
                .elapsed()
                .as_millis()
                .min(REFRESH_INTERVAL.as_millis())
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
