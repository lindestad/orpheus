use std::{
    collections::HashMap,
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::{
    SurfaceConfig, WgpuConfiguration,
    egui::{
        self, Align, Button, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
        Frame, Layout, Margin, RichText, ScrollArea, Stroke, TextStyle, Theme, Vec2,
        ViewportBuilder, Visuals,
    },
    egui_wgpu::{WgpuSetup, WgpuSetupCreateNew},
    wgpu,
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
const ACCENT: Color32 = Color32::from_rgb(245, 158, 11);

#[derive(Clone, Copy, Debug, Default)]
pub struct GuiOptions {
    pub software_renderer: bool,
    pub steady_repaint: bool,
}

pub fn run_gui(gui_options: GuiOptions) -> Result<()> {
    #[cfg(windows)]
    set_windows_app_id()?;
    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Orpheus")
            .with_icon(app_icon()?)
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([760.0, 500.0]),
        wgpu_options: gui_wgpu_options(gui_options),
        ..Default::default()
    };

    eframe::run_native(
        "Orpheus",
        native_options,
        Box::new(move |cc| Ok(Box::new(OrpheusGui::new(cc, gui_options)))),
    )?;
    Ok(())
}

fn app_icon() -> Result<egui::IconData> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .context("failed to decode app icon")
}

#[cfg(windows)]
fn set_windows_app_id() -> Result<()> {
    let app_id: Vec<u16> = "Orpheus\0".encode_utf16().collect();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr())
    };
    anyhow::ensure!(
        result >= 0,
        "failed to set Windows app identity: {result:#x}"
    );
    Ok(())
}

#[cfg(windows)]
fn set_windows_taskbar_icon(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::{
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{ICON_BIG, LoadIconW, SendMessageW, WM_SETICON},
    };
    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    // Use the actual app HWND; the foreground window may belong to another app at launch.
    unsafe {
        let icon = LoadIconW(GetModuleHandleW(std::ptr::null()), 1usize as *const u16);
        if !icon.is_null() {
            SendMessageW(
                handle.hwnd.get() as _,
                WM_SETICON,
                ICON_BIG as usize,
                icon as isize,
            );
        }
    }
}

fn gui_wgpu_options(options: GuiOptions) -> WgpuConfiguration {
    let mut wgpu_options = WgpuConfiguration::default().with_surface_config(SurfaceConfig {
        present_mode: wgpu::PresentMode::AutoVsync,
        ..SurfaceConfig::LOW_LATENCY
    });

    if options.software_renderer {
        let mut setup = WgpuSetupCreateNew::without_display_handle();
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
        setup.power_preference = wgpu::PowerPreference::LowPower;
        setup.native_adapter_selector = Some(Arc::new(select_software_adapter));
        wgpu_options.wgpu_setup = WgpuSetup::CreateNew(setup);
    }

    wgpu_options
}

fn select_software_adapter(
    adapters: &[wgpu::Adapter],
    surface: Option<&wgpu::Surface<'_>>,
) -> std::result::Result<wgpu::Adapter, String> {
    adapters
        .iter()
        .filter(|adapter| surface.is_none_or(|surface| adapter.is_surface_supported(surface)))
        .find(|adapter| {
            let info = adapter.get_info();
            info.device_type == wgpu::DeviceType::Cpu || is_windows_software_adapter(&info.name)
        })
        .cloned()
        .with_context(|| describe_adapter_failure(adapters))
        .map_err(|err| err.to_string())
}

fn is_windows_software_adapter(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("warp") || name.contains("microsoft basic render")
}

fn describe_adapter_failure(adapters: &[wgpu::Adapter]) -> String {
    let adapters = adapters
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            format!("{} ({:?}, {:?})", info.name, info.device_type, info.backend)
        })
        .collect::<Vec<_>>()
        .join(", ");
    if adapters.is_empty() {
        "no wgpu adapters were available for software rendering".to_string()
    } else {
        format!("no software/WARP adapter was available; found: {adapters}")
    }
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
    options: GuiOptions,
    worker: GuiWorker,
    devices: Vec<DeviceSnapshot>,
    selected_device: Option<GuiDeviceKey>,
    targets: HashMap<GuiDeviceKey, PollingRate>,
    pending_rates: HashMap<GuiDeviceKey, PollingRate>,
    status: String,
    rate_status: Option<String>,
    last_error: Option<String>,
    last_refresh: Option<Instant>,
}

