//! "本机": this computer as a remote-control host, a section of the main
//! page (common/web/src/host). The page calls `host.<cmd>`; this side keeps
//! the model — service state, the control pipe (or the files while the
//! service is stopped), background jobs — and pushes `host.snapshot`
//! (whenever something changed), `host.job` (a background job finished)
//! and `host.install` (component install progress).
//!
//! Commands: snapshot, enable, svc, reset_code, set_config, remove_client,
//! disconnect, diag, components, install, log, open_logs, open_url,
//! open_sound_settings; relaunch_elevated is the app's (launcher.rs).
//!
//! Without admin rights only the status is available (the service tells
//! non-administrators nothing else: a paired client controls the computer
//! with the service's rights). `enable` then installs the service through an
//! elevated `nya-server.exe install`; everything else needs the app reopened
//! as administrator.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use nya_webui::{Call, WebUi};
use serde::Serialize;
use serde_json::{json, Value};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use nya_server_core::backend::Backend;
use nya_server_core::config::{ServerConfig, ENCODERS};
use nya_server_core::control_pb::{self as cpb, event::Kind};
use nya_server_core::SERVICE_NAME;
use nya_server_core::{components, install, paths, win as winutil};

use crate::events::{Ui, UiEvent};

const TOOL: &str = "NyaRemoteControl";
/// The command-line program next to us; it installs the service when we are not elevated.
const CLI_EXE: &str = "nya-server.exe";

fn service_exists(name: &str) -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Is the (driver) service loaded? The host itself checks by connecting to it.
fn service_running(name: &str) -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS))
        .and_then(|s| s.query_status())
        .is_ok_and(|s| s.current_state == ServiceState::Running)
}

/// An optional third-party component: installed only when the user asks.
#[derive(Serialize)]
pub struct Component {
    id: &'static str,
    /// What it is for, as users call it.
    name: &'static str,
    /// The driver / program's own name.
    product: &'static str,
    purpose: &'static str,
    status: Option<String>,
    installed: bool,
    url: &'static str,
    note: &'static str,
}

fn component_id(id: &str) -> Option<(components::Id, &'static str)> {
    Some(match id {
        "vdd" => (components::Id::Vdd, "虚拟显示器"),
        "cable" => (components::Id::Cable, "虚拟声卡"),
        "vigem" => (components::Id::Vigem, "手柄"),
        "usbip" => (components::Id::Usbip, "USB 透传"),
        "winfsp" => (components::Id::Winfsp, "文件夹挂载"),
        "printer" => (components::Id::Printer, "打印到客户端"),
        _ => return None,
    })
}

