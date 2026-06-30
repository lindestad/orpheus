use std::{
    collections::HashMap,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Result;
use eframe::egui::{
    self, Align, Button, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    Frame, Layout, Margin, RichText, ScrollArea, Stroke, TextStyle, Theme, Vec2, ViewportBuilder,
    Visuals,
};

use crate::{
    devices::{BatteryStatus, ChargeState, ConnectionKind, PollingRate},
    hid_device::{DeviceSnapshot, DeviceSnapshotCache, HidPollMonitor},
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);
const BG: Color32 = Color32::from_rgb(0, 0, 0);
const SURFACE: Color32 = Color32::from_rgb(10, 10, 10);
const SURFACE_RAISED: Color32 = Color32::from_rgb(18, 18, 18);
const TEXT: Color32 = Color32::from_rgb(237, 237, 237);
const MUTED: Color32 = Color32::from_rgb(161, 161, 161);
const SUBTLE: Color32 = Color32::from_rgb(24, 24, 24);
const BORDER: Color32 = Color32::from_rgb(38, 38, 38);
const ERROR: Color32 = Color32::from_rgb(255, 69, 58);
const OK: Color32 = Color32::from_rgb(0, 112, 243);
const SELECTED: Color32 = Color32::from_rgb(255, 255, 255);

pub fn run_gui() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Orpheus")
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([760.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Orpheus",
        options,
        Box::new(|cc| Ok(Box::new(OrpheusGui::new(cc)))),
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GuiDeviceKey {
    vid: u16,
    pid: u16,
    connection: ConnectionKind,
}

impl GuiDeviceKey {
    fn from_snapshot(device: &DeviceSnapshot) -> Self {
        Self {
            vid: device.vid,
            pid: device.pid,
            connection: device.connection,
        }
    }
}

struct OrpheusGui {
    worker: GuiWorker,
    devices: Vec<DeviceSnapshot>,
    selected_device: Option<GuiDeviceKey>,
    targets: HashMap<GuiDeviceKey, PollingRate>,
    status: String,
    last_error: Option<String>,
    last_refresh: Option<Instant>,
}

impl OrpheusGui {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_geist_fonts(&cc.egui_ctx);
        install_geist_style(&cc.egui_ctx);

        Self {
            worker: GuiWorker::spawn(),
            devices: Vec::new(),
            selected_device: None,
            targets: HashMap::new(),
            status: "Scanning for supported devices".to_string(),
            last_error: None,
            last_refresh: None,
        }
    }

    fn drain_worker_events(&mut self) {
        while let Ok(event) = self.worker.events.try_recv() {
            match event {
                WorkerEvent::Snapshot { devices, at } => {
                    self.last_refresh = Some(at);
                    self.last_error = None;
                    self.devices = devices;
                    self.reconcile_selection();
                    self.reconcile_targets();
                    self.status = if self.devices.is_empty() {
                        "No supported device visible".to_string()
                    } else {
                        format!("{} supported device(s)", self.devices.len())
                    };
                }
                WorkerEvent::Status(message) => {
                    self.status = message;
                }
                WorkerEvent::Error(message) => {
                    self.last_error = Some(message.clone());
                    self.status = message;
                }
            }
        }
    }

    fn reconcile_selection(&mut self) {
        if self.selected_device.is_some_and(|selected| {
            self.devices
                .iter()
                .any(|device| selected == GuiDeviceKey::from_snapshot(device))
        }) {
            return;
        }
        self.selected_device = self.devices.first().map(GuiDeviceKey::from_snapshot);
    }

    fn reconcile_targets(&mut self) {
        let visible = self
            .devices
            .iter()
            .map(GuiDeviceKey::from_snapshot)
            .collect::<Vec<_>>();
        self.targets.retain(|key, _| visible.contains(key));

        for device in &self.devices {
            let key = GuiDeviceKey::from_snapshot(device);
            self.targets.entry(key).or_insert_with(|| {
                device
                    .current_rate
                    .filter(|rate| device.supported_rates.contains(rate))
                    .or_else(|| {
                        device
                            .supported_rates
                            .iter()
                            .copied()
                            .find(|rate| *rate == PollingRate::Hz1000)
                    })
                    .or_else(|| device.supported_rates.first().copied())
                    .unwrap_or(PollingRate::Hz1000)
            });
        }
    }

    fn selected_device(&self) -> Option<&DeviceSnapshot> {
        let selected = self.selected_device?;
        self.devices
            .iter()
            .find(|device| GuiDeviceKey::from_snapshot(device) == selected)
    }

    fn target_for(&self, device: &DeviceSnapshot) -> Option<PollingRate> {
        self.targets
            .get(&GuiDeviceKey::from_snapshot(device))
            .copied()
    }
}

impl eframe::App for OrpheusGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_worker_events();
        ui.ctx().request_repaint_after(Duration::from_millis(250));

        Frame::new()
            .fill(BG)
            .inner_margin(Margin::symmetric(24, 18))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());

                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.heading(RichText::new("Orpheus").size(28.0).color(TEXT));
                        ui.label(RichText::new("Polling control for high-rate mice").color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_sized([88.0, 32.0], geist_button("Refresh"))
                            .clicked()
                        {
                            self.worker.send(WorkerCommand::Refresh);
                        }
                    });
                });

                ui.add_space(22.0);
                let body_height = (ui.available_height() - 36.0).max(260.0);
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(286.0, body_height),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(286.0);
                            ui.set_height(body_height);
                            draw_device_sidebar(ui, self);
                        },
                    );
                    ui.add_space(16.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), body_height),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(ui.available_width());
                            ui.set_height(body_height);
                            draw_device_detail(ui, self);
                        },
                    );
                });

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(6.0);
                    status_bar(ui, self);
                });
            });
    }
}

