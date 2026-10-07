//! One connection to a host: worker threads, network supervisor, stream
//! state and statistics. Created when a connection succeeds, dropped when it
//! ends (the window then returns to the launcher).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use nya_proto::pb::{self, control_msg::Msg, input_msg::Ev};
use nya_win::d3d::D3dDevice;
use tokio::sync::mpsc::UnboundedSender;
use winit::window::CustomCursor;

use crate::clipboard::ClipIn;
use crate::events::{NetCmd, TransferUpdate, Ui};
use crate::net::{self, Link, Params, Sinks, VideoRoutes};
use crate::stats::{Shared, Summary};
use crate::video::{FrameStore, Slot, VideoIn, VideoThread};
use crate::input;

#[derive(Debug, Clone)]
pub enum TransferState {
    Running,
    Done(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TransferView {
    pub id: u64,
    pub upload: bool,
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub state: TransferState,
    /// Local folder with the downloaded files.
    pub folder: Option<std::path::PathBuf>,
}

/// Host display choices of the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DisplayChoice {
    pub count: u32,
    pub physical_off: bool,
    pub block_input: bool,
}

/// An extra window's stream: another host display shown at the same time
/// (FEATURE_MULTI_STREAM). The window itself belongs to the app.
pub struct View {
    pub display_id: u32,
    video_tx: Sender<VideoIn>,
    pub store: Arc<FrameStore>,
    pub stats: Arc<Shared>,
    pub summary: Summary,
    pub server_stats: Option<pb::ServerStats>,
    pub stream: Option<pb::StreamStarted>,
    pub current: Option<Arc<Slot>>,
    pub cursor_shape: u32,
    pub cursor_visible: bool,
    pub status: String,
}

/// Cursor events in the client log: the first ones one by one, then a
/// summary now and then (the pointer vanishing is chased with it).
#[derive(Default)]
pub struct CursorLog {
    lines: u32,
    hides: u32,
    shows: u32,
    since: Option<Instant>,
}

impl CursorLog {
    const LINES: u32 = 300;

    /// Whether one more event may be logged on its own line.
    pub fn line(&mut self) -> bool {
        self.lines += 1;
        self.lines <= Self::LINES
    }