/// Detect optional components (COM is initialised on the calling thread).
fn detect_components() -> Vec<Component> {
    nya_win::com_init();
    let cable = components::cable_device_name();
    let vdd_installed = nya_win::devnode::exists(components::VDD_HWID);
    let vdd = vdd_installed.then(|| {
        if nya_win::devnode::is_started(components::VDD_HWID) {
            "正在使用（有客户端选择了虚拟显示器 / 隐私屏）".to_owned()
        } else {
            "平时停用，有客户端需要时自动启用".to_owned()
        }
    });
    let usbip = components::usbip_exe().map(|p| p.display().to_string());
    let winfsp = components::winfsp_dll().map(|p| p.display().to_string());
    vec![
        Component {
            id: "vdd",
            name: "虚拟显示器",
            product: "Virtual Display Driver 25.7.23",
            purpose: "在被控端新建显示器：分辨率跟随客户端窗口、多屏、隐私屏（本机显示器黑屏、本机键鼠屏蔽）。需要服务模式",
            installed: vdd_installed,
            status: vdd,
            url: "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases",
            note: "免费开源；平时保持停用，不影响本机显示器",
        },
        Component {
            id: "cable",
            name: "虚拟声卡",
            product: "VB-Cable",
            purpose: "把客户端麦克风送进被控端：客户端工具条打开“麦克风”，打开期间“CABLE Output”自动成为被控端默认麦克风",
            installed: cable.is_some(),
            status: cable.map(|n| {
                if nya_win::audio::default_render_is("CABLE") {
                    format!("{n}（注意：它现在是默认播放设备，本机会听不到声音，请在声音设置里把默认播放设备改回扬声器）")
                } else {
                    n
                }
            }),
            url: "https://vb-audio.com/Cable/",
            note: "捐赠软件（安装即表示同意 VB-Audio 许可），需联网从官网下载；安装后需要重启一次",
        },
        Component {
            id: "vigem",
            name: "手柄",
            product: "ViGEmBus 1.22.0",
            purpose: "客户端的手柄在被控端显示为 Xbox 手柄",
            installed: service_exists("ViGEmBus"),
            status: service_running("ViGEmBus").then(|| "驱动已加载".into()),
            url: "https://github.com/nefarius/ViGEmBus/releases",
            note: "免费；作者已停止维护，但仍可用。客户端插上 Xbox / XInput 手柄即自动使用",
        },
        Component {
            id: "usbip",
            name: "USB 透传",
            product: "usbip-win2 0.9.8.1",
            purpose: "U 盾、加密狗等 USB 设备从客户端透传到被控端",
            installed: usbip.is_some(),
            status: usbip,
            url: "https://github.com/vadimgrn/usbip-win2/releases",
            note: "客户端另需 usbipd-win（客户端工具条“USB 设备”里可一键安装）",
        },
        Component {
            id: "winfsp",
            name: "文件夹挂载",
            product: "WinFsp 2025 (2.1)",
            purpose: "客户端共享的文件夹出现在被控端的一个盘符里，被控端的程序可以直接打开、保存客户端的文件",
            installed: winfsp.is_some(),
            status: winfsp,
            url: "https://github.com/winfsp/winfsp/releases",
            note: "免费开源（GPLv3，含开源软件例外）；客户端在“连接设置 → 共享文件夹”里选择文件夹",
        },
        Component {
            id: "printer",
            name: "打印到客户端",
            product: "Windows 自带的 Microsoft Print to PDF",
            purpose: "被控端添加一台打印机“打印到 NyaRemoteControl 客户端”：打印的内容以 PDF 发给正在操作的客户端，用客户端的打印机打出来",
            installed: components::printer_installed(),
            status: components::printer_installed().then(|| components::PRINTER_NAME.to_owned()),
            url: "https://learn.microsoft.com/windows/client-management/",
            note: "不需要下载；需要服务模式。客户端可以选择直接打印、打开 PDF 或只保存",
        },
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SvcState {
    NotInstalled,
    Stopped,
    Running,
    Pending,
    Unknown,
}

impl SvcState {
    fn key(self) -> &'static str {
        match self {
            SvcState::NotInstalled => "not_installed",
            SvcState::Stopped => "stopped",
            SvcState::Running => "running",
            SvcState::Pending => "pending",
            SvcState::Unknown => "unknown",
        }
    }
}

fn service_state() -> SvcState {
    let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return SvcState::Unknown };
    match m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Err(_) => SvcState::NotInstalled,
        Ok(s) => match s.query_status().map(|x| x.current_state) {
            Ok(ServiceState::Running) => SvcState::Running,
            Ok(ServiceState::Stopped) => SvcState::Stopped,
            Ok(_) => SvcState::Pending,
            Err(_) => SvcState::Unknown,
        },
    }
}

fn control_service(start: bool, stop: bool) -> Result<String> {
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let s = m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP)?;
    if stop {
        install::stop_and_wait(&s);
    }
    if start {
        s.start(&[] as &[&std::ffi::OsStr])?;
    }
    Ok(match (start, stop) {
        (true, true) => "服务已重启".into(),
        (true, false) => "服务已启动".into(),
        _ => "服务已停止".into(),
    })
}

/// Diagnostics run in `nya-server-svc.exe`, which has the capture / encoding
/// stack; the report is saved to `out`.
fn run_diag(out: &Path) -> Result<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = paths::service_exe()?;
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    let r = std::process::Command::new(&exe)
        .arg("diag")
        .arg("--out")
        .arg(out)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| anyhow!("无法运行 {}：{e}", exe.display()))?;
    if !r.status.success() {
        return Err(anyhow!("诊断失败（{}）：{}", r.status, String::from_utf8_lossy(&r.stderr)));
    }
    Ok(std::fs::read_to_string(out)?)
}

