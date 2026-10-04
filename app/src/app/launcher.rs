//! The launcher: a web page (common/web/src/client) in a WebView2 child
//! window that covers the app window while no session runs. The page calls
//! commands here and follows the `state`, `connect` and `notice` events.

use nya_webui::{Call, WebUi};
use serde::Serialize;
use serde_json::{json, Value};

use super::{App, Pending};
use crate::config::Defaults;
use crate::events::UiEvent;

/// Where a connection attempt started from the launcher is (drives its dialogs).
#[derive(Debug, Clone, Default)]
pub enum Phase {
    #[default]
    Idle,
    Connecting(String),
    Pairing(String),
    PinChanged(String),
    Verify(String, String),
    /// A pairing link was opened: connect to this host (name, addresses)?
    Invite(String, Vec<String>),
}

impl Phase {
    fn json(&self) -> Value {
        match self {
            Phase::Idle => json!({ "phase": "idle" }),
            Phase::Connecting(l) => json!({ "phase": "connecting", "label": l }),
            Phase::Pairing(l) => json!({ "phase": "pairing", "label": l }),
            Phase::PinChanged(l) => json!({ "phase": "pin_changed", "label": l }),
            Phase::Verify(l, fp) => json!({ "phase": "verify", "label": l, "fingerprint": fp }),
            Phase::Invite(l, a) => json!({ "phase": "invite", "label": l, "addresses": a }),
        }
    }
}

#[derive(Serialize)]
struct HostJs<'a> {
    name: &'a str,
    address: &'a str,
    paired: bool,
    last_connected: u64,
    /// The name the host gives itself (empty until connected).
    server_name: &'a str,
    custom_name: bool,
    /// Own connection settings; `None` = the defaults.
    settings: Option<&'a Defaults>,
}

#[derive(Clone, Copy)]
pub enum Kind {
    Info,
    Error,
}

impl App {
    pub(super) fn create_web(&mut self) {
        let Some(window) = self.launcher.clone() else { return };
        let ui = self.ui_tx.clone();
        let dark = matches!(window.theme(), Some(winit::window::Theme::Dark));
        let opts = nya_webui::Options {
            page: "client.html",
            data_dir: self.data_dir.join("webview"),
            background: if dark { (0x17, 0x18, 0x1c) } else { (0xf7, 0xf8, 0xfa) },
        };
        match WebUi::new(&*window, window.inner_size(), opts, move |c| ui.send(UiEvent::Web(c))) {
            Ok(w) => self.web = Some(w),
            Err(e) => crate::fatal(&format!("{e:#}")),
        }
    }

    /// Show the session window (session started) or hide it and bring the
    /// launcher back (session ended).
    pub(super) fn show_session_window(&self, on: bool) {
        if let Some(win) = &self.window {
            win.set_visible(on);
            if on {
                win.focus_window();
            }
        }
        if !on {
            if let Some(l) = &self.launcher {
                l.set_visible(true);
                l.set_minimized(false);
                l.focus_window();
            }
        }
    }

    /// Events of the launcher window (the page handles its own input).
    pub(super) fn launcher_event(&mut self, el: &winit::event_loop::ActiveEventLoop, event: winit::event::WindowEvent) {
        use winit::event::WindowEvent;
        match event {
            WindowEvent::Resized(size) => {
                if let Some(w) = &self.web {
                    w.resize(size);
                }
            }
            WindowEvent::CloseRequested if self.cfg.close_to_tray && self.tray.is_some() => {
                // Into the tray: sessions and this computer's host keep running.
                if let Some(l) = &self.launcher {
                    l.set_visible(false);
                }
            }
            WindowEvent::CloseRequested if self.any_session() => {
                // The session goes on in its own window; keep the launcher reachable.
                if let Some(l) = &self.launcher {
                    l.set_minimized(true);
                }
            }
            WindowEvent::CloseRequested => {
                self.cancel_connect();
                self.exit = true;
                el.exit();
            }
            WindowEvent::Focused(true) => {
                // Remote sessions keep running; nothing to route here.
            }
            _ => {}
        }
    }

    fn web_state(&self) -> Value {
        let hosts: Vec<HostJs> = self
            .cfg
            .hosts
            .iter()
            .map(|h| HostJs {
                name: &h.name,
                address: &h.address,
                paired: !h.fingerprint.is_empty(),
                last_connected: h.last_connected,
                server_name: &h.server_name,
                custom_name: h.custom_name,
                settings: h.settings.as_ref(),
            })
            .collect();
        json!({
            "version": crate::version(),
            "computer": self.client_name(),
            "client_name": self.cfg.client_name,
            "check_updates": self.cfg.check_updates,
            "close_to_tray": self.cfg.close_to_tray,
            "autostart": crate::tray::autostart::enabled(),
            "update": self.updates.info,
            "computer_name": std::env::var("COMPUTERNAME").unwrap_or_default(),
            "decode": self.decode_summary,
            "hosts": hosts,
            "defaults": self.cfg.defaults,
        })
    }