    pub fn visibility(&mut self, visible: bool, slot: u32, shape: u32) {
        if visible {
            self.shows += 1;
        } else {
            self.hides += 1;
        }
        if self.line() {
            tracing::info!("remote cursor {} (slot {slot}, shape {shape:08x})", if visible { "shown" } else { "hidden" });
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        if self.lines > Self::LINES && since.elapsed() >= Duration::from_secs(10) {
            tracing::info!("remote cursor (10 s+): {} hidden, {} shown; now {}", self.hides, self.shows, if visible { "shown" } else { "hidden" });
            (self.hides, self.shows, self.since) = (0, 0, Some(Instant::now()));
        }
    }
}

pub struct SessionOptions {
    pub hw_decode: bool,
    pub audio: bool,
    pub clipboard: bool,
}

pub struct Session {
    pub label: String,
    pub net_tx: UnboundedSender<NetCmd>,
    video_tx: Sender<VideoIn>,
    /// Decoder inputs by slot, shared with the network task.
    routes: Arc<VideoRoutes>,
    /// Extra windows by slot (> 0).
    pub views: std::collections::BTreeMap<u32, View>,
    next_slot: u32,
    /// The host can stream several displays at once.
    pub multi_supported: bool,
    dev: D3dDevice,
    hw_decode: bool,
    caps: pb::ClientCaps,
    ui: Ui,
    pub stats: Arc<Shared>,
    pub store: Arc<FrameStore>,
    clip_tx: Option<Sender<ClipIn>>,
    pub transfers: Vec<TransferView>,
    mic_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub gamepads: Option<crate::gamepad::Gamepads>,
    pub usb_open: bool,
    pub usb_devices: Option<Result<Vec<crate::usb::UsbDevice>, String>>,
    /// usbipd-win installed here; `None` while checking.
    pub usbipd_present: Option<bool>,
    /// busid -> (attached on the host, last message)
    pub usb_state: HashMap<String, (bool, String)>,
    pub usb_busy: std::collections::HashSet<String>,
    /// usbipd-win one-click install: (running, message)
    pub usbipd_install: Option<(bool, String)>,
    pub offers: Vec<pb::FileOffer>,
    /// Current stream request (display, mode …), replayed on reconnect.
    pub start: pb::StartStream,
    pub status: String,
    /// Clear `status` at this time (short notices).
    pub status_until: Option<Instant>,
    /// The host speaks FEATURE_VIRTUAL_DISPLAY.
    pub vd_supported: bool,
    /// Turn the microphone on once the host says it can take it.
    pub mic_auto: bool,
    /// Who operates the host when other clients are connected too
    /// (`None`: an older host, or nobody else; this client operates it).
    pub role: Option<pb::SessionRole>,
    /// Virtual display follows the window size.
    pub vd_follow_window: bool,
    pub info: Option<pb::SessionInfo>,
    pub stream: Option<pb::StreamStarted>,
    pub server_stats: Option<pb::ServerStats>,
    pub current: Option<Arc<Slot>>,
    pub cursors: HashMap<u32, CustomCursor>,
    pub cursor_shape: u32,
    pub cursor_visible: bool,
    pub cursor_log: CursorLog,
    pub relative: bool,
    pub game: bool,
    pub show_stats: bool,
    /// Connection mode (setting) and what the connection uses now.
    pub transport: crate::net::Transport,
    pub via_tcp: Option<bool>,
    pub summary: Summary,
    last_tick: Instant,
    status_log: Instant,
    last_rendered_total: u64,
    pub winit_keys: u64,
    logged_keys: (u64, u64),
}

fn ctl(m: Msg) -> NetCmd {
    NetCmd::Control(pb::ControlMsg { msg: Some(m) })
}

impl Session {
    pub fn start(
        rt: &tokio::runtime::Handle,
        link: Link,
        params: Params,
        dev: &D3dDevice,
        opts: &SessionOptions,
        ui: Ui,
        label: String,
    ) -> Self {
        let (net_tx, net_rx) = tokio::sync::mpsc::unbounded_channel();
        let (video_tx, video_rx) = crossbeam_channel::bounded(16);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded(64);
        let stats = Arc::new(Shared::new());
        let store = Arc::new(FrameStore::default());
        let game = params.start.config.as_ref().is_some_and(|c| c.mode == pb::StreamMode::Game as i32);

        VideoThread {
            slot: 0,
            hw_allowed: opts.hw_decode,
            caps: params.caps.clone(),
            store: store.clone(),
            ui: ui.clone(),
            net: net_tx.clone(),
            stats: stats.clone(),
        }
        .spawn(dev.clone(), video_rx);
        if opts.audio {
            crate::audio::spawn(audio_rx, stats.clone());
        }
        let clip_files = link.neg.has(nya_proto::pb::Feature::FileTransfer) && link.neg.has(nya_proto::pb::Feature::ClipboardFiles);
        let clip_tx = opts.clipboard.then(|| {
            let (tx, rx) = crossbeam_channel::unbounded();
            crate::clipboard::spawn(rx, net_tx.clone(), clip_files);
            tx
        });
        input::set_session(Some(net_tx.clone()));

        let start = params.start.clone();
        let multi_supported = link.neg.has(nya_proto::pb::Feature::MultiStream);
        let caps = params.caps.clone();
        let routes = Arc::new(VideoRoutes::default());
        routes.set(0, Some(video_tx.clone()));
        let sinks = Sinks { ui: ui.clone(), video: routes.clone(), audio: audio_tx, stats: stats.clone(), clip: Default::default() };
        rt.spawn(net::supervise(link, params, net_rx, sinks));

        Self {
            label,
            net_tx,
            video_tx,
            routes,
            views: Default::default(),
            next_slot: 1,
            multi_supported,
            dev: dev.clone(),
            hw_decode: opts.hw_decode,
            caps,
            ui,
            stats,
            store,
            clip_tx,
            transfers: Vec::new(),
            mic_stop: None,
            gamepads: None,
            usb_open: false,
            usb_devices: None,
            usbipd_present: None,
            usb_state: HashMap::new(),
            usb_busy: Default::default(),
            usbipd_install: None,
            offers: Vec::new(),
            start,
            status: "连接中".into(),
            status_until: None,
            vd_supported: false,
            mic_auto: false,
            role: None,
            vd_follow_window: false,
            info: None,
            stream: None,
            server_stats: None,
            current: None,
            cursors: HashMap::new(),
            cursor_shape: 0,
            cursor_visible: true,
            cursor_log: CursorLog::default(),
            relative: false,
            game,
            show_stats: false,
            transport: crate::net::Transport::Auto,
            via_tcp: None,
            summary: Summary::default(),
            last_tick: Instant::now(),
            status_log: Instant::now(),
            last_rendered_total: 0,
            winit_keys: 0,
            logged_keys: (0, 0),
        }
    }