fn open(target: impl AsRef<std::ffi::OsStr>) {
    let _ = std::process::Command::new("explorer").arg(target).spawn();
}

fn event_kind(k: i32) -> &'static str {
    match Kind::try_from(k).unwrap_or(Kind::Other) {
        Kind::Connected => "connected",
        Kind::Disconnected => "disconnected",
        Kind::Paired => "paired",
        Kind::PairingFailed => "pairing_failed",
        Kind::Rejected => "rejected",
        Kind::Service => "service",
        Kind::Other => "other",
    }
}

fn status_json(s: &cpb::Status) -> Value {
    json!({
        "server_version": s.server_version,
        "listen": s.listen,
        "listen_error": s.listen_error,
        "host": s.host.as_ref().map(|h| json!({ "running": h.running, "console_session": h.console_session, "stream": h.stream })),
        "session": s.session.as_ref().map(|c| json!({
            "client_name": c.client_name, "client_version": c.client_version,
            "remote_addr": c.remote_addr, "since_unix": c.since_unix,
        })),
        "viewers": s.viewers.iter().map(|c| json!({
            "client_name": c.client_name, "client_version": c.client_version,
            "remote_addr": c.remote_addr, "since_unix": c.since_unix,
        })).collect::<Vec<_>>(),
        "recent": s.recent.iter().rev().take(30).map(|e| json!({ "unix": e.unix, "kind": event_kind(e.kind), "text": e.text })).collect::<Vec<_>>(),
    })
}


/// Background work done, delivered through the app's event loop.
pub enum HostEvent {
    /// (call id or 0, label, result) of a background job.
    JobDone(u64, &'static str, Result<String, String>),
    Components(u64, Vec<Component>),
    InstallProgress,
}

struct Model {
    elevated: bool,
    dir: PathBuf,
    svc: SvcState,
    svc_checked: Instant,
    /// The running service (control pipe) or, while it is stopped, its files.
    /// `None` until (re)connected.
    backend: Option<Backend>,
    status: Option<cpb::Status>,
    /// Does the installed service run this directory's nya-server-svc.exe?
    points_here: Option<bool>,
    code: String,
    fingerprint: String,
    /// Full fingerprint (pairing links); empty from older services.
    fingerprint_hex: String,
    cfg: ServerConfig,
    clients: Vec<nya_server_core::auth::PairedClient>,
    load_error: Option<String>,
    busy: Option<&'static str>,
    install_job: Option<Arc<Mutex<InstallJob>>>,
}

impl Model {
    fn new() -> Self {
        let mut m = Self {
            elevated: winutil::is_elevated(),
            dir: paths::service_dir(),
            svc: SvcState::Unknown,
            svc_checked: Instant::now() - Duration::from_secs(10),
            backend: None,
            status: None,
            points_here: None,
            code: String::new(),
            fingerprint: String::new(),
            fingerprint_hex: String::new(),
            cfg: ServerConfig::default(),
            clients: Vec::new(),
            load_error: None,
            busy: None,
            install_job: None,
        };
        m.svc = service_state();
        m.reload();
        m
    }

    /// Run `f` on the backend, connecting first if needed. A failure drops
    /// the connection so the next call reconnects (the service may have restarted).
    fn with_backend<T>(&mut self, f: impl FnOnce(&mut Backend) -> Result<T>) -> Result<T> {
        if self.backend.is_none() {
            self.backend = Some(Backend::service(TOOL)?);
        }
        let r = f(self.backend.as_mut().unwrap());
        if r.is_err() {
            self.backend = None;
        }
        r
    }

    fn live(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.is_live())
    }

