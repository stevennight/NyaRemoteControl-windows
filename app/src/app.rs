//! The winit application: one window that shows the launcher or a session,
//! with the egui UI painted over the remote picture.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nya_proto::pb::{self, cursor_msg, input_msg::Ev};
use nya_transport::invite::Invite;
use nya_transport::{Fingerprint, Identity};
use nya_ui::Gui;
use nya_win::d3d::D3dDevice;
use nya_win::topology::Topology;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::platform::windows::MonitorHandleExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorGrabMode, CursorIcon, CustomCursor, Fullscreen, Window, WindowId};

use crate::config::{ClientConfig, Defaults};
use crate::events::{ConnectDone, Hotkey, Ui, UiEvent};
use crate::net::{self, Link, PairPrompt, Params};
use crate::render::{fit, Renderer};
use crate::session::{DisplayChoice, Session, SessionOptions};
use crate::ui::{self, Action};

mod conn;
mod extra;
pub mod host;
mod update;
mod launcher;
use launcher::{Kind, Phase};
use crate::{caps, input};

/// A connection attempt in progress.
struct Pending {
    address: String,
    label: Option<String>,
    /// Name given with the address (command line), for a host not saved yet.
    name: Option<String>,
    /// Command-line settings for this connection only.
    overrides: Option<crate::config::Overrides>,
    reverify: bool,
    /// Connecting through a pairing link: its addresses, certificate and code.
    invite: Option<Invite>,
}

pub struct App {
    rt: tokio::runtime::Handle,
    ui_tx: Ui,
    data_dir: PathBuf,
    cfg: ClientConfig,
    identity: Identity,
    auto_connect: Option<(String, Option<String>, crate::config::Overrides)>,
    updates: update::Updates,
    /// This computer as a host (the page's "本机" section).
    host: host::HostPanel,
    /// Page the launcher opens on (`--page`), handed out once.
    start_page: Option<String>,
    /// A `nyaremote://` link the program was started with (asked about once the page is up).
    start_link: Option<String>,
    /// An opened pairing link waiting for the user's yes (`Phase::Invite`).
    offered: Option<Invite>,
    /// The current connection (see `conn`) and the parked others.
    conn_id: u64,
    next_conn: u64,
    others: Vec<conn::Conn>,
    hook_installed: bool,
    /// Settings of the running session (the host's, with command-line
    /// overrides), and the host's address to save changes under.
    sd: Defaults,
    session_host: String,

    /// The session window (remote picture); hidden while no session runs.
    window: Option<Arc<Window>>,
    /// The launcher window (devices, settings), a web page.
    launcher: Option<Arc<Window>>,
    /// The tray icon (closing the launcher keeps the program there).
    tray: Option<crate::tray::Tray>,
    /// Started with Windows (`--tray`): the launcher stays hidden until asked for.
    start_in_tray: bool,
    /// The launcher is created hidden and shown once its page is up (no
    /// blank window while WebView2 starts), at the latest at this time.
    launcher_reveal: Option<Instant>,
    renderer: Option<Renderer>,
    gui: Option<Gui>,
    adapter_luid: u64,

    /// The launcher page (hidden during a session).
    web: Option<nya_webui::WebUi>,
    phase: Phase,
    /// Hardware decoding of this computer, for the launcher.
    decode_summary: String,
    session: Option<Session>,
    pending: Option<Pending>,
    attempt: u64,
    connect_task: Option<tokio::task::JoinHandle<()>>,
    pair_reply: Option<std::sync::mpsc::Sender<Option<String>>>,
    verify_link: Option<Box<Link>>,

    focused: bool,
    fullscreen: bool,
    toolbar_open: bool,
    mods: winit::keyboard::ModifiersState,
    cursor_over_ui: bool,
    /// Buttons pressed on the remote side (their releases must follow).
    remote_buttons: u8,
    repaint_at: Option<Instant>,
    exit: bool,
    hovering_file: bool,
    /// Files dropped on the window, sent together once the drop is complete.
    dropped: Vec<PathBuf>,
    /// Window resized: resize the host's virtual display at this time.
    vd_resize_at: Option<Instant>,
    /// Extra windows (other host displays at the same time).
    extras: std::collections::HashMap<WindowId, extra::ExtraWindow>,
    /// Displays to open in a new window (needs the event loop: about_to_wait).
    open_requests: Vec<u32>,
    auto_opened: extra::AutoOpened,
    /// Host displays or the main stream changed: check the extra windows.
    sync_extras: bool,
    /// Virtual screen (1-based) created for a new window, until it appears.
    pending_virtual: Option<u32>,
    /// Last real size of each window (see `guard_size`).
    normal_sizes: std::collections::HashMap<WindowId, LogicalSize<f64>>,
}

/// The program's icon (resource 1, from common/assets/client.ico) for the
/// title bar and the taskbar of our windows.
pub(crate) fn app_icon() -> Option<winit::window::Icon> {
    use winit::platform::windows::IconExtWindows;
    winit::window::Icon::from_resource(1, None).ok()
}