    pub fn send_input(&self, ev: Ev) {
        if self.watching() {
            return; // the host ignores it anyway
        }
        let _ = self.net_tx.send(NetCmd::Input(pb::InputMsg { ev: Some(ev) }));
    }

    /// Another client operates the host; this one only watches.
    pub fn watching(&self) -> bool {
        self.role.as_ref().is_some_and(|r| !r.controlling)
    }

    /// Operate the host from now on; the other client watches, or is
    /// disconnected with `kick`.
    pub fn take_control(&mut self, kick: bool) {
        let _ = self.net_tx.send(ctl(Msg::TakeControl(pb::TakeControl { kick })));
        self.status = "正在接管操作…".into();
    }

    pub fn release_all(&self) {
        self.send_input(Ev::ReleaseAll(pb::ReleaseAll {}));
    }

    pub fn set_device(&mut self, dev: &D3dDevice) {
        self.dev = dev.clone();
        let _ = self.video_tx.send(VideoIn::Device(dev.clone()));
        for v in self.views.values() {
            let _ = v.video_tx.send(VideoIn::Device(dev.clone()));
        }
    }

    /// Show host display `display_id` in an extra window: start its stream.
    /// Returns the new slot.
    pub fn open_view(&mut self, display_id: u32) -> u32 {
        let slot = self.next_slot;
        self.next_slot += 1;
        let (video_tx, video_rx) = crossbeam_channel::bounded(16);
        let store = Arc::new(FrameStore::default());
        let stats = Arc::new(Shared::new());
        VideoThread {
            slot,
            hw_allowed: self.hw_decode,
            caps: self.caps.clone(),
            store: store.clone(),
            ui: self.ui.clone(),
            net: self.net_tx.clone(),
            stats: stats.clone(),
        }
        .spawn(self.dev.clone(), video_rx);
        self.routes.set(slot, Some(video_tx.clone()));
        // Same picture settings as the main window; the display setup is its business.
        let req = pb::StartStream { display_id, slot, display_setup: None, ..self.start.clone() };
        let _ = self.net_tx.send(ctl(Msg::StartStream(req)));
        self.views.insert(
            slot,
            View {
                display_id,
                video_tx,
                store,
                stats,
                summary: Summary::default(),
                server_stats: None,
                stream: None,
                current: None,
                cursor_shape: 0,
                cursor_visible: true,
                status: "连接中".into(),
            },
        );
        tracing::info!("extra window: display {display_id} in slot {slot}");
        slot
    }

    /// The extra window of `slot` was closed: stop its stream.
    pub fn close_view(&mut self, slot: u32) {
        if self.views.remove(&slot).is_some() {
            self.routes.set(slot, None);
            let _ = self.net_tx.send(ctl(Msg::StopStream(pb::StopStream { slot })));
        }
    }

    /// Slot of the extra window showing `display_id`, if any.
    pub fn view_of(&self, display_id: u32) -> Option<u32> {
        self.views.iter().find(|(_, v)| v.display_id == display_id).map(|(s, _)| *s)
    }

    /// Display name for window titles and menus ("屏幕 2 · 虚拟").
    pub fn display_title(&self, display_id: u32) -> String {
        let Some(info) = &self.info else { return "显示器".into() };
        match info.displays.iter().position(|d| d.id == display_id) {
            Some(i) => format!("屏幕 {}{}", i + 1, if info.displays[i].is_virtual { " · 虚拟" } else { "" }),
            None => "显示器".into(),
        }
    }

    pub fn set_game_mode(&mut self, game: bool) {
        if self.game == game {
            return;
        }
        self.game = game;
        let mode = if game { pb::StreamMode::Game } else { pb::StreamMode::Office } as i32;
        self.start.config.get_or_insert_with(Default::default).mode = mode;
        let _ = self.net_tx.send(ctl(Msg::SetMode(pb::SetMode { mode })));
        self.status = if game { "正在切换到游戏模式…" } else { "正在切换到办公模式…" }.into();
    }