    /// Re-read pairing data, settings and clients from the service (or its
    /// files while it is stopped). Without admin rights: the status only.
    fn reload(&mut self) {
        self.backend = None;
        if !self.elevated {
            self.refresh_status();
            return;
        }
        self.points_here = install::service_points_here();
        self.load_error = None;
        match self.with_backend(|b| b.pairing()) {
            Ok(p) => {
                self.code = p.code;
                self.fingerprint = p.fingerprint;
                self.fingerprint_hex = p.fingerprint_hex;
            }
            Err(e) => self.load_error = Some(format!("读取配对码失败：{e:#}")),
        }
        if let Ok(c) = self.with_backend(|b| b.config()) {
            self.cfg = c;
        }
        self.clients = self.with_backend(|b| b.clients()).unwrap_or_default();
        self.refresh_status();
    }

    /// The service's status; non-administrators get it from the running service only.
    fn refresh_status(&mut self) {
        self.status = if self.elevated || self.svc == SvcState::Running {
            self.with_backend(|b| b.status()).ok().flatten()
        } else {
            None
        };
    }

    /// Once a second: follow the service (started / stopped / reinstalled).
    fn tick(&mut self) {
        if self.svc_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        let before = self.svc;
        self.svc = service_state();
        self.svc_checked = Instant::now();
        if self.busy.is_none() {
            // Started / stopped: switch between the pipe and the files.
            if before != self.svc || (self.svc == SvcState::Running && !self.live()) {
                self.reload();
            } else {
                self.refresh_status();
            }
        }
    }

    fn snapshot(&self) -> Value {
        json!({
            "elevated": self.elevated,
            "computer": winutil::computer_name(),
            "version": crate::version(),
            "svc": self.svc.key(),
            "live": self.live(),
            "points_here": self.points_here,
            "status": self.status.as_ref().map(status_json),
            "code": self.code,
            "fingerprint": self.fingerprint,
            "invite": self.invite(),
            "config": self.cfg,
            "encoders": ENCODERS,
            "clients": self.clients,
            "load_error": self.load_error,
            "busy": self.busy,
            "log_dir": self.dir.join("logs"),
        })
    }

    /// The pairing link and its QR code (with the code: administrators only).
    fn invite(&self) -> Value {
        if self.code.is_empty() || self.svc == SvcState::NotInstalled {
            return Value::Null;
        }
        let invite = nya_transport::invite::Invite {
            name: self.cfg.display_name(),
            addresses: invite::addresses(&self.cfg),
            fingerprint: nya_transport::Fingerprint::from_hex(&self.fingerprint_hex),
            code: self.code.clone(),
        };
        let link = invite.to_link();
        json!({ "link": link, "qr": invite::qr_svg(&link), "addresses": invite.addresses })
    }

    fn install_json(&self) -> Value {
        match &self.install_job {
            None => Value::Null,
            Some(j) => {
                let j = j.lock().unwrap();
                json!({
                    "current": j.current,
                    "status": j.status,
                    "log": j.log.iter().map(|(err, t)| json!({ "error": err, "text": t })).collect::<Vec<_>>(),
                    "reboot": j.reboot,
                    "done": j.done,
                })
            }
        }
    }
}

/// This computer as a host: the model behind the page's "本机" section.
pub struct HostPanel {
    model: Model,
    ui: Ui,
    /// Last snapshot sent to the page (to push only changes).
    sent: String,
}

impl HostPanel {
    pub fn new(ui: Ui) -> Self {
        Self { model: Model::new(), ui, sent: String::new() }
    }

    fn reply(web: Option<&WebUi>, id: u64, r: Result<Value, String>) {
        if let (Some(w), true) = (web, id != 0) {
            w.reply(id, r);
        }
    }

    /// Send the snapshot if it changed.
    pub fn push(&mut self, web: Option<&WebUi>) {
        let snap = self.model.snapshot();
        let text = snap.to_string();
        if text != self.sent {
            if let Some(w) = web {
                w.emit("host.snapshot", &snap);
            }
            self.sent = text;
        }
    }

    /// Call often; follows the service once a second.
    pub fn tick(&mut self, web: Option<&WebUi>) {
        self.model.tick();
        self.push(web);
    }