fn status_bar(ui: &mut egui::Ui, app: &OrpheusGui) {
    ui.horizontal(|ui| {
        let status_color = if app.last_error.is_some() {
            ERROR
        } else {
            MUTED
        };
        ui.label(RichText::new(&app.status).color(status_color));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(refresh_text(app.last_refresh)).color(MUTED));
        });
    });
}

fn draw_device_sidebar(ui: &mut egui::Ui, app: &mut OrpheusGui) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.label(RichText::new("Devices").strong().color(TEXT));
            ui.add_space(10.0);

            if app.devices.is_empty() {
                empty_panel(ui, "No supported devices found.");
                return;
            }

            ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    for device in &app.devices {
                        let key = GuiDeviceKey::from_snapshot(device);
                        let selected = app.selected_device == Some(key);
                        let stroke = if selected {
                            Stroke::new(1.0, SELECTED)
                        } else {
                            Stroke::new(1.0, BORDER)
                        };
                        let response = Frame::new()
                            .fill(if selected { SURFACE_RAISED } else { SURFACE })
                            .stroke(stroke)
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(Margin::same(12))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} {}",
                                                device.vendor_name, device.model_name
                                            ))
                                            .strong()
                                            .color(TEXT),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{:04x}:{:04x} · {}",
                                                device.vid, device.pid, device.connection
                                            ))
                                            .monospace()
                                            .color(MUTED),
                                        );
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        status_badge(ui, device);
                                    });
                                });
                            })
                            .response;
                        if response.clicked() {
                            app.selected_device = Some(key);
                        }
                        ui.add_space(8.0);
                    }
                });
        });
}

fn draw_device_detail(ui: &mut egui::Ui, app: &mut OrpheusGui) {
    let Some(device) = app.selected_device().cloned() else {
        empty_panel(ui, "Select a device to manage polling.");
        return;
    };

    let key = GuiDeviceKey::from_snapshot(&device);
    let target = app.target_for(&device);

    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(20))
        .show(ui, |ui| {
            ui.set_min_height(ui.available_height());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(format!("{} {}", device.vendor_name, device.model_name))
                            .size(22.0)
                            .strong()
                            .color(TEXT),
                    );
                    ui.label(
                        RichText::new(device.protocol.to_string())
                            .monospace()
                            .color(MUTED),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    status_badge(ui, &device);
                });
            });

            ui.add_space(18.0);
            ui.columns(3, |columns| {
                metric(
                    &mut columns[0],
                    "Current",
                    rate_text(device.current_rate, device.cached_rate),
                );
                metric(&mut columns[1], "Battery", battery_summary(&device));
                metric(
                    &mut columns[2],
                    "Mode",
                    format!(
                        "{:04x}:{:04x} {}",
                        device.vid, device.pid, device.connection
                    ),
                );
            });

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(18.0);

            ui.label(RichText::new("Target Rate").strong().color(TEXT));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for rate in &device.supported_rates {
                    let selected = target == Some(*rate);
                    let label = RichText::new(format!("{} Hz", rate.hz()))
                        .monospace()
                        .color(if selected { BG } else { TEXT });
                    let button = Button::new(label)
                        .fill(if selected { SELECTED } else { SUBTLE })
                        .stroke(Stroke::new(1.0, if selected { SELECTED } else { BORDER }));
                    let response = ui.add_sized([82.0, 34.0], button);
                    if response.clicked() {
                        app.targets.insert(key, *rate);
                    }
                }
            });

            ui.add_space(18.0);
            ui.horizontal(|ui| {
                let can_set = target.is_some() && device.current_rate != target;
                let label = target
                    .map(|rate| format!("Set {} Hz", rate.hz()))
                    .unwrap_or_else(|| "Set rate".to_string());
                if ui
                    .add_enabled(can_set, Button::new(RichText::new(label).strong()))
                    .clicked()
                {
                    if let Some(rate) = target {
                        app.worker.send(WorkerCommand::SetRate {
                            vid: device.vid,
                            pid: device.pid,
                            rate,
                        });
                        app.status =
                            format!("Queued {} for {:04x}:{:04x}", rate, device.vid, device.pid);
                    }
                }
                if device.current_rate == target {
                    ui.label(RichText::new("Already at target").color(MUTED));
                } else if device.cached_rate || device.current_rate.is_none() {
                    ui.label(RichText::new("Will retry when the device answers").color(MUTED));
                }
            });

            if let Some(error) = &device.read_error {
                ui.add_space(16.0);
                error_line(ui, "Read error", error);
            }
            if let Some(error) = &device.battery_error {
                ui.add_space(8.0);
                error_line(ui, "Battery error", error);
            }
        });
}