    /// Hosts or settings changed: refresh the page.
    pub(super) fn push_state(&self) {
        if let Some(w) = &self.web {
            w.emit("state", &self.web_state());
        }
    }

    pub(super) fn set_phase(&mut self, p: Phase) {
        if let Some(w) = &self.web {
            w.emit("connect", &p.json());
        }
        self.phase = p;
    }

    /// A short message on the launcher.
    pub(super) fn notice(&self, kind: Kind, text: impl Into<String>) {
        let kind = match kind {
            Kind::Info => "info",
            Kind::Error => "error",
        };
        if let Some(w) = &self.web {
            w.emit("notice", &json!({ "kind": kind, "text": text.into() }));
        }
    }

    fn save_cfg(&self) -> Result<Value, String> {
        self.cfg.save(&self.data_dir).map_err(|e| format!("保存失败：{e:#}"))?;
        Ok(self.web_state())
    }

    /// The launcher back on screen (from the tray, or the program started again).
    pub(super) fn bring_launcher_back(&mut self) {
        if self.launcher_reveal.is_some() {
            return self.reveal_launcher();
        }
        if let Some(l) = &self.launcher {
            if let Some(w) = &self.web {
                w.resize(l.inner_size());
            }
            l.set_visible(true);
            l.set_minimized(false);
            l.focus_window();
        }
    }

    /// Show the launcher (created hidden) once its page is up.
    pub(super) fn reveal_launcher(&mut self) {
        let Some(deadline) = self.launcher_reveal.take() else { return };
        let waited = std::time::Duration::from_secs(4).saturating_sub(deadline.saturating_duration_since(std::time::Instant::now()));
        tracing::info!("launcher shown after {} ms", waited.as_millis());
        if let Some(l) = &self.launcher {
            // The page fills the window at its final size (DPI scaling may
            // have changed it since the web view was created).
            if let Some(w) = &self.web {
                w.resize(l.inner_size());
            }
            l.set_visible(true);
            l.focus_window();
        }
    }

    pub(super) fn on_web_call(&mut self, c: Call) {
        // The page's first request: it is loaded and drawn.
        self.reveal_launcher();
        if let Some(cmd) = c.cmd.strip_prefix("host.") {
            return self.host.call(self.web.as_ref(), &c, cmd);
        }
        let id = c.id;
        let r = self.web_call(c);
        if let Some(r) = r {
            if let Some(w) = &self.web {
                w.reply(id, r);
            }
        }
    }