    /// Switch to the host display with this id.
    pub fn select_display(&mut self, id: u32) {
        if self.stream.as_ref().is_some_and(|s| s.display_id == id) {
            return;
        }
        self.start.display_id = id;
        let _ = self.net_tx.send(ctl(Msg::StartStream(self.start.clone())));
        self.status = "正在切换显示器…".into();
    }

    /// 1-based index into the host's display list.
    pub fn select_display_index(&mut self, n: u8) {
        let id = self.info.as_ref().and_then(|s| s.displays.get(n as usize - 1)).map(|d| d.id);
        if let Some(id) = id {
            self.select_display(id);
        }
    }

    pub fn bitrate_policy(&self) -> i32 {
        self.start.config.as_ref().map(|c| c.bitrate_policy).unwrap_or(0)
    }

    /// Restarts the stream with the new policy.
    pub fn set_bitrate_policy(&mut self, p: pb::BitratePolicy) {
        if self.bitrate_policy() == p as i32 {
            return;
        }
        self.start.config.get_or_insert_with(Default::default).bitrate_policy = p as i32;
        let _ = self.net_tx.send(ctl(Msg::StartStream(self.start.clone())));
        self.status = "正在切换码率策略…".into();
    }

    /// (virtual screens, physical displays off, local input blocked) as requested now.
    pub fn display_choice(&self) -> DisplayChoice {
        let s = self.start.display_setup.as_ref();
        DisplayChoice {
            count: s.map(|s| s.virtual_screens.len() as u32).unwrap_or(0),
            physical_off: s.is_some_and(|s| s.physical_off),
            block_input: s.is_some_and(|s| s.block_local_input),
        }
    }

    /// Can the host create a virtual display?
    pub fn vd_available(&self) -> bool {
        self.vd_supported && self.info.as_ref().is_some_and(|i| i.virtual_display_available)
    }

    /// Change the host display setup (virtual screens, their size, physical
    /// displays, local input).
    pub fn set_display_setup(&mut self, setup: Option<pb::DisplaySetup>) {
        let setup = setup.filter(|s| !s.virtual_screens.is_empty() || s.block_local_input);
        if self.start.display_setup == setup {
            return;
        }
        let was = self.display_choice();
        self.start.display_setup = setup.clone();
        let now = self.display_choice();
        if now.count != was.count {
            // The first virtual screen is the host's primary display while it exists.
            self.start.display_id = 0;
        }
        let _ = self.net_tx.send(ctl(Msg::StartStream(self.start.clone())));
        self.status = if now == was {
            let v = setup.and_then(|s| s.virtual_screens.first().copied()).unwrap_or_default();
            format!("正在调整被控端分辨率为 {}x{}…", v.width, v.height)
        } else if now.count == 0 && was.count > 0 {
            "正在移除虚拟显示器…".into()
        } else if now.count != was.count {
            format!("正在创建 {} 个虚拟显示器…", now.count)
        } else {
            "正在调整被控端显示器…".into()
        };
    }

    /// The host could not set up the virtual display and streams a physical
    /// one instead: stop asking for it.
    pub fn virtual_display_failed(&mut self, msg: &str) {
        self.start.display_setup = None;
        self.notice(msg.to_owned(), Duration::from_secs(10));
    }

    pub fn notice(&mut self, msg: String, for_: Duration) {
        self.status = msg;
        self.status_until = Some(Instant::now() + for_);
    }

    pub fn mic_on(&self) -> bool {
        self.mic_stop.is_some()
    }

    /// Name of the host device that receives the microphone, if any.
    pub fn host_mic_device(&self) -> Option<&str> {
        self.info.as_ref().map(|i| i.mic_device.as_str()).filter(|n| !n.is_empty())
    }

    pub fn set_mic(&mut self, on: bool) {
        if on == self.mic_on() {
            return;
        }
        if let Some(stop) = self.mic_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if on {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            crate::mic::spawn(self.net_tx.clone(), stop.clone());
            self.mic_stop = Some(stop);
        }
    }

    pub fn on_session_info(&mut self, info: pb::SessionInfo, focused: bool) {
        // Forward local pads only when the host can create virtual ones.
        if info.gamepad_available && self.gamepads.is_none() {
            self.gamepads = Some(crate::gamepad::Gamepads::spawn(self.net_tx.clone(), focused));
        } else if !info.gamepad_available {
            self.gamepads = None;
        }
        self.info = Some(info);
        if self.mic_auto && self.host_mic_device().is_some() {
            self.mic_auto = false;
            self.set_mic(true);
        }
    }