fn metric(ui: &mut egui::Ui, label: &str, value: String) {
    Frame::new()
        .fill(SURFACE_RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.label(RichText::new(label).color(MUTED));
            ui.add_space(6.0);
            ui.label(RichText::new(value).strong().monospace().color(TEXT));
        });
}

fn empty_panel(ui: &mut egui::Ui, message: &str) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(20))
        .show(ui, |ui| {
            ui.label(RichText::new(message).color(MUTED));
        });
}

fn status_badge(ui: &mut egui::Ui, device: &DeviceSnapshot) {
    let (label, color) = if device.read_error.is_some() {
        ("error", ERROR)
    } else if device.cached_rate || device.cached_battery {
        ("cached", MUTED)
    } else if device.battery_error.is_some() {
        ("battery", MUTED)
    } else {
        ("live", OK)
    };
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(12.0).color(color));
        });
}

fn error_line(ui: &mut egui::Ui, label: &str, message: &str) {
    Frame::new()
        .fill(Color32::from_rgb(37, 9, 9))
        .stroke(Stroke::new(1.0, Color32::from_rgb(127, 29, 29)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.label(RichText::new(label).strong().color(ERROR));
            ui.label(RichText::new(message).color(TEXT));
        });
}

fn geist_button(label: &str) -> Button<'_> {
    Button::new(RichText::new(label).strong())
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
}

fn rate_text(rate: Option<PollingRate>, cached: bool) -> String {
    match (rate, cached) {
        (Some(rate), true) => format!("{} Hz cached", rate.hz()),
        (Some(rate), false) => format!("{} Hz", rate.hz()),
        (None, _) => "unknown".to_string(),
    }
}

fn battery_summary(device: &DeviceSnapshot) -> String {
    let Some(battery) = device.battery else {
        return if device.battery_error.is_some() {
            "read error".to_string()
        } else {
            "unknown".to_string()
        };
    };

    let mut text = battery_level_text(battery);
    match battery.charge_state {
        ChargeState::Charging => text.push_str(" charging"),
        ChargeState::Full => text.push_str(" full"),
        ChargeState::Discharging => text.push_str(" discharging"),
        ChargeState::Unknown => {}
        ChargeState::Raw(raw) => text.push_str(&format!(" raw {raw}")),
    }
    if device.cached_battery {
        text.push_str(" cached");
    }
    text
}

fn battery_level_text(battery: BatteryStatus) -> String {
    battery
        .level_percent
        .map(|level| format!("{level}%"))
        .unwrap_or_else(|| "level unknown".to_string())
}

fn refresh_text(last_refresh: Option<Instant>) -> String {
    let Some(last_refresh) = last_refresh else {
        return "waiting for first scan".to_string();
    };
    let elapsed = last_refresh.elapsed().as_secs();
    if elapsed == 0 {
        "refreshed just now".to_string()
    } else {
        format!("refreshed {elapsed}s ago")
    }
}

fn install_geist_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "geist".to_string(),
        FontData::from_static(include_bytes!("../assets/fonts/Geist-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        "geist-medium".to_string(),
        FontData::from_static(include_bytes!("../assets/fonts/Geist-Medium.ttf")).into(),
    );
    fonts.font_data.insert(
        "geist-mono".to_string(),
        FontData::from_static(include_bytes!("../assets/fonts/GeistMono-Regular.ttf")).into(),
    );

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .splice(0..0, ["geist".to_string(), "geist-medium".to_string()]);
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "geist-mono".to_string());

    ctx.set_fonts(fonts);
}