impl OrpheusGui {
    fn new(cc: &eframe::CreationContext<'_>, options: GuiOptions) -> Self {
        #[cfg(windows)]
        set_windows_taskbar_icon(cc);
        install_geist_fonts(&cc.egui_ctx);
        install_geist_style(&cc.egui_ctx);

        Self {
            options,
            worker: GuiWorker::spawn(cc.egui_ctx.clone()),
            devices: Vec::new(),
            selected_device: None,
            targets: HashMap::new(),
            pending_rates: HashMap::new(),
            status: "Scanning for supported devices".to_string(),
            rate_status: None,
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
                WorkerEvent::RateStatus {
                    key,
                    pending,
                    message,
                } => {
                    if let Some(rate) = pending {
                        self.pending_rates.insert(key, rate);
                    } else {
                        self.pending_rates.remove(&key);
                    }
                    self.rate_status = Some(message);
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
        let focused = ui.ctx().input(|input| input.focused);
        let repaint_interval = if self.options.steady_repaint && focused {
            Duration::from_millis(16)
        } else {
            REFRESH_INTERVAL
        };
        ui.ctx().request_repaint_after(repaint_interval);

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
                let body_height = (ui.available_height() - 44.0).max(0.0);
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
        let refresh = refresh_text(app.last_refresh);
        let refresh_width = ui.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(refresh.clone(), TextStyle::Body.resolve(ui.style()), MUTED)
                .size()
                .x
        });
        let status_color = if app.last_error.is_some() {
            ERROR
        } else {
            MUTED
        };
        let status = if app.last_error.is_some() {
            &app.status
        } else {
            app.rate_status.as_ref().unwrap_or(&app.status)
        };
        ui.add_sized(
            [
                (ui.available_width() - refresh_width - ui.spacing().item_spacing.x).max(0.0),
                20.0,
            ],
            egui::Label::new(RichText::new(status).color(status_color))
                .wrap_mode(egui::TextWrapMode::Truncate),
        )
        .on_hover_text(status);
        ui.label(RichText::new(refresh).color(MUTED));
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
                            Stroke::new(1.0, ACCENT)
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
                                });
                                status_badge(ui, device);
                            })
                            .response
                            .interact(egui::Sense::click());
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
    let mut target = app.target_for(&device);

    ScrollArea::vertical()
        .id_salt("device-detail")
        .auto_shrink([false, false])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(20))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    device.vendor_name, device.model_name
                                ))
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
                                .fill(if selected { ACCENT } else { SUBTLE })
                                .stroke(Stroke::new(1.0, if selected { ACCENT } else { BORDER }));
                            let response = ui.add_sized([82.0, 34.0], button);
                            if response.clicked() {
                                app.targets.insert(key, *rate);
                                target = Some(*rate);
                            }
                        }
                    });

                    ui.add_space(18.0);
                    ui.horizontal_wrapped(|ui| {
                        let pending = app.pending_rates.get(&key).copied();
                        let can_set = can_set_rate(&device, target, pending);
                        let label = target
                            .map(|rate| format!("Set {} Hz", rate.hz()))
                            .unwrap_or_else(|| "Set rate".to_string());
                        if ui
                            .add_enabled(
                                can_set,
                                Button::new(RichText::new(label).strong().color(TEXT)),
                            )
                            .clicked()
                            && let Some(rate) = target
                        {
                            app.worker.send(WorkerCommand::SetRate { key, rate });
                            app.pending_rates.insert(key, rate);
                            app.rate_status = Some(format!(
                                "Queued {} for {:04x}:{:04x}",
                                rate, device.vid, device.pid
                            ));
                        }
                        if let Some(rate) = pending {
                            ui.label(RichText::new(format!("Queued {rate}")).color(MUTED));
                        } else if device.current_rate == target && !device.cached_rate {
                            ui.label(RichText::new("Already at target").color(MUTED));
                        } else if device.protocol.supports_rate_read()
                            && (device.cached_rate || device.current_rate.is_none())
                        {
                            ui.label(
                                RichText::new("Will retry when the device answers").color(MUTED),
                            );
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
        });
}

fn can_set_rate(
    device: &DeviceSnapshot,
    target: Option<PollingRate>,
    pending: Option<PollingRate>,
) -> bool {
    target.is_some()
        && target != pending
        && (device.cached_rate || device.current_rate != target || pending.is_some())
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
        ("live", ACCENT)
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
    Button::new(RichText::new(label).strong().color(TEXT))
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
    style.visuals.widgets.active.bg_fill = ACCENT;
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, BG);
    style.visuals.selection.bg_fill = ACCENT;
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
    fn spawn(ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let handle = thread::spawn(move || worker_loop(command_rx, event_tx, ctx));
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
        key: GuiDeviceKey,
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
    RateStatus {
        key: GuiDeviceKey,
        pending: Option<PollingRate>,
        message: String,
    },
    Error(String),
}