fn hwnd(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Device on the GPU that drives the monitor the window is on.
fn device_for_window(window: &Window) -> anyhow::Result<D3dDevice> {
    if let (Some(m), Ok(topo)) = (window.current_monitor(), Topology::enumerate()) {
        if let Some(a) = topo.adapter_for_monitor(m.hmonitor()) {
            if let Ok(d) = D3dDevice::for_adapter(&a.adapter) {
                return Ok(d);
            }
        }
    }
    D3dDevice::default_adapter()
}

fn parse_codec(s: &str) -> pb::Codec {
    match s.to_ascii_lowercase().as_str() {
        "h264" | "avc" => pb::Codec::H264,
        "hevc" | "h265" => pb::Codec::Hevc,
        "av1" => pb::Codec::Av1,
        _ => pb::Codec::Unspecified,
    }
}

fn parse_chroma(s: &str) -> pb::Chroma {
    match s {
        "420" => pb::Chroma::Yuv420,
        "444" => pb::Chroma::Yuv444,
        _ => pb::Chroma::Unspecified,
    }
}

/// "不限制": the top of the encoder's range; static office content still only
/// uses what it needs.
const UNLIMITED_KBPS: u32 = 80_000;

/// Virtual display sizes: width a multiple of 8, height even, at least 640x480.
/// The host's note about trouble with the capture (`ServerStats.capture_note`)
/// as the status line while it lasts; cleared when the host stops sending it,
/// unless another status replaced it meanwhile.
fn show_capture_note(shown: &mut String, status: &mut String, note: &str) {
    if note == shown.as_str() {
        return;
    }
    if !note.is_empty() {
        *status = note.to_string();
    } else if status == shown {
        status.clear();
    }
    *shown = note.to_string();
}

fn vd_dims(w: u32, h: u32) -> (u32, u32) {
    ((w & !7).max(640), (h & !1).max(480))
}

/// `hdr`: this window's monitor shows HDR (and the user allows HDR10).
fn start_request(d: &Defaults, setup: Option<pb::DisplaySetup>, hdr: bool) -> pb::StartStream {
    let game = d.mode.eq_ignore_ascii_case("game");
    pb::StartStream {
        // The first virtual screen is the host's primary display.
        display_id: if setup.as_ref().is_some_and(|s| !s.virtual_screens.is_empty()) { 0 } else { d.display },
        display_setup: setup,
        slot: 0,
        config: Some(pb::StreamConfig {
            codec: parse_codec(&d.codec) as i32,
            chroma: parse_chroma(&d.chroma) as i32,
            width: 0,
            height: 0,
            fps: 0,
            bitrate_kbps: if d.unlimited_bitrate { UNLIMITED_KBPS } else { d.bitrate_kbps },
            mode: if game { pb::StreamMode::Game } else { pb::StreamMode::Office } as i32,
            bitrate_policy: crate::ui::parse_policy(&d.bitrate_policy) as i32,
            video_transport: match d.video_transport.as_str() {
                "stream" => pb::VideoTransport::Stream,
                "datagram" => pb::VideoTransport::Datagram,
                _ => pb::VideoTransport::Auto,
            } as i32,
            hdr,
        }),
        encoder_preference: d.encoder.clone(),
    }
}

impl App {
    pub fn new(
        rt: tokio::runtime::Handle,
        ui_tx: Ui,
        data_dir: PathBuf,
        cfg: ClientConfig,
        identity: Identity,
        auto_connect: Option<(String, Option<String>, crate::config::Overrides)>,
        start_page: Option<String>,
        start_in_tray: bool,
        start_link: Option<String>,
    ) -> Self {
        Self {
            start_link,
            offered: None,
            start_in_tray,
            host: host::HostPanel::new(ui_tx.clone()),
            start_page,
            rt,
            ui_tx,
            data_dir,
            cfg,
            identity,
            auto_connect,
            updates: update::Updates::new(),
            conn_id: 0,
            next_conn: 0,
            others: Vec::new(),
            hook_installed: false,
            sd: Defaults::default(),
            session_host: String::new(),
            window: None,
            launcher: None,
            launcher_reveal: None,
            tray: None,
            renderer: None,
            gui: None,
            adapter_luid: 0,
            web: None,
            phase: Phase::Idle,
            // Filled in by a worker thread: the check uses multithreaded COM,
            // which must stay off the UI thread (winit needs OLE there).
            decode_summary: "正在检测硬件解码…".into(),
            session: None,
            pending: None,
            attempt: 0,
            connect_task: None,
            pair_reply: None,
            verify_link: None,
            focused: false,
            fullscreen: false,
            toolbar_open: false,
            mods: Default::default(),
            cursor_over_ui: false,
            remote_buttons: 0,
            repaint_at: None,
            exit: false,
            hovering_file: false,
            dropped: Vec::new(),
            vd_resize_at: None,
            extras: Default::default(),
            open_requests: Vec::new(),
            auto_opened: Default::default(),
            sync_extras: false,
            pending_virtual: None,
            normal_sizes: Default::default(),
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    // ------------------------------------------------------------------ devices

    fn create_renderer(&mut self, dev: D3dDevice) -> anyhow::Result<()> {
        let window = self.window.clone().unwrap();
        let h = hwnd(&window).ok_or_else(|| anyhow::anyhow!("no HWND"))?;
        let size = window.inner_size();
        self.adapter_luid = dev.luid;
        match self.gui.as_mut() {
            Some(g) => g.set_device(&dev)?,
            None => self.gui = Some(Gui::new(&window, &dev)?),
        }
        self.renderer = Some(Renderer::new(dev, h, size.width, size.height)?);
        Ok(())
    }

    fn recreate_device(&mut self) {
        self.renderer = None;
        let Some(w) = self.window.clone() else { return };
        match device_for_window(&w) {
            Ok(dev) => {
                if let Some(s) = &mut self.session {
                    s.current = None;
                    s.set_device(&dev);
                }
                if let Err(e) = self.create_renderer(dev) {
                    tracing::error!("renderer: {e:#}");
                }
                self.rebuild_extras();
            }
            Err(e) => tracing::error!("D3D device: {e:#}"),
        }
    }

    /// Display setup for the host from these choices, the size settings and
    /// this window. `None` = the host's displays as they are.
    fn setup_request(&self, c: DisplayChoice, fullscreen: bool) -> Option<pb::DisplaySetup> {
        let count = c.count.min(4);
        if count == 0 && !c.block_input {
            return None;
        }
        let main = self.window.as_ref()?;
        let follow = !matches!(self.sd.vd_size.as_str(), "fixed" | "screen");
        let displays = self.session.as_ref().and_then(|s| s.info.as_ref()).map(|i| i.displays.clone()).unwrap_or_default();
        let mut screens = Vec::new();
        for i in 1..=count {
            let shown = displays.iter().find(|d| d.virtual_index == i);
            // "Follow the window": each virtual screen takes the size of the
            // window that shows it; a screen no window shows keeps its size.
            let screen = if follow {
                match shown.map(|d| (d, self.window_showing(d.id))) {
                    Some((_, Some((w, fs)))) => self.virtual_screen(&w, fs),
                    Some((d, None)) => Some(pb::VirtualScreen {
                        width: d.width,
                        height: d.height,
                        refresh_hz: d.refresh_hz,
                        scale_percent: self.virtual_screen(main, fullscreen).map(|s| s.scale_percent).unwrap_or(0),
                    }),
                    None => self.virtual_screen(main, fullscreen),
                }
            } else {
                self.virtual_screen(main, fullscreen)
            };
            // A minimized window has no size: keep what the host has.
            let screen = screen.or_else(|| shown.map(|d| pb::VirtualScreen { width: d.width, height: d.height, refresh_hz: d.refresh_hz, scale_percent: 0 }));
            screens.push(screen.unwrap_or(pb::VirtualScreen { width: 1920, height: 1080, refresh_hz: 60, scale_percent: 0 }));
        }
        Some(pb::DisplaySetup {
            virtual_screens: screens,
            physical_off: c.physical_off && count > 0,
            block_local_input: c.block_input,
        })
    }

    /// The window currently showing host display `id` (main or extra) and
    /// whether it is fullscreen.
    fn window_showing(&self, id: u32) -> Option<(Arc<Window>, bool)> {
        let s = self.session.as_ref()?;
        if s.stream.as_ref().is_some_and(|st| st.display_id == id) {
            return self.window.clone().map(|w| (w, self.fullscreen));
        }
        let slot = s.view_of(id)?;
        self.extras.values().find(|w| w.slot == slot).map(|w| (w.window.clone(), w.fullscreen))
    }

    /// Keeps a window from shrinking to nothing. winit sizes a window on a
    /// DPI change from its client area, which is 0×0 while it is minimized
    /// (RDP reconnects, display changes): the window would come back as a
    /// 16×39 sliver. The window's last real size is used instead, and a window
    /// that is restored that small anyway gets it back.
    fn guard_size(&mut self, w: &Window, event: &mut WindowEvent) {
        const TINY: f64 = 100.0;
        let fallback = LogicalSize::new(1100.0, 760.0);
        let minimized = w.is_minimized() == Some(true);
        match event {
            WindowEvent::Resized(size) if !minimized => {
                let logical = size.to_logical::<f64>(w.scale_factor());
                if logical.width >= TINY && logical.height >= TINY {
                    self.normal_sizes.insert(w.id(), logical);
                } else if w.fullscreen().is_none() {
                    let back = self.normal_sizes.get(&w.id()).copied().unwrap_or(fallback);
                    tracing::warn!("window {:?} restored at {}x{}, back to {:.0}x{:.0}", w.id(), size.width, size.height, back.width, back.height);
                    let _ = w.request_inner_size(back);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, inner_size_writer } => {
                let inner = w.inner_size().to_logical::<f64>(w.scale_factor());
                if minimized || inner.width < TINY || inner.height < TINY {
                    let keep = self.normal_sizes.get(&w.id()).copied().unwrap_or(fallback);
                    tracing::info!("scale {scale_factor} while minimized: window {:?} keeps {:.0}x{:.0}", w.id(), keep.width, keep.height);
                    let _ = inner_size_writer.request_inner_size(keep.to_physical(*scale_factor));
                }
            }
            _ => {}
        }
    }

    /// A virtual screen sized for `w` (following the settings); `None` while
    /// the window is minimized.
    fn virtual_screen(&self, w: &Window, fullscreen: bool) -> Option<pb::VirtualScreen> {
        let d = &self.sd;
        if w.is_minimized() == Some(true) || w.inner_size().width == 0 {
            return None;
        }
        let monitor = w.current_monitor();
        let monitor_size = monitor.as_ref().map(|m| (m.size().width, m.size().height)).unwrap_or((1920, 1080));
        let (width, height) = match d.vd_size.as_str() {
            "fixed" => (d.vd_width, d.vd_height),
            "screen" => monitor_size,
            // Follow the window; fullscreen means the whole monitor.
            _ if fullscreen => monitor_size,
            _ => (w.inner_size().width, w.inner_size().height),
        };
        let (width, height) = vd_dims(width, height);
        let refresh_hz = monitor.and_then(|m| m.refresh_rate_millihertz()).map(|mhz| (mhz + 500) / 1000).unwrap_or(60);
        Some(pb::VirtualScreen { width, height, refresh_hz, scale_percent: if d.vd_scale { (w.scale_factor() * 100.0).round() as u32 } else { 0 } })
    }

    /// The window size settled: fit the virtual screens to it.
    fn fit_virtual_display(&mut self) {
        let Some(s) = self.session.as_ref() else { return };
        let choice = s.display_choice();
        if !s.vd_follow_window || choice.count == 0 {
            return;
        }
        let setup = self.setup_request(choice, self.fullscreen);
        if let Some(s) = self.session.as_mut() {
            s.set_display_setup(setup);
        }
    }

    // --------------------------------------------------------------- connecting

    fn connect(&mut self, target: String, name: Option<String>, overrides: Option<crate::config::Overrides>) {
        if let Some(invite) = Invite::parse(&target) {
            return self.connect_invite(invite, name, overrides);
        }
        let entry = self.cfg.find(&target).cloned();
        let address = entry.as_ref().map(|e| e.address.clone()).unwrap_or(target);
        let name = name.filter(|n| !n.trim().is_empty() && entry.is_none());
        let label = entry.as_ref().map(|e| e.name.clone()).or_else(|| name.clone());
        self.start_connect(Pending { address, label, name, overrides, reverify: false, invite: None });
    }

    /// Connect through a pairing link: all its addresses at once (a saved
    /// host with the same certificate: its address first), pairing with the
    /// link's code.
    fn connect_invite(&mut self, mut invite: Invite, name: Option<String>, overrides: Option<crate::config::Overrides>) {
        let fp = invite.fingerprint.map(|f| f.to_hex());
        let saved = fp.and_then(|fp| self.cfg.hosts.iter().find(|h| h.fingerprint == fp)).cloned();
        if let Some(h) = &saved {
            invite.addresses.retain(|a| *a != h.address);
            invite.addresses.insert(0, h.address.clone());
        }
        let name = name.filter(|n| !n.trim().is_empty()).or_else(|| Some(invite.name.clone()).filter(|n| !n.is_empty()));
        let label = saved.as_ref().map(|h| h.name.clone()).or_else(|| name.clone());
        let address = invite.addresses[0].clone();
        tracing::info!("pairing link: {} address(es), {}", invite.addresses.len(), if invite.fingerprint.is_some() { "pinned" } else { "no fingerprint" });
        self.start_connect(Pending { address, label, name, overrides, reverify: false, invite: Some(invite) });
    }

    /// A `nyaremote://` link was opened: ask before connecting (a link from
    /// someone else would send them this computer's keyboard and clipboard).
    fn offer_invite(&mut self, link: &str) {
        self.bring_launcher_back();
        let Some(invite) = Invite::parse(link) else {
            self.notice(Kind::Error, "配对链接无效或不完整（复制时少了一部分？）");
            return;
        };
        if self.pending.is_some() {
            self.notice(Kind::Error, "正在连接其他设备，请稍后再打开链接");
            return;
        }
        let label = if invite.name.is_empty() { invite.addresses[0].clone() } else { invite.name.clone() };
        tracing::info!("pairing link opened: {label} ({})", invite.addresses.join(", "));
        self.set_phase(Phase::Invite(label, invite.addresses.clone()));
        self.offered = Some(invite);
    }

    /// This computer's name as hosts show it.
    pub(super) fn client_name(&self) -> String {
        let n = self.cfg.client_name.trim();
        if n.is_empty() {
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "NyaRemoteControl".into())
        } else {
            n.to_owned()
        }
    }

    /// A setting changed during the session: keep it for this host.
    fn remember(&mut self, f: impl Fn(&mut Defaults)) {
        f(&mut self.sd);
        if self.cfg.edit_settings(&self.session_host, f) {
            if let Err(e) = self.cfg.save(&self.data_dir) {
                tracing::warn!("save config: {e:#}");
            }
            self.push_state();
        }
    }

    fn start_connect(&mut self, p: Pending) {
        if let Some(invite) = p.invite.clone() {
            return self.start_connect_invite(p, invite);
        }
        let pinned = if p.reverify {
            None
        } else {
            self.cfg
                .hosts
                .iter()
                .find(|h| h.address == p.address)
                .and_then(|h| Fingerprint::from_hex(&h.fingerprint))
        };
        self.attempt += 1;
        let attempt = self.attempt;
        let (id, name, ui, address) = (self.identity.clone(), self.client_name(), self.ui_tx.clone(), p.address.clone());
        let transport = net::Transport::parse(&self.cfg.settings_for(&p.address).transport);
        let ui2 = ui.clone();
        let prompt: PairPrompt = Arc::new(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            ui2.send(UiEvent::NeedPairing(tx));
            rx.recv().ok().flatten()
        });
        let task = self.rt.spawn(async move {
            let res = async {
                let addr = nya_transport::endpoint::resolve(&address, nya_proto::DEFAULT_PORT)?;
                net::connect(addr, &id, pinned, &name, Some(prompt), transport, false).await
            }
            .await;
            let (result, pin_mismatch) = match res {
                Ok(l) => (Ok(Box::new(l)), false),
                Err(e) => {
                    let m = format!("{e:#}");
                    let pm = pinned.is_some() && m.contains(nya_transport::tls::PIN_MISMATCH);
                    (Err(m), pm)
                }
            };
            ui.send(UiEvent::ConnectDone(ConnectDone { attempt, result, pin_mismatch, address: None }));
        });
        self.set_phase(Phase::Connecting(Self::pending_label(&p)));
        self.pending = Some(p);
        self.connect_task = Some(task);
        self.request_redraw();
    }

    fn start_connect_invite(&mut self, p: Pending, invite: Invite) {
        self.attempt += 1;
        let attempt = self.attempt;
        let (id, name, ui) = (self.identity.clone(), self.client_name(), self.ui_tx.clone());
        let transport = net::Transport::parse(&self.cfg.settings_for(&p.address).transport);
        let code = invite.code.clone();
        let prompt: PairPrompt = Arc::new(move || Some(code.clone()));
        let task = self.rt.spawn(async move {
            let res = net::connect_any(&invite.addresses, &id, invite.fingerprint, &name, Some(prompt), transport).await;
            let (result, address) = match res {
                Ok((a, l)) => (Ok(Box::new(l)), Some(a)),
                Err(e) => (Err(format!("{e:#}")), None),
            };
            ui.send(UiEvent::ConnectDone(ConnectDone { attempt, result, pin_mismatch: false, address }));
        });
        self.set_phase(Phase::Connecting(Self::pending_label(&p)));
        self.pending = Some(p);
        self.connect_task = Some(task);
        self.request_redraw();
    }

    fn cancel_connect(&mut self) {
        if let Some(t) = self.connect_task.take() {
            t.abort();
        }
        if let Some(tx) = self.pair_reply.take() {
            let _ = tx.send(None);
        }
        self.pending = None;
        self.verify_link = None;
        self.set_phase(Phase::Idle);
    }

    fn on_connect_done(&mut self, done: ConnectDone) {
        if done.attempt != self.attempt || self.pending.is_none() {
            return; // cancelled
        }
        self.connect_task = None;
        if let (Some(a), Some(p)) = (done.address, self.pending.as_mut()) {
            p.address = a;
        }
        let label = self.pending.as_ref().map(Self::pending_label).unwrap_or_default();
        match done.result {
            Ok(link) => {
                let reverify = self.pending.as_ref().is_some_and(|p| p.reverify);
                if reverify && !link.welcome.needs_pairing {
                    self.set_phase(Phase::Verify(label, link.server_fp.to_string()));
                    self.verify_link = Some(link);
                } else {
                    self.set_phase(Phase::Idle);
                    self.finish_connect(link);
                }
            }
            Err(msg) if done.pin_mismatch => {
                tracing::warn!("{msg}");
                self.set_phase(Phase::PinChanged(label));
            }
            Err(msg) => {
                self.pending = None;
                self.set_phase(Phase::Idle);
                self.notice(Kind::Error, format!("无法连接 {label}：{msg}"));
            }
        }
    }

    fn finish_connect(&mut self, link: Box<Link>) {
        let Some(p) = self.pending.take() else { return };
        if !self.activate_idle() {
            self.notice(Kind::Error, "无法打开远程窗口");
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let new_host = !self.cfg.hosts.iter().any(|h| h.address == p.address);
        let mut label = self.cfg.connected(&p.address, &link.welcome.server_name, link.server_fp.to_hex(), now);
        if let (true, Some(name)) = (new_host, &p.name) {
            if let Some(i) = self.cfg.hosts.iter().position(|h| h.address == p.address) {
                if self.cfg.rename(i, name).is_ok() {
                    label = self.cfg.hosts[i].name.clone();
                }
            }
        }
        if let Err(e) = self.cfg.save(&self.data_dir) {
            tracing::warn!("save config: {e:#}");
        }
        self.push_state();
        let Some(dev) = self.renderer.as_ref().map(|r| r.dev.clone()) else { return };
        let mut d = self.cfg.settings_for(&p.address);
        if let Some(o) = &p.overrides {
            o.apply(&mut d);
        }
        self.sd = d.clone();
        self.session_host = p.address.clone();
        let monitor_fps = self
            .window
            .as_ref()
            .and_then(|w| w.current_monitor())
            .and_then(|m| m.refresh_rate_millihertz())
            .map(|mhz| mhz.div_ceil(1000))
            .unwrap_or(60);
        let caps = caps::detect(&dev, d.hw_decode, if d.max_fps > 0 { d.max_fps } else { monitor_fps });
        tracing::info!("decoders: {:?}", caps.decoders.iter().map(|c| (c.codec, c.chroma, c.hardware)).collect::<Vec<_>>());
        let vd_supported = link.neg.has(pb::Feature::VirtualDisplay);
        let choice = DisplayChoice { count: d.vd_count, physical_off: d.physical_off, block_input: d.block_input };
        let vd = if vd_supported { self.setup_request(choice, d.fullscreen) } else { None };
        let params = Params {
            addr: link.conn.remote_address(),
            pinned: link.server_fp,
            identity: self.identity.clone(),
            name: self.client_name(),
            caps,
            start: start_request(&d, vd, d.hdr && self.renderer.as_ref().is_some_and(|r| r.display_hdr())),
            extra: Default::default(),
            shares: std::sync::Arc::new(d.shares()),
            transport: net::Transport::parse(&d.transport),
            prefer_tcp_until: None,
            tcp_hold: net::FIRST_TCP_HOLD,
        };
        let opts = SessionOptions { hw_decode: d.hw_decode, audio: d.audio, clipboard: d.clipboard };
        let mut session = Session::start(&self.rt, *link, params, &dev, &opts, self.ui_tx.for_conn(self.conn_id), label);
        session.vd_supported = vd_supported;
        session.vd_follow_window = d.vd_size == "window";
        self.session = Some(session);
        self.vd_resize_at = None;
        self.toolbar_open = false;
        self.show_session_window(true);
        if d.fullscreen {
            self.set_fullscreen(true);
        }
        if let Some(s) = &mut self.session {
            s.mic_auto = d.mic;
            s.transport = net::Transport::parse(&d.transport);
        }
        self.set_grab(d.grab_keyboard);
        self.update_title();
    }

    fn end_session(&mut self, message: Option<(Kind, String)>) {
        self.close_all_extras();
        let Some(s) = self.session.take() else { return };
        s.quit();
        drop(s);
        self.set_fullscreen(false);
        if let Some(w) = &self.window {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor(CursorIcon::Default);
            w.set_cursor_visible(true);
        }
        self.remote_buttons = 0;
        self.show_session_window(false);
        if let Some((kind, text)) = message {
            self.notice(kind, text);
        }
        self.update_title();
        self.request_redraw();
    }

    // ------------------------------------------------------------------ session

    fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        if let Some(w) = &self.window {
            w.set_fullscreen(on.then(|| Fullscreen::Borderless(None)));
        }
    }

    fn set_relative(&mut self, on: bool) {
        let Some(s) = self.session.as_mut() else { return };
        s.relative = on;
        if let Some(w) = &self.window {
            if on {
                let _ = w.set_cursor_grab(CursorGrabMode::Confined).or_else(|_| w.set_cursor_grab(CursorGrabMode::Locked));
                w.set_cursor_visible(false);
            } else {
                let _ = w.set_cursor_grab(CursorGrabMode::None);
                w.set_cursor_visible(s.cursor_visible);
            }
        }
    }

    fn update_title(&self) {
        let Some(w) = &self.window else { return };
        let t = match &self.session {
            None => "NyaRemoteControl".to_string(),
            Some(s) => {
                let mut t = format!("{} — NyaRemoteControl", s.label);
                if s.via_tcp == Some(true) {
                    t += " · TCP";
                }
                if let Some(st) = &s.stream {
                    let c = st.config.clone().unwrap_or_default();
                    t += &format!(" — {}x{}@{} {} {} kbps", c.width, c.height, s.summary.fps, st.encoder_name, s.summary.kbps);
                }
                if !input::grabbed() {
                    t += " [键盘未捕获]";
                }
                t
            }
        };
        w.set_title(&t);
    }

    fn hotkey(&mut self, h: Hotkey) {
        tracing::info!("hotkey {h:?}");
        if self.session.is_none() {
            return;
        }
        match h {
            Hotkey::ToggleGrab => {
                let on = !input::grabbed();
                self.set_grab(on);
                self.remember(|d| d.grab_keyboard = on);
            }
            Hotkey::ToggleStats => {
                if let Some(s) = &mut self.session {
                    s.show_stats = !s.show_stats;
                }
            }
            Hotkey::ToggleMode => {
                if let Some(s) = &mut self.session {
                    let g = !s.game;
                    s.set_game_mode(g);
                    self.remember(|d| d.mode = if g { "game" } else { "office" }.into());
                }
            }
            Hotkey::ToggleRelative => {
                let on = !self.session.as_ref().is_some_and(|s| s.relative);
                self.set_relative(on);
            }
            Hotkey::ToggleFullscreen => self.set_fullscreen(!self.fullscreen),
            Hotkey::CtrlAltDel => {
                if let Some(s) = &self.session {
                    s.ctrl_alt_del();
                }
            }
            Hotkey::ToggleToolbar => self.toolbar_open = !self.toolbar_open,
            Hotkey::Display(n) => {
                if let Some(s) = &mut self.session {
                    s.select_display_index(n);
                }
            }
            Hotkey::Quit => self.end_session(Some((Kind::Info, "已断开连接".into()))),
        }
        self.update_title();
        self.request_redraw();
    }

    fn set_grab(&mut self, on: bool) {
        tracing::info!("keyboard capture {}", if on { "on" } else { "off" });
        input::set_grab(on);
        if !on {
            if let Some(s) = &self.session {
                s.release_all();
            }
        }
        self.update_title();
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Hotkey(h) => self.hotkey(h),
                Action::SetGameMode(g) => {
                    if let Some(s) = &mut self.session {
                        s.set_game_mode(g);
                    }
                    self.remember(|d| d.mode = if g { "game" } else { "office" }.into());
                }
                Action::OpenWindow(display_id) => self.open_requests.push(display_id),
                Action::NewVirtualWindow => self.new_virtual_window(),
                Action::SetDisplayChoice(choice) => {
                    let setup = self.setup_request(choice, self.fullscreen);
                    if let Some(s) = &mut self.session {
                        s.set_display_setup(setup);
                    }
                    self.remember(|d| {
                        d.vd_count = choice.count;
                        d.physical_off = choice.physical_off;
                        d.block_input = choice.block_input;
                    });
                }
                Action::SelectDisplay(id) => {
                    if let Some(s) = &mut self.session {
                        s.select_display(id);
                    }
                }
                Action::SetGrab(on) => {
                    self.set_grab(on);
                    self.remember(|d| d.grab_keyboard = on);
                }
                Action::ToggleUsb => {
                    let open = self.session.as_ref().is_some_and(|s| !s.usb_open);
                    if let Some(s) = &mut self.session {
                        s.usb_open = open;
                    }
                    if open {
                        self.refresh_usb();
                    }
                }
                Action::RefreshUsb => self.refresh_usb(),
                Action::InstallUsbipd => {
                    if let Some(s) = &mut self.session {
                        s.usbipd_install = Some((true, "准备安装包…".into()));
                    }
                    let ui = self.ui_tx.for_conn(self.conn_id);
                    std::thread::spawn(move || {
                        let r = crate::usb::install_usbipd(&mut |m| ui.send(UiEvent::UsbipdInstall(true, m)));
                        ui.send(UiEvent::UsbipdInstall(
                            false,
                            match r {
                                Ok(()) => "安装完成".into(),
                                Err(e) => format!("安装失败：{e:#}"),
                            },
                        ));
                    });
                }
                Action::UsbAttach { busid, description, bound } => {
                    if let Some(s) = &mut self.session {
                        s.usb_busy.insert(busid.clone());
                        let (net, ui) = (s.net_tx.clone(), self.ui_tx.for_conn(self.conn_id));
                        std::thread::spawn(move || {
                            // Share it with usbipd first (UAC prompt), then ask the host to attach.
                            let shared = if bound { Ok(()) } else { crate::usb::bind(&busid) };
                            match shared {
                                Ok(()) => {
                                    let m = pb::ControlMsg {
                                        msg: Some(pb::control_msg::Msg::UsbAttach(pb::UsbAttach { busid, description })),
                                    };
                                    let _ = net.send(crate::events::NetCmd::Control(m));
                                }
                                Err(e) => ui.send(UiEvent::UsbStatus(pb::UsbStatus {
                                    busid,
                                    attached: false,
                                    message: format!("{e:#}"),
                                })),
                            }
                        });
                    }
                }
                Action::UsbDetach(busid) => {
                    if let Some(s) = &mut self.session {
                        s.usb_detach(&busid);
                    }
                }
                Action::TakeControl(kick) => {
                    if let Some(s) = &mut self.session {
                        s.take_control(kick);
                    }
                }
                Action::SetMic(on) => {
                    if let Some(s) = &mut self.session {
                        s.set_mic(on);
                    }
                    self.remember(|d| d.mic = on);
                }
                Action::SetPolicy(p) => {
                    if let Some(s) = &mut self.session {
                        s.set_bitrate_policy(p);
                    }
                    self.remember(|d| d.bitrate_policy = crate::ui::policy_key(p).into());
                }
                Action::SetTransport(t) => {
                    if let Some(s) = &mut self.session {
                        s.transport = t;
                        let _ = s.net_tx.send(crate::events::NetCmd::SetTransport(t));
                    }
                    self.remember(|d| d.transport = t.key().into());
                }
                Action::Disconnect => self.end_session(Some((Kind::Info, "已断开连接".into()))),
                Action::PickFiles => {
                    if let Some(paths) = rfd::FileDialog::new().set_title("选择要发送到被控端的文件").pick_files() {
                        if let Some(s) = &mut self.session {
                            s.send_files(paths);
                        }
                    }
                }
                Action::AcceptOffer(id) => {
                    if let Some(s) = &mut self.session {
                        s.accept_offer(id);
                    }
                }
                Action::DismissOffer(id) => {
                    if let Some(s) = &mut self.session {
                        s.dismiss_offer(id);
                    }
                }
                Action::DismissTransfer(id) => {
                    if let Some(s) = &mut self.session {
                        s.dismiss_transfer(id);
                    }
                }
                Action::CancelTransfer(id) => {
                    if let Some(s) = &mut self.session {
                        s.cancel_transfer(id);
                    }
                }
                Action::OpenFolder(p) => {
                    let _ = std::process::Command::new("explorer").arg(p).spawn();
                }
            }
        }
    }

    // ------------------------------------------------------------------ drawing

    fn draw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        if self.session.is_none() {
            return; // the launcher page covers the window
        }
        let Some(mut gui) = self.gui.take() else { return };
        let mut actions = Vec::new();
        let (toolbar_open, fullscreen, hovering_file) = (self.toolbar_open, self.fullscreen, self.hovering_file);
        let session = &mut self.session;
        let frame = gui.run(&window, |ctx| {
            if let Some(s) = session.as_mut() {
                ui::session_overlay(ctx, s, toolbar_open, fullscreen, hovering_file, &mut actions)
            }
        });
        let over_ui = gui.ctx.is_pointer_over_area() || gui.ctx.is_using_pointer();
        self.sync_cursor_after_ui(over_ui, frame.cursor_set);

        let mut fresh = false;
        if let Some(s) = &mut self.session {
            let (slot, f) = s.store.take();
            if slot.is_some() {
                s.current = slot;
            }
            fresh = f;
        }
        let game = self.session.as_ref().is_some_and(|s| s.game);
        let t = Instant::now();
        let mut failed = false;
        if let Some(r) = self.renderer.as_mut() {
            let res = (|| -> anyhow::Result<()> {
                let rtv = r.begin()?;
                if let Some(slot) = self.session.as_ref().and_then(|s| s.current.clone()) {
                    r.draw_video(&slot);
                }
                gui.paint(&rtv, (r.width, r.height), &frame)?;
                r.present(game)
            })();
            if let Err(e) = res {
                tracing::warn!("render failed ({e:#}); recreating device");
                failed = true;
            }
        }
        self.gui = Some(gui);
        if failed {
            self.recreate_device();
        }

        if fresh {
            if let Some(s) = &self.session {
                let render_ms = t.elapsed().as_secs_f32() * 1000.0;
                let lat = s.current.as_ref().map(|c| s.stats.latency_ms(c.capture_ts));
                s.stats.with(|st| {
                    if st.total_rendered == 0 {
                        tracing::info!("first frame rendered ({render_ms:.1} ms)");
                    }
                    st.total_rendered += 1;
                    st.render_ms.push(render_ms);
                    st.frames_rendered += 1;
                    if let Some(l) = lat {
                        st.latency_ms.push(l);
                    }
                });
            }
        }
        self.repaint_at = (frame.repaint_after < Duration::from_secs(1)).then(|| Instant::now() + frame.repaint_after);
        if !actions.is_empty() {
            self.apply(actions);
            self.request_redraw();
        }
    }

    fn refresh_usb(&mut self) {
        if let Some(s) = &mut self.session {
            s.usb_devices = None;
        }
        let ui = self.ui_tx.for_conn(self.conn_id);
        // Looking for usbipd and listing devices start processes: never on the UI thread.
        std::thread::spawn(move || {
            let present = crate::usb::usbipd_exe().is_some();
            let list = if present { crate::usb::list().map_err(|e| format!("{e:#}")) } else { Ok(Vec::new()) };
            ui.send(UiEvent::UsbDevices(present, list));
        });
    }

    fn map_mouse(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let (vw, vh) = self.session.as_ref()?.video_size()?;
        let r = self.renderer.as_ref()?;
        let rect = fit(r.width, r.height, vw, vh);
        let nx = ((x - rect.x) / rect.w).clamp(0.0, 1.0);
        let ny = ((y - rect.y) / rect.h).clamp(0.0, 1.0);
        Some(((nx * 65535.0).round() as u32, (ny * 65535.0).round() as u32))
    }

    /// Switch between the remote cursor and a normal arrow over the toolbar.
    fn update_cursor_over_ui(&mut self, over: bool) {
        if over == self.cursor_over_ui {
            return;
        }
        self.cursor_over_ui = over;
        if over {
            if let (Some(w), Some(_)) = (&self.window, &self.session) {
                w.set_cursor(CursorIcon::Default);
                w.set_cursor_visible(true);
            }
        } else {
            self.show_remote_cursor();
        }
    }

    /// After a UI frame. egui sets its own cursor when its icon changes or the
    /// pointer comes back into the window; outside the toolbar the remote
    /// cursor goes back (it stayed a local arrow, or showed while hidden).
    fn sync_cursor_after_ui(&mut self, over: bool, egui_set: bool) {
        if egui_set {
            if let Some(s) = self.session.as_mut() {
                if s.cursor_log.line() {
                    tracing::info!(
                        "ui set the cursor (over toolbar {over}); remote cursor {} shape {:08x}",
                        if s.cursor_visible { "shown" } else { "hidden" },
                        s.cursor_shape
                    );
                }
            }
            self.cursor_over_ui = over;
            if !over {
                self.show_remote_cursor();
            }
        } else {
            self.update_cursor_over_ui(over);
        }
    }

    fn show_remote_cursor(&self) {
        let (Some(w), Some(s)) = (&self.window, &self.session) else { return };
        if s.relative {
            w.set_cursor_visible(false);
            return;
        }
        if let Some(c) = s.cursors.get(&s.cursor_shape) {
            w.set_cursor(c.clone());
        }
        w.set_cursor_visible(s.cursor_visible);
    }

    fn on_cursor(&mut self, el: &ActiveEventLoop, m: pb::CursorMsg) {
        let Some(s) = self.session.as_mut() else { return };
        match m.msg {
            Some(cursor_msg::Msg::Shape(sh)) => {
                if s.cursor_log.line() {
                    let opaque = sh.rgba.chunks_exact(4).filter(|p| p[3] != 0).count();
                    tracing::info!("cursor shape {:08x}: {}x{} hot ({},{}) opaque {opaque}", sh.id, sh.width, sh.height, sh.hot_x, sh.hot_y);
                }
                let src = CustomCursor::from_rgba(
                    sh.rgba,
                    sh.width.min(u16::MAX as u32) as u16,
                    sh.height.min(u16::MAX as u32) as u16,
                    sh.hot_x.clamp(0, sh.width as i32 - 1) as u16,
                    sh.hot_y.clamp(0, sh.height as i32 - 1) as u16,
                );
                match src {
                    Ok(src) => {
                        s.cursors.insert(sh.id, el.create_custom_cursor(src));
                    }
                    Err(e) => tracing::warn!("cursor shape {:08x}: {e}", sh.id),
                }
            }
            Some(cursor_msg::Msg::State(st)) if st.slot != 0 => {
                // Cursor of a display shown in an extra window.
                // (Fields, not methods: `s` borrows the session.)
                let Some(w) = self.extras.values().find(|w| w.slot == st.slot).map(|w| w.window.clone()) else { return };
                if let Some(v) = s.views.get_mut(&st.slot) {
                    if v.cursor_shape != st.shape_id {
                        if let Some(c) = s.cursors.get(&st.shape_id) {
                            w.set_cursor(c.clone());
                            v.cursor_shape = st.shape_id;
                        } else if s.cursor_log.line() {
                            tracing::info!("cursor state names unknown shape {:08x} (slot {})", st.shape_id, st.slot);
                        }
                    }
                    if v.cursor_visible != st.visible {
                        s.cursor_log.visibility(st.visible, st.slot, st.shape_id);
                        v.cursor_visible = st.visible;
                        w.set_cursor_visible(st.visible);
                    }
                }
            }
            Some(cursor_msg::Msg::State(st)) => {
                let Some(w) = &self.window else { return };
                if st.shape_id != s.cursor_shape {
                    if let Some(c) = s.cursors.get(&st.shape_id) {
                        if !self.cursor_over_ui {
                            w.set_cursor(c.clone());
                        }
                        s.cursor_shape = st.shape_id;
                    } else if s.cursor_log.line() {
                        tracing::info!("cursor state names unknown shape {:08x}", st.shape_id);
                    }
                }
                if st.visible != s.cursor_visible {
                    s.cursor_log.visibility(st.visible, 0, st.shape_id);
                    s.cursor_visible = st.visible;
                    if !s.relative && !self.cursor_over_ui {
                        w.set_cursor_visible(st.visible);
                    }
                }
            }
            None => {}
        }
    }

    fn hotkey_from_key(&self, event: &winit::event::KeyEvent) -> Option<Hotkey> {
        use winit::keyboard::{KeyCode, PhysicalKey};
        let m = self.mods;
        if event.state != ElementState::Pressed || !(m.control_key() && m.alt_key() && m.shift_key()) {
            return None;
        }
        let PhysicalKey::Code(c) = event.physical_key else { return None };
        Some(match c {
            KeyCode::KeyQ => Hotkey::ToggleGrab,
            KeyCode::KeyS => Hotkey::ToggleStats,
            KeyCode::KeyM => Hotkey::ToggleMode,
            KeyCode::KeyR => Hotkey::ToggleRelative,
            KeyCode::KeyF => Hotkey::ToggleFullscreen,
            KeyCode::KeyD => Hotkey::CtrlAltDel,
            KeyCode::KeyT => Hotkey::ToggleToolbar,
            KeyCode::KeyX => Hotkey::Quit,
            KeyCode::Digit1 => Hotkey::Display(1),
            KeyCode::Digit2 => Hotkey::Display(2),
            KeyCode::Digit3 => Hotkey::Display(3),
            KeyCode::Digit4 => Hotkey::Display(4),
            _ => return None,
        })
    }

    /// Mouse / keyboard events while a session is active.
    fn session_input(&mut self, event: &WindowEvent) {
        let over_ui = self.gui.as_ref().is_some_and(|g| g.ctx.is_pointer_over_area() || g.ctx.is_using_pointer());
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.update_cursor_over_ui(over_ui);
                let relative = self.session.as_ref().is_some_and(|s| s.relative);
                if !over_ui && !relative && self.focused {
                    if let Some((x, y)) = self.map_mouse(position.x, position.y) {
                        if let Some(s) = &self.session {
                            s.send_input(Ev::MouseAbs(pb::MouseAbs { x, y, slot: 0 }));
                        }
                    }
                }
                // Keep the toolbar handle responsive near the top edge.
                if position.y < 60.0 || over_ui {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (b, bit) = match button {
                    MouseButton::Left => (pb::MouseButton::Left, 1),
                    MouseButton::Right => (pb::MouseButton::Right, 2),
                    MouseButton::Middle => (pb::MouseButton::Middle, 4),
                    MouseButton::Back => (pb::MouseButton::X1, 8),
                    MouseButton::Forward => (pb::MouseButton::X2, 16),
                    MouseButton::Other(_) => return,
                };
                let down = *state == ElementState::Pressed;
                // Presses on the toolbar stay local; a release always follows its press.
                let forward = if down { !over_ui } else { self.remote_buttons & bit != 0 };
                if forward {
                    if down {
                        self.remote_buttons |= bit;
                    } else {
                        self.remote_buttons &= !bit;
                    }
                    if let Some(s) = &self.session {
                        s.send_input(Ev::MouseButton(pb::MouseButtonEv { button: b as i32, down }));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !over_ui => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32),
                    MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                if dx != 0 || dy != 0 {
                    if let Some(s) = &self.session {
                        s.send_input(Ev::Wheel(pb::Wheel { dx, dy }));
                    }
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if let Some(s) = &mut self.session {
                    s.winit_keys += 1;
                }
                if let Some(h) = self.hotkey_from_key(event) {
                    self.hotkey(h);
                    return;
                }
                // Keys the hook forwarded were swallowed and never reach the window, so
                // anything arriving here still has to go to the host (the hook is
                // not running, or missed the key).
                if input::grabbed() && self.focused {
                    if !is_synthetic {
                        input::key_missed_hook();
                    }
                    use winit::platform::scancode::PhysicalKeyExtScancode;
                    if let (Some(sc), Some(s)) = (event.physical_key.to_scancode(), &self.session) {
                        let (scancode, extended) = (sc & 0xff, sc & 0xff00 == 0xe000);
                        if scancode != 0 {
                            s.send_input(Ev::Key(pb::Key { scancode, extended, down: event.state == ElementState::Pressed }));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // The launcher and the session each get a window, so the launcher
        // stays usable during a session.
        let launcher = Window::default_attributes()
            .with_window_icon(app_icon())
            .with_visible(false)
            .with_title("NyaRemoteControl")
            .with_inner_size(LogicalSize::new(1100.0, 760.0))
            .with_min_inner_size(LogicalSize::new(640.0, 480.0));
        let launcher = match el.create_window(launcher) {
            Ok(l) => Arc::new(l),
            Err(e) => {
                crate::fatal(&format!("无法创建窗口：{e}"));
                el.exit();
                return;
            }
        };
        self.launcher = Some(launcher);
        self.launcher_reveal = Some(Instant::now() + Duration::from_secs(4));
        self.tray = crate::tray::create(self.ui_tx.clone());
        if self.start_in_tray && self.tray.is_some() {
            self.launcher_reveal = None;
        }
        // The first (idle) session window, ready for a connection.
        if let Err(e) = self.new_conn(el) {
            crate::fatal(&format!("{e:#}"));
            el.exit();
            return;
        }
        self.create_web();
        // winit registered keyboards for raw input with the mice: that keeps
        // the keyboard hook from being called for our windows (input.rs).
        input::drop_raw_keyboard();
        let ui = self.ui_tx.clone();
        std::thread::spawn(move || ui.send(UiEvent::DecodeSummary(crate::diag::decode_summary())));
        if self.cfg.check_updates {
            self.check_update();
        }
        if let Some((target, name, overrides)) = self.auto_connect.take() {
            self.connect(target, name, Some(overrides));
        }
        if let Some(link) = self.start_link.take() {
            self.offer_invite(&link);
        }
        self.draw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, mut event: WindowEvent) {
        let launcher = self.launcher.as_ref().is_some_and(|l| l.id() == id);
        if !launcher {
            match self.conn_of_window(id) {
                Some(c) => {
                    self.activate(c);
                }
                None => return,
            }
        }
        // The window is now the launcher, `self.window` or one of `self.extras`.
        let win = [self.launcher.as_ref(), self.window.as_ref()].into_iter().flatten().find(|w| w.id() == id).cloned();
        if let Some(w) = win.or_else(|| self.extras.get(&id).map(|e| e.window.clone())) {
            self.guard_size(&w, &mut event);
        }
        if launcher {
            return self.launcher_event(el, event);
        }
        let Some(window) = self.window.clone() else { return };
        if id != window.id() {
            return self.extra_event(id, event);
        }
        // Keyboard goes to egui only in the launcher (in a session it belongs to the host).
        let keyboard = matches!(event, WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) | WindowEvent::Ime(_));
        if self.session.is_none() || !keyboard {
            if let Some(g) = self.gui.as_mut() {
                let r = g.on_event(&window, &event);
                if r.repaint && self.session.is_none() {
                    window.request_redraw();
                }
            }
        }
        match &event {
            // Closing the session window disconnects; the launcher stays.
            WindowEvent::CloseRequested => self.end_session(Some((Kind::Info, "已断开连接".into()))),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    if let Err(e) = r.resize(size.width, size.height) {
                        tracing::warn!("resize: {e:#}");
                    }
                }
                if self.session.as_ref().is_some_and(|s| s.vd_follow_window) {
                    // Wait until resizing stops: a size the driver does not offer yet restarts it.
                    self.vd_resize_at = Some(Instant::now() + Duration::from_millis(800));
                }
                self.draw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Focused(f) => {
                self.focused = *f;
                if *f {
                    // The keyboard hook sends keys to the focused session.
                    if let Some(s) = &self.session {
                        input::set_session(Some(s.net_tx.clone()));
                        input::reinstall_hook();
                    }
                }
                if *f {
                    // Whatever registered raw keyboard input since: the hook needs it gone.
                    input::drop_raw_keyboard();
                }
                if let Some(g) = self.session.as_ref().and_then(|s| s.gamepads.as_ref()) {
                    g.set_active(*f);
                }
                if !*f {
                    input::reset_modifiers();
                    if let Some(s) = &self.session {
                        s.release_all();
                    }
                    self.remote_buttons = 0;
                    if self.session.as_ref().is_some_and(|s| s.relative) {
                        self.set_relative(false);
                    }
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::HoveredFile(_) if self.session.is_some() => {
                self.hovering_file = true;
                window.request_redraw();
            }
            WindowEvent::HoveredFileCancelled => {
                self.hovering_file = false;
                window.request_redraw();
            }
            WindowEvent::DroppedFile(p) if self.session.is_some() => {
                self.hovering_file = false;
                self.dropped.push(p.clone());
            }
            WindowEvent::Moved(_) => {
                // Onto an HDR monitor or off it: the swap chain follows.
                if let Some(r) = self.renderer.as_mut() {
                    r.refresh_display();
                }
                // Moving to a monitor on another GPU: follow it (design doc §3.5, client side).
                if let (Some(m), Ok(topo)) = (window.current_monitor(), Topology::enumerate()) {
                    if let Some(a) = topo.adapter_for_monitor(m.hmonitor()) {
                        if a.luid != self.adapter_luid && self.adapter_luid != 0 {
                            tracing::info!("window moved to GPU {}; recreating device", a.name);
                            self.recreate_device();
                        }
                    }
                }
            }
            _ => {}
        }
        if self.session.is_some() {
            self.session_input(&event);
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            self.activate_focused();
            if let Some(s) = &self.session {
                if s.relative && self.focused {
                    let (dx, dy) = (delta.0.round() as i32, delta.1.round() as i32);
                    if dx != 0 || dy != 0 {
                        s.send_input(Ev::MouseRel(pb::MouseRel { dx, dy }));
                    }
                }
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UiEvent) {
        let event = match event {
            UiEvent::Conn(id, ev) => {
                if !self.activate(id) {
                    return; // that connection is gone
                }
                *ev
            }
            UiEvent::Hotkey(h) => {
                self.activate_focused();
                UiEvent::Hotkey(h)
            }
            ev => ev,
        };
        match event {
            UiEvent::Frame(0) => return self.draw(),
            UiEvent::Frame(slot) => {
                if let Some(id) = self.extra_of_slot(slot) {
                    self.draw_extra(id);
                }
                return;
            }
            UiEvent::Cursor(m) => return self.on_cursor(el, m),
            UiEvent::ConnectDone(d) => self.on_connect_done(d),
            UiEvent::Web(c) => self.on_web_call(c),
            UiEvent::Tray(crate::tray::TrayAction::Open) => self.bring_launcher_back(),
            UiEvent::Tray(crate::tray::TrayAction::Quit) => {
                tracing::info!("quit from the tray");
                self.cancel_connect();
                self.exit = true;
            }
            UiEvent::DecodeSummary(s) => {
                self.decode_summary = s;
                self.push_state();
            }
            UiEvent::UpdateChecked(r) => self.on_update_checked(r),
            UiEvent::UpdateProgress(p) => self.on_update_progress(p),
            UiEvent::UpdateDownloaded(r) => self.on_update_downloaded(r),
            UiEvent::Host(ev) => self.host.event(self.web.as_ref(), ev),
            UiEvent::WebReply(id, r) => {
                if let Some(w) = &self.web {
                    w.reply(id, r);
                }
            }
            UiEvent::OpenLink(link) => self.offer_invite(&link),
            UiEvent::NeedPairing(tx) => {
                self.pair_reply = Some(tx);
                let label = self.pending.as_ref().map(Self::pending_label).unwrap_or_default();
                self.set_phase(Phase::Pairing(label));
            }
            UiEvent::Hotkey(h) => self.hotkey(h),
            UiEvent::Disconnected(msg) => self.end_session(Some((Kind::Error, msg))),
            UiEvent::ClipOffer(o) => {
                if let Some(s) = &mut self.session {
                    s.on_clip_offer(o);
                }
            }
            UiEvent::FileOffer(o) => {
                if let Some(s) = &mut self.session {
                    s.on_offer(o);
                }
            }
            UiEvent::FileResult(r) => {
                if let Some(s) = &mut self.session {
                    s.on_file_result(r);
                }
            }
            UiEvent::Transfer(u) => {
                if let Some(s) = &mut self.session {
                    s.on_transfer(u);
                }
            }
            UiEvent::UsbStatus(st) => {
                if let Some(s) = &mut self.session {
                    s.on_usb_status(st);
                }
            }
            UiEvent::PrintJob(path) => {
                let mode = self.sd.print_mode.clone();
                let ui = self.ui_tx.for_conn(self.conn_id);
                std::thread::spawn(move || ui.send(UiEvent::PrintDone(crate::printing::handle(&path, &mode))));
            }
            UiEvent::PrintDone(msg) => {
                if let Some(s) = &mut self.session {
                    s.notice(msg, Duration::from_secs(10));
                }
            }
            UiEvent::FolderMount(st) => {
                if let Some(s) = &mut self.session {
                    let msg = if st.mounted {
                        format!("共享文件夹已出现在被控端的 {} 盘", st.mount_point.trim_end_matches(':'))
                    } else if !st.message.is_empty() {
                        format!("共享文件夹没有挂载：{}", st.message)
                    } else {
                        String::new()
                    };
                    if !msg.is_empty() {
                        tracing::info!("{msg}");
                        s.notice(msg, Duration::from_secs(8));
                    }
                }
            }
            UiEvent::UsbipdInstall(running, msg) => {
                let done = !running;
                if let Some(s) = &mut self.session {
                    s.usbipd_install = Some((running, msg));
                }
                if done {
                    self.refresh_usb();
                }
            }
            UiEvent::UsbDevices(present, r) => {
                if let Some(s) = &mut self.session {
                    s.usbipd_present = Some(present);
                    s.usb_devices = Some(r);
                }
            }
            UiEvent::ClipboardImage(dib) => {
                if let Some(s) = &self.session {
                    s.clipboard_image_from_host(dib);
                }
            }
            other => {
                let Some(s) = self.session.as_mut() else { return };
                match other {
                    UiEvent::Connected => s.status.clear(),
                    UiEvent::Role(r) => {
                        let was_watching = s.watching();
                        s.role = Some(r);
                        if was_watching && !s.watching() {
                            s.notice("你现在操作被控端".into(), Duration::from_secs(4));
                        } else if !was_watching && s.watching() {
                            let who = s.role.as_ref().map(|r| r.controller.clone()).unwrap_or_default();
                            s.notice(format!("{who} 接管了操作，你现在只能观看"), Duration::from_secs(6));
                        }
                    }
                    UiEvent::SessionInfo(i) => {
                        s.on_session_info(i, self.focused);
                        self.sync_extras = true;
                    }
                    UiEvent::GamepadRumble(r) => {
                        if let Some(g) = &s.gamepads {
                            g.rumble(&r);
                        }
                    }
                    UiEvent::StreamStarted(st) if st.slot != 0 => {
                        let slot = st.slot;
                        if let Some(v) = s.views.get_mut(&slot) {
                            v.status.clear();
                            v.stream = Some(st);
                        }
                        if let Some(id) = self.extra_of_slot(slot) {
                            self.extras[&id].window.request_redraw();
                        }
                    }
                    UiEvent::StreamStarted(st) => {
                        self.sync_extras = true;
                        tracing::info!(
                            "stream: {} {:?} cross_gpu={}",
                            st.encoder_name,
                            st.config.as_ref().map(|c| (c.width, c.height, c.fps, c.bitrate_kbps)),
                            st.cross_gpu
                        );
                        s.game = st.config.as_ref().is_some_and(|c| c.mode == pb::StreamMode::Game as i32);
                        s.stream = Some(st);
                        if s.status_until.is_none() {
                            s.status.clear();
                        }
                        s.cursor_shape = 0;
                    }
                    UiEvent::StreamError(slot, e) if slot != 0 => {
                        if let Some(v) = s.views.get_mut(&slot) {
                            v.status = e;
                        }
                        if let Some(id) = self.extra_of_slot(slot) {
                            self.extras[&id].window.request_redraw();
                        }
                    }
                    UiEvent::StreamError(_, e) if e.starts_with("虚拟显示器") => {
                        tracing::error!("{e}");
                        s.virtual_display_failed(&e);
                    }
                    UiEvent::StreamError(_, e) => {
                        tracing::error!("stream error: {e}");
                        s.status = format!("被控端无法开始推流：{e}");
                    }
                    UiEvent::ServerStats(st) if st.slot != 0 => {
                        let slot = st.slot;
                        if let Some(v) = s.views.get_mut(&slot) {
                            show_capture_note(&mut v.capture_note, &mut v.status, &st.capture_note);
                            v.server_stats = Some(st);
                        }
                        if let Some(id) = self.extra_of_slot(slot) {
                            self.extras[&id].window.request_redraw();
                        }
                    }
                    UiEvent::ServerStats(st) => {
                        show_capture_note(&mut s.capture_note, &mut s.status, &st.capture_note);
                        s.server_stats = Some(st);
                    }
                    UiEvent::Transport { tcp } => s.via_tcp = Some(tcp),
                    UiEvent::Clipboard(t) => s.clipboard_from_host(t),
                    UiEvent::Reconnecting(msg) => {
                        s.status = format!("连接中断，正在重连…（{msg}）");
                        s.release_all();
                    }
                    _ => {}
                }
                self.update_title();
            }
        }
        self.request_redraw();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.exit {
            el.exit();
            return;
        }
        let mut wake = Instant::now() + Duration::from_millis(250);
        if let Some(at) = self.launcher_reveal {
            if Instant::now() >= at {
                tracing::warn!("launcher page not up after 4 s; showing the window anyway");
                self.reveal_launcher();
            } else {
                wake = wake.min(at);
            }
        }
        for id in self.conn_ids() {
            if self.activate(id) {
                wake = wake.min(self.conn_tick(el));
            }
        }
        self.keep_one_idle(el);
        // "本机" follows the service while the launcher is on screen (the
        // control pipe is asked on this thread, so not behind a session).
        if self.launcher.as_ref().is_some_and(|l| l.is_visible() != Some(false) && l.is_minimized() != Some(true)) {
            self.host.tick(self.web.as_ref());
        }
        el.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}

impl App {
    /// Timers and queued work of the current connection; returns when to wake up next.
    fn conn_tick(&mut self, el: &ActiveEventLoop) -> Instant {
        if std::mem::take(&mut self.sync_extras) {
            self.sync_extras();
        }
        for display_id in std::mem::take(&mut self.open_requests) {
            self.open_extra(el, display_id);
        }
        let mut redraw = false;
        // One drop delivers one DroppedFile event per file; send them as a batch.
        if !self.dropped.is_empty() {
            let files = std::mem::take(&mut self.dropped);
            if let Some(s) = &mut self.session {
                s.send_files(files);
            }
            redraw = true;
        }
        if let Some(s) = &mut self.session {
            if s.tick() {
                redraw = s.show_stats;
                self.update_title();
                // HDR switched on or off in Windows' display settings.
                if let Some(r) = self.renderer.as_mut() {
                    redraw |= r.refresh_display();
                }
                for w in self.extras.values_mut() {
                    w.refresh_display();
                }
            }
        }
        if self.repaint_at.is_some_and(|t| Instant::now() >= t) {
            self.repaint_at = None;
            redraw = true;
        }
        if self.vd_resize_at.is_some_and(|t| Instant::now() >= t) {
            self.vd_resize_at = None;
            self.fit_virtual_display();
            redraw = true;
        }
        if redraw {
            self.request_redraw();
        }
        let next = Instant::now() + Duration::from_millis(250);
        let wake = self.repaint_at.map_or(next, |t| t.min(next));
        self.vd_resize_at.map_or(wake, |t| t.min(wake))
    }
}