fn install_geist_style(ctx: &egui::Context) {
    ctx.set_theme(Theme::Dark);
    let mut style = (*ctx.style_of(Theme::Dark)).clone();
    style.visuals = Visuals::dark();
    style.visuals.window_fill = BG;
    style.visuals.panel_fill = BG;
    style.visuals.widgets.inactive.bg_fill = SURFACE;
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    style.visuals.widgets.hovered.bg_fill = SUBTLE;
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    style.visuals.widgets.active.bg_fill = SELECTED;
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, BG);
    style.visuals.selection.bg_fill = SELECTED;
    style.visuals.selection.stroke = Stroke::new(1.0, BG);
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(24.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(14.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    ctx.set_style_of(Theme::Dark, style);
}

struct GuiWorker {
    commands: Sender<WorkerCommand>,
    events: Receiver<WorkerEvent>,
    handle: Option<JoinHandle<()>>,
}

impl GuiWorker {
    fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let handle = thread::spawn(move || worker_loop(command_rx, event_tx));
        Self {
            commands: command_tx,
            events: event_rx,
            handle: Some(handle),
        }
    }

    fn send(&self, command: WorkerCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for GuiWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum WorkerCommand {
    Refresh,
    SetRate {
        vid: u16,
        pid: u16,
        rate: PollingRate,
    },
    Shutdown,
}

#[derive(Debug)]
enum WorkerEvent {
    Snapshot {
        devices: Vec<DeviceSnapshot>,
        at: Instant,
    },
    Status(String),
    Error(String),
}

#[derive(Clone, Copy, Debug)]
struct PendingWorkerRate {
    vid: u16,
    pid: u16,
    rate: PollingRate,
}

fn worker_loop(commands: Receiver<WorkerCommand>, events: Sender<WorkerEvent>) {
    let monitor = match HidPollMonitor::new() {
        Ok(monitor) => monitor,
        Err(err) => {
            let _ = events.send(WorkerEvent::Error(format!(
                "failed to initialize HID: {err}"
            )));
            return;
        }
    };
    let mut cache = DeviceSnapshotCache::default();
    let mut pending_rate = None;
    scan_and_send(&monitor, &mut cache, &events);

    loop {
        match commands.recv_timeout(REFRESH_INTERVAL) {
            Ok(WorkerCommand::Refresh) => scan_and_send(&monitor, &mut cache, &events),
            Ok(WorkerCommand::SetRate { vid, pid, rate }) => {
                pending_rate = Some(PendingWorkerRate { vid, pid, rate });
                try_pending_rate(&monitor, &events, &mut pending_rate);
                scan_and_send(&monitor, &mut cache, &events);
            }
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                try_pending_rate(&monitor, &events, &mut pending_rate);
                scan_and_send(&monitor, &mut cache, &events);
            }
        }
    }
}

fn scan_and_send(
    monitor: &HidPollMonitor,
    cache: &mut DeviceSnapshotCache,
    events: &Sender<WorkerEvent>,
) {
    match monitor.scan() {
        Ok(mut devices) => {
            cache.apply(&mut devices);
            let _ = events.send(WorkerEvent::Snapshot {
                devices,
                at: Instant::now(),
            });
        }
        Err(err) => {
            let _ = events.send(WorkerEvent::Error(format!("scan failed: {err}")));
        }
    }
}

fn try_pending_rate(
    monitor: &HidPollMonitor,
    events: &Sender<WorkerEvent>,
    pending_rate: &mut Option<PendingWorkerRate>,
) {
    let Some(pending) = *pending_rate else {
        return;
    };

    match monitor.open_by_vid_pid(pending.vid, pending.pid) {
        Ok(device) => {
            if let Err(err) = device.set_rate(pending.rate) {
                let _ = events.send(WorkerEvent::Status(format!(
                    "Queued {} for {:04x}:{:04x}: {err}",
                    pending.rate, pending.vid, pending.pid
                )));
                return;
            }
            thread::sleep(Duration::from_millis(80));
            match device.read_rate() {
                Ok(after) if after == pending.rate => {
                    *pending_rate = None;
                    let _ = events.send(WorkerEvent::Status(format!(
                        "Set {:04x}:{:04x} to {after}",
                        pending.vid, pending.pid
                    )));
                }
                Ok(after) => {
                    let _ = events.send(WorkerEvent::Status(format!(
                        "Queued {} for {:04x}:{:04x}; device is still {after}",
                        pending.rate, pending.vid, pending.pid
                    )));
                }
                Err(err) => {
                    let _ = events.send(WorkerEvent::Status(format!(
                        "Queued {} for {:04x}:{:04x}: {err}",
                        pending.rate, pending.vid, pending.pid
                    )));
                }
            }
        }
        Err(err) => {
            let _ = events.send(WorkerEvent::Status(format!(
                "Queued {} for {:04x}:{:04x}: {err}",
                pending.rate, pending.vid, pending.pid
            )));
        }
    }
}