fn worker_loop(commands: Receiver<WorkerCommand>, events: Sender<WorkerEvent>, ctx: egui::Context) {
    let mut monitor = match HidPollMonitor::new() {
        Ok(monitor) => monitor,
        Err(err) => {
            let _ = events.send(WorkerEvent::Error(format!(
                "failed to initialize HID: {err}"
            )));
            ctx.request_repaint();
            return;
        }
    };
    let mut cache = DeviceSnapshotCache::default();
    let mut pending_rates = HashMap::new();
    scan_and_send(&mut monitor, &mut cache, &events, &mut pending_rates);
    ctx.request_repaint();

    loop {
        match commands.recv_timeout(REFRESH_INTERVAL) {
            Ok(WorkerCommand::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Ok(WorkerCommand::SetRate { key, rate }) => {
                pending_rates.insert(key, rate);
            }
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
        scan_and_send(&mut monitor, &mut cache, &events, &mut pending_rates);
        ctx.request_repaint();
    }
}

fn scan_and_send(
    monitor: &mut HidPollMonitor,
    cache: &mut DeviceSnapshotCache,
    events: &Sender<WorkerEvent>,
    pending_rates: &mut HashMap<GuiDeviceKey, PollingRate>,
) {
    if let Err(err) = monitor.refresh_devices() {
        let _ = events.send(WorkerEvent::Error(format!("device refresh failed: {err}")));
        return;
    }
    try_pending_rates(events, pending_rates, |key, rate| {
        let device = monitor.open_by_vid_pid(key.vid, key.pid)?;
        device.set_rate(rate)?;
        verify_rate_write(rate, device.supports_rate_read(), || {
            thread::sleep(Duration::from_millis(80));
            device.read_rate()
        })?;
        Ok(device.supports_rate_read())
    });
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

fn try_pending_rates(
    events: &Sender<WorkerEvent>,
    pending_rates: &mut HashMap<GuiDeviceKey, PollingRate>,
    mut apply: impl FnMut(GuiDeviceKey, PollingRate) -> Result<bool>,
) {
    pending_rates.retain(|key, rate| {
        let result = apply(*key, *rate);
        let (pending, message) = rate_write_status(*key, *rate, result);
        let _ = events.send(WorkerEvent::RateStatus {
            key: *key,
            pending,
            message,
        });
        pending.is_some()
    });
}

fn verify_rate_write(
    target: PollingRate,
    supports_rate_read: bool,
    read_rate: impl FnOnce() -> Result<PollingRate>,
) -> Result<()> {
    if supports_rate_read {
        let after = read_rate()?;
        anyhow::ensure!(after == target, "device is still {after}");
    }
    Ok(())
}

fn rate_write_status(
    key: GuiDeviceKey,
    rate: PollingRate,
    result: Result<bool>,
) -> (Option<PollingRate>, String) {
    match result {
        Ok(true) => (
            None,
            format!("Set {:04x}:{:04x} to {rate}", key.vid, key.pid),
        ),
        Ok(false) => (
            None,
            format!(
                "Sent {rate} to {:04x}:{:04x}; current-rate verification unavailable",
                key.vid, key.pid
            ),
        ),
        Err(err) => (
            Some(rate),
            format!("Queued {rate} for {:04x}:{:04x}: {err}", key.vid, key.pid),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::find_model;

    fn snapshot(vid: u16, pid: u16) -> DeviceSnapshot {
        let (model, connection) = find_model(vid, pid).unwrap();
        DeviceSnapshot {
            path: format!("{vid:04x}:{pid:04x}"),
            vid,
            pid,
            product_name: None,
            vendor_name: model.vendor_name,
            model_name: model.name,
            connection,
            protocol: model.protocol,
            supported_rates: model.supported_rates(connection).to_vec(),
            current_rate: Some(PollingRate::Hz1000),
            battery: None,
            cached_rate: false,
            cached_battery: false,
            battery_error: None,
            read_error: None,
        }
    }

    fn app() -> (OrpheusGui, Sender<WorkerEvent>) {
        let (commands, _) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        let mut app = OrpheusGui {
            options: GuiOptions::default(),
            worker: GuiWorker {
                commands,
                events: receiver,
                handle: None,
            },
            devices: vec![snapshot(0x372e, 0x1014), snapshot(0x33e4, 0x3517)],
            selected_device: None,
            targets: HashMap::new(),
            pending_rates: HashMap::new(),
            status: String::new(),
            rate_status: None,
            last_error: None,
            last_refresh: None,
        };
        app.reconcile_selection();
        app.reconcile_targets();
        (app, events)
    }

    #[test]
    fn clicking_a_device_row_selects_it() {
        let (mut app, _) = app();
        let ctx = egui::Context::default();
        let draw = |app: &mut OrpheusGui, events| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(300.0, 500.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| draw_device_sidebar(ui, app),
            )
        };
        let output = draw(&mut app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().contains("Fenrir") => {
                    Some(text.pos + text.galley.rect.size() * 0.5)
                }
                _ => None,
            })
            .expect("second device label must be visible");
        let _ = draw(
            &mut app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let _ = draw(
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(
            app.selected_device,
            Some(GuiDeviceKey::from_snapshot(&app.devices[1]))
        );
    }

    #[test]
    fn snapshots_preserve_rate_change_status() {
        let (mut app, events) = app();
        let message = "Queued 4000 Hz for sleeping mouse";
        events
            .send(WorkerEvent::RateStatus {
                key: GuiDeviceKey::from_snapshot(&app.devices[0]),
                pending: Some(PollingRate::Hz4000),
                message: message.to_string(),
            })
            .unwrap();
        events
            .send(WorkerEvent::Snapshot {
                devices: app.devices.clone(),
                at: Instant::now(),
            })
            .unwrap();
        app.drain_worker_events();
        assert_eq!(app.rate_status.as_deref(), Some(message));
    }

    #[test]
    fn cached_rate_does_not_disable_setting_the_same_target() {
        let mut device = snapshot(0x33e4, 0x3517);
        let target = Some(PollingRate::Hz1000);
        assert!(!can_set_rate(&device, target, None));
        device.cached_rate = true;
        assert!(can_set_rate(&device, target, None));
        assert!(!can_set_rate(&device, target, target));
        assert!(!can_set_rate(&device, None, None));
        device.cached_rate = false;
        assert!(can_set_rate(&device, target, Some(PollingRate::Hz4000)));
    }

    #[test]
    fn write_only_protocol_does_not_attempt_verification() {
        verify_rate_write(PollingRate::Hz1000, false, || {
            panic!("a write-only protocol must not attempt a current-rate read")
        })
        .unwrap();
    }

    #[test]
    fn readable_protocol_requires_the_requested_rate() {
        assert!(
            verify_rate_write(PollingRate::Hz4000, true, || { Ok(PollingRate::Hz1000) }).is_err()
        );
        assert!(
            verify_rate_write(PollingRate::Hz4000, true, || {
                Err(anyhow::anyhow!("sleeping"))
            })
            .is_err()
        );
        verify_rate_write(PollingRate::Hz4000, true, || Ok(PollingRate::Hz4000)).unwrap();
    }

    #[test]
    fn pending_requests_are_independent_and_success_is_not_retried() {
        let (app, events) = app();
        let first = GuiDeviceKey::from_snapshot(&app.devices[0]);
        let second = GuiDeviceKey::from_snapshot(&app.devices[1]);
        let mut pending =
            HashMap::from([(first, PollingRate::Hz4000), (second, PollingRate::Hz8000)]);
        let mut attempted = Vec::new();
        try_pending_rates(&events, &mut pending, |key, rate| {
            attempted.push((key, rate));
            if key == first {
                Err(anyhow::anyhow!("sleeping"))
            } else {
                Ok(false)
            }
        });
        assert_eq!(attempted.len(), 2);
        assert_eq!(pending, HashMap::from([(first, PollingRate::Hz4000)]));
        try_pending_rates(&events, &mut pending, |key, rate| {
            assert_eq!((key, rate), (first, PollingRate::Hz4000));
            Ok(true)
        });
        assert!(pending.is_empty());
        try_pending_rates(&events, &mut pending, |_, _| {
            panic!("successful writes must not repeat")
        });
    }

    #[test]
    fn long_errors_and_status_stay_within_their_panels() {
        let (mut app, _) = app();
        app.devices[0].read_error = Some("A sleeping device did not answer. ".repeat(50));
        app.rate_status = Some("Queued rate change for a sleeping mouse. ".repeat(20));
        let ctx = egui::Context::default();
        install_geist_fonts(&ctx);
        install_geist_style(&ctx);
        for size in [Vec2::new(400.0, 300.0), Vec2::new(600.0, 450.0)] {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ui| {
                    let max = ui.max_rect();
                    draw_device_detail(ui, &mut app);
                    assert!(ui.min_rect().max.y <= max.max.y + 1.0);
                    assert!(ui.min_rect().max.x <= max.max.x + 1.0);
                },
            );
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ui| {
                    let max = ui.max_rect();
                    status_bar(ui, &app);
                    assert!(ui.min_rect().max.x <= max.max.x + 1.0);
                },
            );
        }
    }
}
