//! The tray icon, and one program per user session.
//!
//! Closing the launcher keeps the program in the tray (as remote-control
//! tools do; setting `close_to_tray`): left click or "打开" shows it again,
//! "退出" ends every session and quits. Starting the program again while it
//! runs only brings the running one to the front (with a clicked pairing
//! link: hands it the link, through a file next to the settings).

use std::path::{Path, PathBuf};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE};

use crate::events::{Ui, UiEvent};

/// What the tray asks for.
#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    Open,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
}

/// Put the icon in the tray; its clicks arrive as `UiEvent::Tray`.
pub fn create(ui: Ui) -> Option<Tray> {
    let open = MenuItem::new("打开 NyaRemoteControl", true, None);
    let quit = MenuItem::new("退出", true, None);
    let menu = Menu::new();
    if let Err(e) = menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]) {
        tracing::warn!("tray menu: {e}");
    }
    let (open_id, quit_id): (MenuId, MenuId) = (open.id().clone(), quit.id().clone());
    let menu_ui = ui.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            menu_ui.send(UiEvent::Tray(TrayAction::Open));
        } else if e.id == quit_id {
            menu_ui.send(UiEvent::Tray(TrayAction::Quit));
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        let open = match e {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => true,
            TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => true,
            _ => false,
        };
        if open {
            ui.send(UiEvent::Tray(TrayAction::Open));
        }
    }));
    let icon = tray_icon::Icon::from_resource(1, None).ok();
    let mut b = TrayIconBuilder::new().with_tooltip("NyaRemoteControl").with_menu(Box::new(menu)).with_menu_on_left_click(false);
    if let Some(i) = icon {
        b = b.with_icon(i);
    }
    match b.build() {
        Ok(icon) => Some(Tray { _icon: icon }),
        Err(e) => {
            tracing::warn!("tray icon: {e}");
            None
        }
    }
}

/// This program's claim to the user session (kept for the process lifetime).
pub struct Instance {
    show: HANDLE,
}

/// Where a second start leaves its pairing link for the running program.
fn link_file(dir: &Path) -> PathBuf {
    dir.join("open-link.txt")
}

/// How long a relaunched program waits for the one that started it to quit.
const RELAUNCH_WAIT_MS: u32 = 15_000;

/// `Some` if this is the only instance; otherwise the running one was asked
/// to show itself (and open `link`) and this one should quit.
/// `relaunched`: started by the running program in its place (reopened as
/// administrator), which quits right after: wait for it, then take over.
pub fn claim(dir: &Path, link: Option<&str>, relaunched: bool) -> Option<Instance> {
    // SAFETY: named kernel objects of this user session; the handles live as
    // long as the process (the mutex marks it as running).
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\NyaRemoteControl.App"));
        let mut exists = GetLastError() == ERROR_ALREADY_EXISTS;
        if exists && relaunched {
            if let Ok(m) = mutex {
                // Ours once the previous program has quit (abandoned or released).
                let r = WaitForSingleObject(m, RELAUNCH_WAIT_MS);
                exists = r != WAIT_OBJECT_0 && r != WAIT_ABANDONED;
                if exists {
                    tracing::warn!("relaunched, but the previous program is still running");
                }
            }
        }
        if exists {
            if let Some(l) = link {
                if let Err(e) = std::fs::write(link_file(dir), l) {
                    tracing::warn!("hand over the link: {e}");
                }
            }
            if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\NyaRemoteControl.Show")) {
                let _ = SetEvent(ev);
            }
            return None;
        }
        // Left over from a start the previous program never read.
        let _ = std::fs::remove_file(link_file(dir));
        let show = CreateEventW(None, false, false, w!("Local\\NyaRemoteControl.Show")).ok()?;
        Some(Instance { show })
    }
}

impl Instance {
    /// Another start of the program shows this one (`UiEvent::Tray(Open)`),
    /// or opens the pairing link it was started with (`UiEvent::OpenLink`).
    pub fn listen(self, ui: Ui, dir: PathBuf) {
        let show = self.show.0 as isize; // a handle, valid in every thread
        std::thread::Builder::new()
            .name("single instance".into())
            .spawn(move || loop {
                // SAFETY: our own event handle, never closed.
                unsafe { WaitForSingleObject(HANDLE(show as *mut _), INFINITE) };
                let file = link_file(&dir);
                match std::fs::read_to_string(&file) {
                    Ok(link) => {
                        let _ = std::fs::remove_file(&file);
                        ui.send(UiEvent::OpenLink(link));
                    }
                    Err(_) => ui.send(UiEvent::Tray(TrayAction::Open)),
                }
            })
            .ok();
    }
}

/// Start with Windows (this user's sign-in), into the tray: the `Run` key of
/// the current user. Separate from the host service, which starts with the
/// computer whether anybody signs in or not.
pub mod autostart {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteKeyValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_SZ, RRF_RT_REG_SZ,
    };

    const KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    const NAME: PCWSTR = w!("NyaRemoteControl");

    /// The command the `Run` value should hold.
    fn command() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        Some(format!("\"{}\" --tray", exe.display()))
    }

    pub fn enabled() -> bool {
        let mut len = 0u32;
        // SAFETY: size query of a string value.
        unsafe { RegGetValueW(HKEY_CURRENT_USER, KEY, NAME, RRF_RT_REG_SZ, None, None, Some(&mut len)).is_ok() }
    }

    pub fn set(on: bool) -> anyhow::Result<()> {
        // SAFETY: the current user's Run key, a string value of our own.
        unsafe {
            if !on {
                let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, NAME);
                return if r.is_ok() || !enabled() { Ok(()) } else { Err(anyhow::anyhow!("删除开机启动项失败：{r:?}")) };
            }
            let cmd = command().ok_or_else(|| anyhow::anyhow!("找不到程序路径"))?;
            let wide: Vec<u16> = HSTRING::from(cmd).as_wide().iter().copied().chain(std::iter::once(0)).collect();
            let mut key = HKEY::default();
            RegOpenKeyExW(HKEY_CURRENT_USER, KEY, 0, KEY_SET_VALUE, &mut key).ok()?;
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let r = RegSetValueExW(key, NAME, 0, REG_SZ, Some(bytes));
            let _ = RegCloseKey(key);
            r.ok()?;
        }
        Ok(())
    }

    /// Keep an enabled entry pointing at this program (after an update or a move).
    pub fn refresh() {
        if enabled() {
            if let Err(e) = set(true) {
                tracing::warn!("start with Windows: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Writes the current user's Run key; only when asked for. Leaves it as it was.
    #[test]
    #[ignore]
    fn autostart_on_and_off() {
        use super::autostart;
        let before = autostart::enabled();
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = super::autostart::set(self.0);
            }
        }
        let _restore = Restore(before);
        autostart::set(true).unwrap();
        assert!(autostart::enabled());
        autostart::set(false).unwrap();
        assert!(!autostart::enabled());
        autostart::set(false).unwrap();
    }
}
