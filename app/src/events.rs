//! Messages between the network task, worker threads and the UI thread.

use nya_proto::pb;
use winit::event_loop::EventLoopProxy;

use crate::net::Link;

/// Commands for the network task.
pub enum NetCmd {
    Input(pb::InputMsg),
    Control(pb::ControlMsg),
    /// Upload files to the host's Downloads folder.
    SendFiles(Vec<std::path::PathBuf>),
    /// Local clipboard image (CF_DIB) for the host.
    SendImage(Vec<u8>),
    /// Files were copied here: offer them to the host.
    OfferFiles(Vec<std::path::PathBuf>),
    /// The host's files (offer id) are being pasted here: fetch them.
    ClipboardPaste(u64, crate::transfer::PasteReply),
    /// Stop a transfer (both sides), dropping what was received of it.
    CancelTransfer(u64),
    /// Connection mode changed ("auto" | "udp" | "tcp"): reconnect if needed.
    SetTransport(crate::net::Transport),
    /// Encoded MIC datagram.
    Mic(Vec<u8>),
    Quit,
}

/// Progress / outcome of one file transfer batch.
#[derive(Debug, Clone)]
pub struct TransferUpdate {
    pub id: u64,
    pub upload: bool,
    /// Current file name (or a summary).
    pub name: String,
    pub done: u64,
    pub total: u64,
    /// Some(Ok(message)) / Some(Err(message)) when finished.
    pub finished: Option<Result<String, String>>,
    /// Folder the files ended up in (downloads).
    pub folder: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    ToggleGrab,
    ToggleStats,
    ToggleMode,
    ToggleRelative,
    ToggleFullscreen,
    CtrlAltDel,
    ToggleToolbar,
    Display(u8),
    Quit,
}

/// Result of a connection attempt started from the launcher.
pub struct ConnectDone {
    pub attempt: u64,
    pub result: Result<Box<Link>, String>,
    /// The failure was a changed server certificate.
    pub pin_mismatch: bool,
    /// A pairing link: the address that answered.
    pub address: Option<String>,
}

/// Events delivered to the winit event loop.
pub enum UiEvent {
    /// The tray icon (or another start of the program) asks for something.
    Tray(crate::tray::TrayAction),
    /// A `nyaremote://` link was opened (this start of the program, or another one).
    OpenLink(String),
    Connected,
    SessionInfo(pb::SessionInfo),
    /// Who operates the host (several clients connected).
    Role(pb::SessionRole),
    GamepadRumble(pb::GamepadRumble),
    StreamStarted(pb::StreamStarted),
    /// (slot, message)
    StreamError(u32, String),
    Cursor(pb::CursorMsg),
    ServerStats(pb::ServerStats),
    Clipboard(String),
    /// A new decoded frame is ready in the frame store of this slot.
    Frame(u32),
    Reconnecting(String),
    /// The session's connection: over TCP (QUIC over TCP) or UDP.
    Transport { tcp: bool },
    Disconnected(String),
    Hotkey(Hotkey),
    ConnectDone(ConnectDone),
    FileOffer(pb::FileOffer),
    /// The host copied files; they are on our clipboard now (paste to fetch).
    ClipOffer(pb::FileOffer),
    FileResult(pb::FileResult),
    Transfer(TransferUpdate),
    UsbStatus(pb::UsbStatus),
    /// What became of our shared folders on the host.
    FolderMount(pb::FolderMountStatus),
    /// A print job from the host, saved here.
    PrintJob(std::path::PathBuf),
    /// How a print job went (shown in the session window).
    PrintDone(String),
    /// (usbipd installed, devices).
    UsbDevices(bool, Result<Vec<crate::usb::UsbDevice>, String>),
    /// usbipd-win install progress: (still running, message)
    UsbipdInstall(bool, String),
    /// Clipboard image from the host.
    ClipboardImage(Vec<u8>),
    /// The host wants a pairing code; answer through the sender (None = cancel).
    NeedPairing(std::sync::mpsc::Sender<Option<String>>),
    /// Hardware decoding summary of this computer (worker thread).
    DecodeSummary(String),
    /// Newest client release (update check).
    UpdateChecked(Result<nya_win::update::Release, String>),
    /// Installer download progress, percent.
    UpdateProgress(u32),
    UpdateDownloaded(Result<std::path::PathBuf, String>),
    /// A request from the launcher page.
    Web(nya_webui::Call),
    /// Late answer to a launcher request (work done on another thread).
    WebReply(u64, Result<serde_json::Value, String>),
    /// Background work of the "本机" section finished.
    Host(crate::app::host::HostEvent),
    /// An event of one connection (session window), see `Ui::for_conn`.
    Conn(u64, Box<UiEvent>),
}

#[derive(Clone)]
pub struct Ui {
    proxy: EventLoopProxy<UiEvent>,
    /// Connection the events belong to (0 = the app as a whole).
    conn: u64,
}

impl Ui {
    pub fn new(proxy: EventLoopProxy<UiEvent>) -> Self {
        Self { proxy, conn: 0 }
    }

    /// A sender whose events are delivered to connection `conn`.
    pub fn for_conn(&self, conn: u64) -> Self {
        Self { proxy: self.proxy.clone(), conn }
    }

    pub fn send(&self, ev: UiEvent) {
        let ev = if self.conn == 0 { ev } else { UiEvent::Conn(self.conn, Box::new(ev)) };
        let _ = self.proxy.send_event(ev);
    }
}