    pub fn usb_available(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.usb_available)
    }

    pub fn usb_detach(&mut self, busid: &str) {
        self.usb_busy.insert(busid.to_owned());
        let _ = self.net_tx.send(ctl(Msg::UsbDetach(pb::UsbDetach { busid: busid.to_owned() })));
    }

    pub fn on_usb_status(&mut self, st: pb::UsbStatus) {
        self.usb_busy.remove(&st.busid);
        self.usb_state.insert(st.busid, (st.attached, st.message));
    }

    pub fn ctrl_alt_del(&self) {
        let _ = self.net_tx.send(ctl(Msg::SendSas(pb::SendSas {})));
    }

    pub fn clipboard_from_host(&self, text: String) {
        if let Some(tx) = &self.clip_tx {
            let _ = tx.send(ClipIn::Text(text));
        }
    }

    pub fn clipboard_image_from_host(&self, dib: Vec<u8>) {
        if let Some(tx) = &self.clip_tx {
            let _ = tx.send(ClipIn::Image(dib));
        }
    }

    pub fn send_files(&mut self, paths: Vec<std::path::PathBuf>) {
        if !paths.is_empty() {
            let _ = self.net_tx.send(NetCmd::SendFiles(paths));
        }
    }

    /// Download the files of a host offer.
    pub fn accept_offer(&mut self, id: u64) {
        let Some(i) = self.offers.iter().position(|o| o.transfer_id == id) else { return };
        let o = self.offers.remove(i);
        let total = o.files.iter().map(|f| f.size).sum();
        self.transfers.push(TransferView {
            id,
            upload: false,
            name: format!("{} 个文件", o.files.len()),
            done: 0,
            total,
            state: TransferState::Running,
            folder: None,
        });
        let _ = self.net_tx.send(ctl(Msg::FileRequest(pb::FileRequest { transfer_id: id, purpose: pb::FilePurpose::Save as i32 })));
    }

    pub fn dismiss_offer(&mut self, id: u64) {
        self.offers.retain(|o| o.transfer_id != id);
    }

    pub fn dismiss_transfer(&mut self, id: u64) {
        self.transfers.retain(|t| t.id != id);
    }

    /// Stop a running transfer; its row says so (later progress of it is ignored).
    pub fn cancel_transfer(&mut self, id: u64) {
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
            t.state = TransferState::Failed("已取消".into());
        }
        let _ = self.net_tx.send(NetCmd::CancelTransfer(id));
    }

    /// The host copied files: they are on our clipboard now.
    pub fn on_clip_offer(&mut self, o: pb::FileOffer) {
        let Some(tx) = &self.clip_tx else { return self.on_offer(o) };
        let _ = tx.send(ClipIn::Offer(o.transfer_id, crate::clipboard::virtual_files(&o.files)));
        let top = o.files.iter().filter(|f| !f.path.contains('/')).count().max(1);
        self.notice(format!("被控端复制了 {top} 项，可以在本机粘贴"), Duration::from_secs(4));
    }

    pub fn on_offer(&mut self, o: pb::FileOffer) {
        self.offers.retain(|x| x.transfer_id != o.transfer_id);
        self.offers.push(o);
        // Only the latest few matter.
        while self.offers.len() > 3 {
            self.offers.remove(0);
        }
    }

    pub fn on_transfer(&mut self, u: TransferUpdate) {
        let t = match self.transfers.iter_mut().find(|t| t.id == u.id) {
            // Cancelled here: what the stopping tasks report changes nothing.
            Some(t) if matches!(&t.state, TransferState::Failed(m) if m == "已取消") => return,
            Some(t) => t,
            None => {
                self.transfers.push(TransferView {
                    id: u.id,
                    upload: u.upload,
                    name: String::new(),
                    done: 0,
                    total: u.total,
                    state: TransferState::Running,
                    folder: None,
                });
                self.transfers.last_mut().unwrap()
            }
        };
        if !u.name.is_empty() {
            t.name = u.name;
        }
        t.done = t.done.max(u.done);
        if u.total > 0 {
            t.total = u.total;
        }
        if u.folder.is_some() {
            t.folder = u.folder;
        }
        match u.finished {
            Some(Ok(m)) => {
                t.state = TransferState::Done(m);
                t.done = t.total.max(t.done);
            }
            Some(Err(m)) => t.state = TransferState::Failed(m),
            None => {}
        }
    }