    /// `None` = answered later (work on another thread).
    fn web_call(&mut self, c: Call) -> Option<Result<Value, String>> {
        #[derive(serde::Deserialize)]
        struct Addr {
            address: String,
            #[serde(default)]
            name: Option<String>,
        }
        let args = |c: &Call| c.args::<Addr>().map_err(|e| e.to_string());
        Some(match c.cmd.as_str() {
            "state" => Ok(self.web_state()),
            "start_page" => Ok(json!(self.start_page.take())),
            // Managing this computer as a host needs admin rights: reopen the app elevated.
            "relaunch_elevated" => (|| {
                if self.any_session() || self.pending.is_some() {
                    return Err("请先断开远程连接，再以管理员身份重新打开".to_string());
                }
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let page = c.args.get("page").and_then(Value::as_str).filter(|p| p.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')).unwrap_or("host");
                nya_win::package::start_elevated(&exe, &format!("--page {page} --relaunched")).map_err(|e| format!("{e:#}"))?;
                self.exit = true;
                Ok(Value::Null)
            })(),
            "connect" => (|| {
                let a = args(&c)?;
                if self.pending.is_some() {
                    return Err("正在连接中".to_string());
                }
                let address = a.address.trim().to_owned();
                if address.is_empty() {
                    return Err("请输入地址".to_string());
                }
                let address = self.cfg.find(&address).map(|h| h.address.clone()).unwrap_or(address);
                if self.connected_to(&address) {
                    return Err("已经连接着这台设备（见它的远程窗口）".to_string());
                }
                self.connect(address, a.name.filter(|n| !n.trim().is_empty()), None);
                Ok(Value::Null)
            })(),
            "pick_folder" => {
                let picked = rfd::FileDialog::new().set_title("选择要共享给被控端的文件夹").pick_folder();
                Ok(match picked {
                    Some(p) => json!({
                        "path": p.to_string_lossy(),
                        "name": p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().trim_end_matches(['\\', ':']).to_owned()),
                    }),
                    None => Value::Null,
                })
            }
            "cancel_connect" => {
                self.cancel_connect();
                Ok(Value::Null)
            }
            "pair" => {
                let code = c.args.get("code").and_then(|v| v.as_str()).map(str::to_owned);
                if let Some(tx) = self.pair_reply.take() {
                    let _ = tx.send(code.clone());
                }
                if code.is_none() {
                    self.cancel_connect();
                } else if let Some(p) = &self.pending {
                    let label = p.label.clone().unwrap_or_else(|| p.address.clone());
                    self.set_phase(Phase::Connecting(label));
                }
                Ok(Value::Null)
            }
            "pin_changed" => {
                let retry = c.args.get("retry").and_then(Value::as_bool).unwrap_or(false);
                self.set_phase(Phase::Idle);
                match self.pending.take() {
                    Some(mut p) if retry => {
                        p.reverify = true;
                        self.start_connect(p);
                    }
                    _ => self.pending = None,
                }
                Ok(Value::Null)
            }
            "invite_ok" => {
                let ok = c.args.get("ok").and_then(Value::as_bool).unwrap_or(false);
                self.set_phase(Phase::Idle);
                match self.offered.take() {
                    Some(invite) if ok && self.pending.is_none() => self.connect_invite(invite, None, None),
                    _ => {}
                }
                Ok(Value::Null)
            }
            // A pairing link on the clipboard (the add dialog offers it).
            "clipboard_link" => Ok(nya_win::clipboard::get_text()
                .ok()
                .flatten()
                .filter(|t| t.len() < 4096)
                .and_then(|t| nya_transport::invite::Invite::parse(&t).map(|i| i.to_link()))
                .map_or(Value::Null, Value::String)),
            "fingerprint_ok" => {
                let ok = c.args.get("ok").and_then(Value::as_bool).unwrap_or(false);
                self.set_phase(Phase::Idle);
                match self.verify_link.take() {
                    Some(link) if ok => self.finish_connect(link),
                    _ => {
                        self.pending = None;
                        self.notice(Kind::Error, "证书指纹未确认，已取消连接");
                    }
                }
                Ok(Value::Null)
            }
            "save_host" => (|| {
                let a = args(&c)?;
                let address = a.address.trim().to_owned();
                if address.is_empty() {
                    return Err("请输入地址".to_string());
                }
                if nya_transport::invite::Invite::parse(&address).is_some() {
                    return Err("配对链接要连接一次才能完成配对，请点“连接”".to_string());
                }
                if self.cfg.hosts.iter().any(|h| h.address == address) {
                    return Err("这个地址已经保存过了".to_string());
                }
                self.cfg.add(&address, &a.name.unwrap_or_default());
                self.save_cfg()
            })(),
            "rename_host" => (|| {
                let a = args(&c)?;
                let i = self.cfg.hosts.iter().position(|h| h.address == a.address).ok_or("设备不存在")?;
                self.cfg.rename(i, &a.name.unwrap_or_default())?;
                self.save_cfg()
            })(),
            "delete_host" => (|| {
                let a = args(&c)?;
                self.cfg.hosts.retain(|h| h.address != a.address);
                self.save_cfg()
            })(),
            // With `address`: that host's own settings; otherwise the defaults.
            "save_defaults" => (|| {
                let d: Defaults = serde_json::from_value(c.args.get("defaults").cloned().unwrap_or_default()).map_err(|e| format!("设置格式不对：{e}"))?;
                match c.args.get("address").and_then(Value::as_str) {
                    Some(address) => {
                        let h = self.cfg.hosts.iter_mut().find(|h| h.address == address).ok_or("设备不存在")?;
                        h.settings = Some(d);
                    }
                    None => self.cfg.defaults = d,
                }
                self.save_cfg()
            })(),
            "reset_host_settings" => (|| {
                let a = args(&c)?;
                let h = self.cfg.hosts.iter_mut().find(|h| h.address == a.address).ok_or("设备不存在")?;
                h.settings = None;
                self.save_cfg()
            })(),
            "update_check" => {
                self.check_update();
                Ok(Value::Null)
            }
            "update_apply" => self.install_update().map(|()| Value::Null),
            "set_autostart" => (|| {
                let on = c.args.get("on").and_then(Value::as_bool).unwrap_or(false);
                crate::tray::autostart::set(on).map_err(|e| format!("{e:#}"))?;
                Ok(self.web_state())
            })(),
            "set_close_to_tray" => (|| {
                self.cfg.close_to_tray = c.args.get("on").and_then(Value::as_bool).unwrap_or(true);
                self.save_cfg()
            })(),
            "set_check_updates" => (|| {
                self.cfg.check_updates = c.args.get("on").and_then(Value::as_bool).unwrap_or(true);
                if self.cfg.check_updates && self.updates.info.state == "idle" {
                    self.check_update();
                }
                self.save_cfg()
            })(),
            "set_client_name" => (|| {
                let name = c.args.get("name").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
                if name.chars().count() > 64 {
                    return Err("名称太长".to_string());
                }
                self.cfg.client_name = name;
                self.save_cfg()
            })(),
            "diag" => {
                let (ui, id) = (self.ui_tx.clone(), c.id);
                std::thread::spawn(move || {
                    let r = crate::diag::report().map(Value::String).map_err(|e| format!("{e:#}"));
                    ui.send(UiEvent::WebReply(id, r));
                });
                return None;
            }
            "open_logs" => {
                let _ = std::process::Command::new("explorer").arg(self.data_dir.join("logs")).spawn();
                Ok(Value::Null)
            }
            other => Err(format!("未知命令 {other}")),
        })
    }

    /// Label shown while connecting to `p`.
    pub(super) fn pending_label(p: &Pending) -> String {
        p.label.clone().unwrap_or_else(|| p.address.clone())
    }
}