    /// Start a background job; its result answers call `id` and refreshes.
    fn job(&mut self, web: Option<&WebUi>, id: u64, label: &'static str, f: impl FnOnce() -> Result<String> + Send + 'static) {
        if let Some(b) = self.model.busy {
            return Self::reply(web, id, Err(format!("正在{b}，请稍候")));
        }
        self.model.busy = Some(label);
        self.push(web);
        let ui = self.ui.clone();
        std::thread::spawn(move || {
            let r = f().map_err(|e| format!("{e:#}"));
            ui.send(UiEvent::Host(HostEvent::JobDone(id, label, r)));
        });
    }

    /// A `host.<cmd>` call from the page (`cmd` without the prefix).
    pub fn call(&mut self, web: Option<&WebUi>, c: &Call, cmd: &str) {
        let id = c.id;
        let m = &mut self.model;
        let str_arg = |k: &str| c.args.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let r: Result<Value, String> = match cmd {
            "snapshot" => Ok(m.snapshot()),
            "open_url" => {
                let url = str_arg("url");
                if url.starts_with("https://") || url.starts_with("ms-settings:") {
                    open(url);
                }
                Ok(Value::Null)
            }
            "open_logs" => {
                open(m.dir.join("logs"));
                Ok(Value::Null)
            }
            // Turn on remote control of this computer: install and start the service.
            "enable" => {
                if m.svc != SvcState::NotInstalled {
                    Err("服务已经安装".into())
                } else if m.elevated {
                    return self.job(web, id, "开启远程控制", || install::install(None));
                } else {
                    return self.job(web, id, "开启远程控制", install_elevated);
                }
            }
            _ if !m.elevated => Err("需要管理员权限".into()),
            "svc" => {
                let action = str_arg("action");
                let (label, f): (&'static str, Box<dyn FnOnce() -> Result<String> + Send>) = match action.as_str() {
                    "install" => ("安装服务", Box::new(|| install::install(None))),
                    "uninstall" => ("卸载服务", Box::new(|| install::uninstall(false))),
                    "start" => ("启动服务", Box::new(|| control_service(true, false))),
                    "stop" => ("停止服务", Box::new(|| control_service(false, true))),
                    "restart" => ("重启服务", Box::new(|| control_service(true, true))),
                    other => return Self::reply(web, id, Err(format!("未知操作 {other}"))),
                };
                return self.job(web, id, label, f);
            }
            "diag" => {
                let out = m.dir.join("logs").join("nya-diag.txt");
                return self.job(web, id, "诊断", move || run_diag(&out));
            }
            "reset_code" => m.with_backend(|b| b.reset_pairing_code()).map_err(|e| format!("{e:#}")).map(|p| {
                m.code = p.code;
                let when = if m.live() { "已生效" } else { "服务启动后生效" };
                Value::String(format!("已生成新配对码，{when}"))
            }),
            "set_config" => (|| {
                let cfg: ServerConfig = serde_json::from_value(c.args.get("config").cloned().unwrap_or_default())
                    .map_err(|e| format!("设置格式不对：{e}"))?;
                cfg.validate().map_err(|e| format!("{e:#}"))?;
                let msg = m.with_backend(|b| b.set_config(&cfg)).map_err(|e| format!("保存失败：{e:#}"))?;
                m.cfg = cfg;
                Ok(Value::String(msg))
            })(),
            "remove_client" => {
                let fp = str_arg("fingerprint");
                let r = m.with_backend(|b| b.remove_client(&fp)).map_err(|e| format!("{e:#}"));
                m.clients = m.with_backend(|b| b.clients()).unwrap_or_default();
                r.map(Value::String)
            }
            "disconnect" => {
                let r = m.with_backend(|b| b.disconnect("被控端管理员断开了连接")).map_err(|e| format!("{e:#}"));
                m.refresh_status();
                r.map(Value::String)
            }
            "log" => {
                let name = str_arg("name");
                let name = ["service", "helper", "gui", "standalone", "vdd-test"].into_iter().find(|n| *n == name).unwrap_or("service");
                m.with_backend(|b| b.tail_log(name, 400)).map(Value::String).map_err(|e| format!("{e:#}"))
            }
            "components" => {
                let ui = self.ui.clone();
                std::thread::spawn(move || ui.send(UiEvent::Host(HostEvent::Components(id, detect_components()))));
                return;
            }
            "install" => {
                if m.install_job.as_ref().is_some_and(|j| !j.lock().unwrap().done) {
                    Err("正在安装，请稍候".into())
                } else {
                    let ids: Vec<(components::Id, &'static str)> = c
                        .args
                        .get("ids")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|v| v.as_str().and_then(component_id)).collect())
                        .unwrap_or_default();
                    if ids.is_empty() {
                        Err("没有要安装的组件".into())
                    } else {
                        m.install_job = Some(start_install(ids, self.ui.clone()));
                        Ok(m.install_json())
                    }
                }
            }
            "open_sound_settings" => {
                open("ms-settings:sound");
                Ok(Value::Null)
            }
            other => Err(format!("未知命令 host.{other}")),
        };
        Self::reply(web, id, r);
        self.push(web);
    }

    pub fn event(&mut self, web: Option<&WebUi>, ev: HostEvent) {
        match ev {
            HostEvent::JobDone(id, label, r) => {
                let m = &mut self.model;
                m.busy = None;
                m.svc_checked = Instant::now() - Duration::from_secs(10);
                if label != "诊断" {
                    // The service was (un)installed / started / stopped.
                    m.svc = service_state();
                    m.reload();
                }
                if let Some(w) = web {
                    w.emit("host.job", &json!({ "label": label, "ok": r.is_ok() }));
                }
                Self::reply(web, id, r.map(Value::String));
                self.push(web);
            }
            HostEvent::Components(id, list) => Self::reply(web, id, Ok(serde_json::to_value(list).unwrap_or_default())),
            HostEvent::InstallProgress => {
                if let Some(w) = web {
                    w.emit("host.install", &self.model.install_json());
                }
            }
        }
    }
}