    /// The host's verdict on an upload (or a failed download).
    pub fn on_file_result(&mut self, r: pb::FileResult) {
        let state = if r.ok {
            TransferState::Done(if r.saved_to.is_empty() { r.message.clone() } else { format!("{}，位置：{}", r.message, r.saved_to) })
        } else {
            TransferState::Failed(r.message.clone())
        };
        match self.transfers.iter_mut().find(|t| t.id == r.transfer_id) {
            Some(t) if matches!(&t.state, TransferState::Failed(m) if m == "已取消") => {}
            Some(t) => {
                t.state = state;
                if r.ok {
                    t.done = t.total.max(t.done);
                }
            }
            None => self.transfers.push(TransferView {
                id: r.transfer_id,
                upload: true,
                name: String::new(),
                done: 0,
                total: 0,
                state,
                folder: None,
            }),
        }
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        if let Some(s) = &self.current {
            return Some((s.width, s.height));
        }
        let c = self.stream.as_ref()?.config.clone()?;
        Some((c.width, c.height))
    }

    /// Per-second bookkeeping: statistics, periodic diagnostics.
    pub fn tick(&mut self) -> bool {
        let secs = self.last_tick.elapsed().as_secs_f32();
        if secs < 1.0 {
            return false;
        }
        if self.status_until.is_some_and(|t| Instant::now() >= t) {
            self.status_until = None;
            self.status.clear();
        }
        if self.stream.is_some() && self.status_log.elapsed() >= Duration::from_secs(5) {
            self.status_log = Instant::now();
            let (f, b, d, r) =
                self.stats.with(|s| (s.total_rx_frames, s.total_rx_bytes, s.total_decoded, s.total_rendered));
            if r == self.last_rendered_total {
                tracing::warn!("no new picture in 5 s: received {f} frames / {} KB, decoded {d}, rendered {r}", b / 1024);
            }
            self.last_rendered_total = r;
            let keys = (input::hook_key_count(), self.winit_keys);
            if keys != self.logged_keys {
                let (reinstalls, missed, own) = input::hook_repairs();
                tracing::info!(
                    "keys so far: hook {} (hook calls {}, {own} with a session window in front) / window {} (grab {}, past the hook {missed}, hook put first again {reinstalls}x)",
                    keys.0,
                    input::hook_call_count(),
                    keys.1,
                    input::grabbed()
                );
                self.logged_keys = keys;
            }
        }
        self.last_tick = Instant::now();
        self.summary = self.stats.take_summary(secs);
        for v in self.views.values_mut() {
            v.summary = v.stats.take_summary(secs);
        }
        let s = &self.summary;
        let _ = self.net_tx.send(ctl(Msg::ClientStats(pb::ClientStats {
            decode_ms_p50: s.decode_ms,
            render_ms_p50: s.render_ms,
            frames_dropped: s.dropped,
            fps: s.fps,
            video_shards_received: s.dgram.shards_received,
            video_shards_lost: s.dgram.shards_lost,
            video_frames_recovered: s.dgram.frames_recovered,
            video_frames_lost: s.dgram.frames_lost,
        })));
        true
    }

}

