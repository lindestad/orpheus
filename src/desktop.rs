use anyhow::Result;
use eframe::egui;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct OpenRequest {
    pending: Arc<AtomicBool>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl OpenRequest {
    pub fn attach(&self, ctx: egui::Context) {
        *self.ctx.lock().unwrap() = Some(ctx);
    }

    pub fn take(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Open,
    Settings,
    Quit,
}

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::Context;
    use std::{
        path::PathBuf,
        sync::mpsc::{self, Receiver},
    };
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };
    use windows_registry::CURRENT_USER;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub struct Instance {
        handle: windows_sys::Win32::Foundation::HANDLE,
        open: OpenRequest,
        message: u32,
    }

    impl Instance {
        pub fn acquire() -> Result<Option<Self>> {
            use windows_sys::Win32::{
                Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
                System::Threading::CreateMutexW,
                UI::WindowsAndMessaging::{FindWindowW, PostMessageW, RegisterWindowMessageW},
            };
            let name: Vec<u16> = "Local\\Orpheus.Gui\0".encode_utf16().collect();
            let message_name: Vec<u16> = "Orpheus.Open\0".encode_utf16().collect();
            // Keep the mutex handle for the entire GUI lifetime, including while hidden.
            unsafe {
                let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
                anyhow::ensure!(!handle.is_null(), "failed to create app instance mutex");
                let exists = GetLastError() == ERROR_ALREADY_EXISTS;
                let message = RegisterWindowMessageW(message_name.as_ptr());
                if message == 0 {
                    CloseHandle(handle);
                    anyhow::bail!("failed to register app restore message");
                }
                if exists {
                    CloseHandle(handle);
                    let title: Vec<u16> = "Orpheus\0".encode_utf16().collect();
                    for _ in 0..30 {
                        let window = FindWindowW(std::ptr::null(), title.as_ptr());
                        if !window.is_null() {
                            anyhow::ensure!(
                                PostMessageW(window, message, 0, 0) != 0,
                                "failed to restore running app"
                            );
                            return Ok(None);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    anyhow::bail!("Orpheus is already starting; try opening it again shortly");
                }
                Ok(Some(Self {
                    handle,
                    open: OpenRequest::default(),
                    message,
                }))
            }
        }

        pub fn open_request(&self) -> OpenRequest {
            self.open.clone()
        }

        pub fn event_loop_hook(&self) -> eframe::EventLoopBuilderHook {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            let open = self.open.clone();
            let message = self.message;
            Box::new(move |builder| {
                builder.with_msg_hook(move |raw| {
                    let msg = unsafe {
                        &*(raw as *const windows_sys::Win32::UI::WindowsAndMessaging::MSG)
                    };
                    if msg.message == message {
                        open.pending.store(true, Ordering::Release);
                        if let Some(ctx) = open.ctx.lock().unwrap().as_ref() {
                            ctx.request_repaint();
                        }
                        true
                    } else {
                        false
                    }
                });
            })
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }

    pub struct Tray {
        _icon: TrayIcon,
        actions: Receiver<TrayAction>,
    }

    impl Tray {
        pub fn new(ctx: egui::Context, icon: egui::IconData) -> Result<Self> {
            let menu = Menu::new();
            let open = MenuItem::new("Open Orpheus", true, None);
            let settings = MenuItem::new("Settings", true, None);
            let quit = MenuItem::new("Quit Orpheus", true, None);
            menu.append_items(&[&open, &settings, &PredefinedMenuItem::separator(), &quit])?;
            let icon = TrayIconBuilder::new()
                .with_tooltip("Orpheus")
                .with_icon(Icon::from_rgba(icon.rgba, icon.width, icon.height)?)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()?;
            let (tx, actions) = mpsc::channel();
            let open_id = open.id().clone();
            let settings_id = settings.id().clone();
            let quit_id = quit.id().clone();
            let menu_ctx = ctx.clone();
            let menu_tx = tx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let action = if event.id == open_id {
                    TrayAction::Open
                } else if event.id == settings_id {
                    TrayAction::Settings
                } else if event.id == quit_id {
                    TrayAction::Quit
                } else {
                    return;
                };
                let _ = menu_tx.send(action);
                menu_ctx.request_repaint();
            }));
            let tray_id = icon.id().clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                if event.id() == &tray_id
                    && matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } | TrayIconEvent::DoubleClick {
                            button: MouseButton::Left,
                            ..
                        }
                    )
                {
                    let _ = tx.send(TrayAction::Open);
                    ctx.request_repaint();
                }
            }));
            Ok(Self {
                _icon: icon,
                actions,
            })
        }

        pub fn try_action(&self) -> Option<TrayAction> {
            self.actions.try_recv().ok()
        }
    }

    fn legacy_shortcut() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("user startup directory unavailable")?
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup\Orpheus.lnk"))
    }

    pub fn startup_enabled() -> Result<bool> {
        let registered = match CURRENT_USER.open(RUN_KEY) {
            Ok(key) => match key.get_string("Orpheus") {
                Ok(_) => true,
                Err(err) if err.code().0 as u32 == 0x80070002 => false,
                Err(err) => return Err(err.into()),
            },
            Err(err) if err.code().0 as u32 == 0x80070002 => false,
            Err(err) => return Err(err.into()),
        };
        Ok(registered || legacy_shortcut()?.is_file())
    }

    pub fn set_startup(enabled: bool) -> Result<()> {
        let key = CURRENT_USER.create(RUN_KEY)?;
        if enabled {
            let installed = dirs::data_local_dir()
                .context("local app directory unavailable")?
                .join(r"Programs\Orpheus\orpheus.exe");
            let exe = if installed.is_file() {
                installed
            } else {
                std::env::current_exe()?
            };
            key.set_string("Orpheus", format!("\"{}\" gui", exe.display()))?;
        } else if let Err(err) = key.remove_value("Orpheus")
            && err.code().0 as u32 != 0x80070002
        {
            return Err(err.into());
        }
        // Replace the old installer shortcut so startup never launches two copies.
        match std::fs::remove_file(legacy_shortcut()?) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("failed to remove legacy startup shortcut"),
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use windows::{Instance, Tray, set_startup, startup_enabled};

#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn try_action(&self) -> Option<TrayAction> {
        None
    }
}

#[cfg(not(windows))]
pub fn startup_enabled() -> Result<bool> {
    Ok(false)
}

#[cfg(not(windows))]
pub fn set_startup(_: bool) -> Result<()> {
    anyhow::bail!("Windows startup is unavailable on this platform")
}