/// Not elevated: `nya-server.exe install` behind a UAC prompt, waiting for it.
fn install_elevated() -> Result<String> {
    let cli = std::env::current_exe()?.with_file_name(CLI_EXE);
    if !cli.exists() {
        return Err(anyhow!("找不到 {}", cli.display()));
    }
    match nya_win::package::run_elevated(&cli, "install")? {
        0 => Ok("已开启远程控制：服务已安装并启动".into()),
        code => Err(anyhow!("安装服务失败（代码 {code}），详情见 {}", paths::service_dir().join("logs").display())),
    }
}

#[derive(Default)]
struct InstallJob {
    current: Option<&'static str>,
    status: String,
    /// (is error, message)
    log: Vec<(bool, String)>,
    reboot: bool,
    done: bool,
}

/// Install components one after another on a worker thread.
fn start_install(list: Vec<(components::Id, &'static str)>, ui: Ui) -> Arc<Mutex<InstallJob>> {
    let job = Arc::new(Mutex::new(InstallJob::default()));
    let j = job.clone();
    std::thread::spawn(move || {
        let notify = || {
            ui.send(UiEvent::Host(HostEvent::InstallProgress));
        };
        for (id, name) in list {
            {
                let mut g = j.lock().unwrap();
                g.current = Some(name);
                g.status.clear();
            }
            notify();
            let r = components::install(id, &mut |s| {
                j.lock().unwrap().status = s;
                notify();
            });
            let mut g = j.lock().unwrap();
            match r {
                Ok(i) => {
                    g.reboot |= i.reboot;
                    let mut line = format!("{name} 安装完成");
                    if i.reboot {
                        line.push_str("（需要重启）");
                    }
                    if !i.note.is_empty() {
                        line.push('。');
                        line.push_str(&i.note);
                    }
                    g.log.push((false, line));
                }
                Err(e) => g.log.push((true, format!("{name} 安装失败：{e:#}"))),
            }
            drop(g);
            notify();
        }
        let mut g = j.lock().unwrap();
        g.current = None;
        g.done = true;
        drop(g);
        notify();
    });
    job
}

/// What goes into this computer's pairing link.
mod invite {
    use std::net::{IpAddr, SocketAddr};

    use nya_server_core::config::ServerConfig;

    /// Longest address list in a link (the QR code grows with it).
    const MAX: usize = 6;

    /// The user's external addresses first (port forwarding, frp), then this
    /// computer's own IPv4 addresses (LAN, overlay networks) at the service port.
    pub fn addresses(cfg: &ServerConfig) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for a in cfg.public_address.split([',', '，', ';', ' ']).map(str::trim).filter(|a| !a.is_empty()) {
            out.push(with_port(a, cfg.port));
        }
        let ips = match cfg.bind.parse::<IpAddr>() {
            Ok(ip) if !ip.is_unspecified() => vec![ip],
            _ => local_ips(),
        };
        out.extend(ips.into_iter().map(|ip| SocketAddr::new(ip, cfg.port).to_string()));
        let mut seen = std::collections::HashSet::new();
        out.retain(|a| seen.insert(a.to_ascii_lowercase()));
        out.truncate(MAX);
        out
    }

    /// `host` → `host:port`; addresses with a port stay as they are.
    pub(super) fn with_port(a: &str, port: u16) -> String {
        if a.parse::<SocketAddr>().is_ok() {
            return a.to_owned();
        }
        if let Ok(ip) = a.trim_matches(['[', ']']).parse::<IpAddr>() {
            return SocketAddr::new(ip, port).to_string();
        }
        match a.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.parse::<u16>().is_ok() => a.to_owned(),
            _ => format!("{a}:{port}"),
        }
    }

    /// IPv4 addresses of the network adapters, likely ones first (virtual
    /// machine and WSL switches last); no loopback or self-assigned addresses.
    fn local_ips() -> Vec<IpAddr> {
        let Ok(ifs) = if_addrs::get_if_addrs() else { return Vec::new() };
        let mut v: Vec<(bool, IpAddr)> = ifs
            .into_iter()
            .filter(|i| match i.ip() {
                IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
                IpAddr::V6(_) => false,
            })
            .map(|i| {
                let n = i.name.to_ascii_lowercase();
                let virt = ["vethernet", "vmware", "virtualbox", "hyper-v", "wsl", "docker", "loopback"].iter().any(|k| n.contains(k));
                (virt, i.ip())
            })
            .collect();
        v.sort_by_key(|(virt, _)| *virt);
        v.into_iter().map(|(_, ip)| ip).collect()
    }

    /// The link as a QR code (SVG, black on white: scanners want the contrast).
    pub fn qr_svg(link: &str) -> Option<String> {
        use qrcode::render::svg;
        let code = qrcode::QrCode::with_error_correction_level(link.as_bytes(), qrcode::EcLevel::M).ok()?;
        let s = code.render::<svg::Color>().min_dimensions(200, 200).dark_color(svg::Color("#000000")).light_color(svg::Color("#ffffff")).build();
        // Inline in the page: without the XML declaration.
        Some(s[s.find("<svg")?..].to_owned())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ports_are_added_where_missing() {
            assert_eq!(with_port("frp.example.com", 47100), "frp.example.com:47100");
            assert_eq!(with_port("frp.example.com:7000", 47100), "frp.example.com:7000");
            assert_eq!(with_port("1.2.3.4", 47100), "1.2.3.4:47100");
            assert_eq!(with_port("fd00::1", 47100), "[fd00::1]:47100");
            assert_eq!(with_port("[fd00::1]:9", 47100), "[fd00::1]:9");
        }

        #[test]
        fn external_addresses_come_first() {
            let cfg = ServerConfig { public_address: "a.example.com:7000， b.example.com".into(), ..Default::default() };
            let a = addresses(&cfg);
            assert_eq!(a[..2], ["a.example.com:7000".to_owned(), format!("b.example.com:{}", cfg.port)]);
            assert!(a.len() <= MAX);
            let cfg = ServerConfig { bind: "100.64.0.2".into(), ..Default::default() };
            assert_eq!(addresses(&cfg), [format!("100.64.0.2:{}", cfg.port)]);
            assert!(qr_svg(&"x".repeat(300)).unwrap().contains("<svg"));
        }
    }
}