/// Statistics lines of one stream.
fn stream_lines(lines: &mut Vec<String>, stream: Option<&pb::StreamStarted>, server: Option<&pb::ServerStats>, s: &Summary, main: bool) {
    if let Some(st) = stream {
        let c = st.config.unwrap_or_default();
        lines.push(format!(
            "编码 {} {} {}x{}@{}{}",
            st.encoder_name,
            if c.chroma == pb::Chroma::Yuv444 as i32 { "4:4:4" } else { "4:2:0" },
            c.width,
            c.height,
            c.fps,
            if st.cross_gpu { format!("  跨显卡 [{}]→[{}]", st.capture_gpu_index, st.encode_gpu_index) } else { String::new() }
        ));
        if st.hdr_tonemapped {
            lines.push("HDR  被控端显示器开启了 HDR，已转换为 SDR 传输".into());
        }
        if c.hdr {
            lines.push("HDR  HDR10 直通（HEVC 10 bit，BT.2020 PQ）".into());
        }
    }
    let (sfps, skbps, enc_ms, enc_p99, xfer_ms, target, note) = server
        .map(|x| (x.fps, x.bitrate_kbps, x.encode_ms_p50, x.encode_ms_p99, x.transfer_ms_p50, x.target_kbps, x.bitrate_note.clone()))
        .unwrap_or_default();
    // Older hosts don't report the 99th percentile.
    let p99 = |v: f32| if v > 0.0 { format!("{v:.1}") } else { "—".into() };
    lines.push(format!("帧率  被控端 {sfps} / 本机 {}   丢帧 {}", s.fps, s.dropped));
    let kbps = if main { skbps.max(s.kbps) } else { skbps };
    lines.push(format!("码率  实际 {:.1} Mbps   上限 {:.1} Mbps", kbps as f32 / 1000.0, target as f32 / 1000.0));
    if main && !note.is_empty() {
        lines.push(format!("策略  {note}"));
    }
    let fec = server.map(|x| x.fec_percent).unwrap_or(0);
    if main && fec > 0 {
        let d = &s.dgram;
        lines.push(format!(
            "传输  数据报 + 纠错 {fec}%   丢包 {:.1}%   纠错恢复 {} 帧   丢帧 {}",
            d.loss_pct(),
            d.frames_recovered,
            d.frames_lost
        ));
    }
    let latency = format!("延迟  端到端 {:.1} ms（P99 {:.1}）", s.latency_ms, s.latency_p99);
    if main {
        lines.push(format!("{latency}   RTT {:.1} ms", s.rtt_ms));
    } else {
        lines.push(latency);
    }
    lines.push(format!(
        "耗时（中位/P99）  编码 {enc_ms:.1}/{}  解码 {:.1}/{:.1}  渲染 {:.1}/{:.1} ms",
        p99(enc_p99),
        s.decode_ms,
        s.decode_p99,
        s.render_ms,
        s.render_p99
    ));
    if xfer_ms > 0.0 {
        lines.push(format!("耗时  跨显卡传输 {xfer_ms:.1} ms"));
    }
    lines.push(format!("解码器  {}", s.decoder));
    if let Some(a) = s.audio.filter(|_| main) {
        let speed = if a.speed != 1.0 { format!("  调速 {:+.2}%", (a.speed - 1.0) * 100.0) } else { String::new() };
        lines.push(format!(
            "声音  缓冲 {:.0}/{:.0} ms  抖动 {:.0} ms{speed}  断音 {}  丢弃 {} ms",
            a.level_ms, a.target_ms, a.jitter_ms, a.underruns, a.dropped_ms
        ));
    }
}

impl Session {
    /// Lines for the statistics window: the main window's stream, then one
    /// block per extra window.
    pub fn stats_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let many = !self.views.is_empty();
        if many {
            let title = self.stream.as_ref().map(|s| self.display_title(s.display_id)).unwrap_or_else(|| "主窗口".into());
            lines.push(format!("── 主窗口 · {title} ──"));
        }
        stream_lines(&mut lines, self.stream.as_ref(), self.server_stats.as_ref(), &self.summary, true);
        if let Some(tcp) = self.via_tcp {
            let path = self
                .server_stats
                .as_ref()
                .map(|st| format!("，丢包 {:.1}%，往返 {:.0} ms", st.path_loss_pct, st.path_rtt_ms))
                .unwrap_or_default();
            let mode = match self.transport {
                crate::net::Transport::Auto => "自动",
                crate::net::Transport::Udp => "仅 UDP",
                crate::net::Transport::Tcp => "仅 TCP",
            };
            lines.push(format!("连接：{}（{mode}）{path}", if tcp { "TCP" } else { "UDP" }));
        }
        for v in self.views.values() {
            lines.push(String::new());
            lines.push(format!("── 窗口 · {} ──", self.display_title(v.display_id)));
            if !v.status.is_empty() {
                lines.push(v.status.clone());
            }
            stream_lines(&mut lines, v.stream.as_ref(), v.server_stats.as_ref(), &v.summary, false);
        }
        lines
    }

    pub fn quit(&self) {
        let _ = self.net_tx.send(NetCmd::Quit);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.set_mic(false);
        input::end_session(&self.net_tx);
        let _ = self.net_tx.send(NetCmd::Quit);
    }
}
