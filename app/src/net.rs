//! Connection, handshake, pairing, and the long-running session with
//! automatic reconnection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Sender;
use nya_proto::frame::{datagram_type, stream_type, AudioPacket, VideoFrameHeader};
use nya_proto::framing::{encode_varint, expect_msg, read_msg, read_varint, write_msg};
use nya_proto::negotiate::{self, LocalVersion, Negotiated};
use nya_proto::pb::{self, control_msg::Msg, Feature};
use nya_proto::{MAX_MESSAGE_LEN, MAX_VIDEO_FRAME_LEN};
use nya_transport::identity::peer_fingerprint;
use nya_transport::pairing::{self, PairingKey, Transcript};
use nya_transport::quinn::{Connection, Endpoint, RecvStream, SendStream};
use nya_transport::{Fingerprint, Identity};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::events::{NetCmd, Ui, UiEvent};
use crate::stats::Shared;
use crate::video::VideoIn;

pub type PairPrompt = Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub struct Link {
    pub endpoint: Endpoint,
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub neg: Negotiated,
    pub welcome: pb::Welcome,
    pub server_fp: Fingerprint,
    /// QUIC over TCP (else UDP).
    pub via_tcp: bool,
}

/// How a session travels (setting `transport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// UDP; TCP when UDP does not connect or loses too much, back to UDP
    /// when it works again (see [`supervise`]).
    Auto,
    Udp,
    /// QUIC over TCP (nya_transport::tcptunnel).
    Tcp,
}

impl Transport {
    pub fn parse(s: &str) -> Self {
        match s {
            "udp" => Self::Udp,
            "tcp" => Self::Tcp,
            _ => Self::Auto,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

/// Auto: how long the preferred transport may try alone before the other one starts too.
const HEAD_START: Duration = Duration::from_millis(2500);
/// Auto: host-reported loss on UDP that counts as bad, and for how long it
/// has to last before the session moves to TCP.
const BAD_LOSS_PCT: f32 = 10.0;
const BAD_FOR: Duration = Duration::from_secs(20);
/// Auto, on TCP: how often UDP is tried again.
const UDP_PROBE_EVERY: Duration = Duration::from_secs(300);
/// Auto: after moving to TCP for loss, stay this long (doubling each time, up to an hour).
pub const FIRST_TCP_HOLD: Duration = Duration::from_secs(600);

type Opened = (Endpoint, Connection, bool);

async fn open_one(addr: SocketAddr, id: Identity, pinned: Option<Fingerprint>, tcp: bool) -> Result<Opened> {
    let endpoint = if tcp {
        nya_transport::tcptunnel::client_endpoint(addr).await?
    } else {
        nya_transport::endpoint::client_endpoint(addr)?
    };
    let conn = timeout(Duration::from_secs(8), nya_transport::endpoint::connect(&endpoint, addr, &id, pinned))
        .await
        .map_err(|_| anyhow!("{} 连接超时", if tcp { "TCP" } else { "UDP" }))??;
    Ok((endpoint, conn, tcp))
}

/// The QUIC connection: over UDP, over TCP, or (auto) the preferred one with
/// a head start and then both at once — the first that connects is used.
async fn open(addr: SocketAddr, id: &Identity, pinned: Option<Fingerprint>, mode: Transport, prefer_tcp: bool) -> Result<Opened> {
    let first_tcp = match mode {
        Transport::Udp => return open_one(addr, id.clone(), pinned, false).await,
        Transport::Tcp => return open_one(addr, id.clone(), pinned, true).await,
        Transport::Auto => prefer_tcp,
    };
    let a = open_one(addr, id.clone(), pinned, first_tcp);
    tokio::pin!(a);
    let mut err_a = None;
    tokio::select! {
        r = &mut a => match r {
            Ok(x) => return Ok(x),
            Err(e) => err_a = Some(e),
        },
        _ = tokio::time::sleep(HEAD_START) => {}
    }
    let b = open_one(addr, id.clone(), pinned, !first_tcp);
    tokio::pin!(b);
    let mut err_b = None;
    loop {
        tokio::select! {
            r = &mut a, if err_a.is_none() => match r {
                Ok(x) => return Ok(x),
                Err(e) => err_a = Some(e),
            },
            r = &mut b, if err_b.is_none() => match r {
                Ok(x) => return Ok(x),
                Err(e) => err_b = Some(e),
            },
        }
        if let (Some(ea), Some(eb)) = (&err_a, &err_b) {
            let (udp, tcp) = if first_tcp { (eb, ea) } else { (ea, eb) };
            bail!("UDP：{udp:#}；TCP：{tcp:#}（检查组网是否连通、被控端是否运行、防火墙和端口转发的 UDP / TCP 端口）");
        }
    }
}

fn ctl(m: Msg) -> pb::ControlMsg {
    pb::ControlMsg { msg: Some(m) }
}

/// Connect and complete the handshake (and pairing if the host asks for it).
pub async fn connect(
    addr: SocketAddr,
    id: &Identity,
    pinned: Option<Fingerprint>,
    client_name: &str,
    prompt: Option<PairPrompt>,
    mode: Transport,
    prefer_tcp: bool,
) -> Result<Link> {
    let opened = open(addr, id, pinned, mode, prefer_tcp).await.with_context(|| format!("连接 {addr} 失败"))?;
    handshake(addr, opened, id, pinned, client_name, prompt).await
}

/// A pairing link: try all its addresses at once; the first that answers
/// (with the link's certificate, when it has one) is used. Returns that address.
pub async fn connect_any(
    targets: &[String],
    id: &Identity,
    pinned: Option<Fingerprint>,
    client_name: &str,
    prompt: Option<PairPrompt>,
    mode: Transport,
) -> Result<(String, Link)> {
    let mut set = tokio::task::JoinSet::new();
    for (i, t) in targets.iter().enumerate() {
        let (t, id) = (t.clone(), id.clone());
        set.spawn(async move {
            let r = async {
                let addr = tokio::task::spawn_blocking(move || nya_transport::endpoint::resolve(&t, nya_proto::DEFAULT_PORT)).await??;
                Ok::<_, anyhow::Error>((addr, open(addr, &id, pinned, mode, false).await?))
            }
            .await;
            (i, r)
        });
    }
    let mut errors = Vec::new();
    while let Some(r) = set.join_next().await {
        let Ok((i, r)) = r else { continue };
        match r {
            Ok((addr, opened)) => {
                set.abort_all();
                tracing::info!("pairing link: {} answered", targets[i]);
                let link = handshake(addr, opened, id, pinned, client_name, prompt).await?;
                return Ok((targets[i].clone(), link));
            }
            Err(e) => errors.push(format!("{}：{e:#}", targets[i])),
        }
    }
    bail!("链接里的地址都连不上（{}）", errors.join("；"))
}

/// The session handshake on an opened connection (and pairing if the host asks for it).
async fn handshake(
    addr: SocketAddr,
    (endpoint, conn, via_tcp): Opened,
    id: &Identity,
    pinned: Option<Fingerprint>,
    client_name: &str,
    prompt: Option<PairPrompt>,
) -> Result<Link> {
    tracing::info!("connected to {addr} over {}", if via_tcp { "TCP" } else { "UDP" });
    let server_fp = peer_fingerprint(&conn).ok_or_else(|| anyhow!("被控端没有证书"))?;
    let (mut send, mut recv) = conn.open_bi().await?;

    let me = LocalVersion::current();
    write_msg(&mut send, &negotiate::hello(&me, client_name, env!("CARGO_PKG_VERSION"))).await?;
    let reply: pb::HelloReply = timeout(Duration::from_secs(10), expect_msg(&mut recv, MAX_MESSAGE_LEN))
        .await
        .context("等待被控端响应超时")??;
    let welcome = match reply.reply {
        Some(pb::hello_reply::Reply::Welcome(w)) => w,
        Some(pb::hello_reply::Reply::Reject(r)) => bail!("被控端拒绝连接：{}", r.message),
        None => bail!("被控端响应无法识别（版本差异过大？）"),
    };
    let neg = negotiate::accept_welcome(&welcome, &me).map_err(|e| anyhow!(e))?;

    if welcome.needs_pairing {
        let challenge: pb::ControlMsg = expect_msg(&mut recv, MAX_MESSAGE_LEN).await?;
        let Some(Msg::AuthChallenge(ch)) = challenge.msg else { bail!("expected AuthChallenge") };
        let Some(prompt) = prompt else {
            bail!("被控端要求重新配对（可能被控端重装或移除了本机），请重新运行并输入配对码");
        };
        let code = tokio::task::spawn_blocking(move || prompt()).await?.ok_or_else(|| anyhow!("已取消配对"))?;
        let key = PairingKey::from_code(&code).ok_or_else(|| anyhow!("配对码格式不正确"))?;
        let client_nonce = pairing::nonce();
        let t = Transcript {
            server_nonce: &ch.server_nonce,
            client_nonce: &client_nonce,
            server_fp,
            client_fp: id.fingerprint(),
        };
        write_msg(&mut send, &ctl(Msg::AuthResponse(pb::AuthResponse { client_nonce: client_nonce.to_vec(), mac: t.client_mac(&key) })))
            .await?;
        let res: pb::ControlMsg = expect_msg(&mut recv, MAX_MESSAGE_LEN).await?;
        let Some(Msg::AuthResult(r)) = res.msg else { bail!("expected AuthResult") };
        if !r.ok {
            bail!("配对失败：{}", r.message);
        }
        if !t.verify_server(&key, &r.server_mac) {
            bail!("被控端无法证明它知道配对码，已中止（可能存在中间人）");
        }
    } else if pinned.is_none() {
        tracing::warn!("被控端已认识本机但本机没有保存它的指纹；将信任并保存 {server_fp}");
    }
    Ok(Link { endpoint, conn, send, recv, neg, welcome, server_fp, via_tcp })
}

/// Everything needed to (re)start the session.
/// Decoder input of every open window, by stream slot.
#[derive(Default)]
pub struct VideoRoutes(std::sync::Mutex<std::collections::HashMap<u32, Sender<VideoIn>>>);

impl VideoRoutes {
    pub fn set(&self, slot: u32, tx: Option<Sender<VideoIn>>) {
        let mut m = self.0.lock().unwrap();
        match tx {
            Some(tx) => {
                m.insert(slot, tx);
            }
            None => {
                m.remove(&slot);
            }
        }
    }

    fn get(&self, slot: u32) -> Option<Sender<VideoIn>> {
        self.0.lock().unwrap().get(&slot).cloned()
    }

    /// Reconnect: every decoder waits for a new keyframe.
    fn reset_all(&self) {
        for tx in self.0.lock().unwrap().values() {
            let _ = tx.send(VideoIn::Reset);
        }
    }
}

pub struct Params {
    pub addr: SocketAddr,
    pub pinned: Fingerprint,
    pub identity: Identity,
    pub name: String,
    pub caps: pb::ClientCaps,
    pub start: pb::StartStream,
    /// Streams of extra windows (slot > 0), replayed after a reconnect.
    pub extra: std::collections::BTreeMap<u32, pb::StartStream>,
    /// Folders shown on the host as a drive (FEATURE_FOLDER_MOUNT).
    pub shares: Arc<nya_transport::folders::Shares>,
    /// Connection mode (setting `transport`).
    pub transport: Transport,
    /// Auto: try TCP first until then (moved to TCP for loss).
    pub prefer_tcp_until: Option<Instant>,
    /// Auto: how long the next move to TCP for loss lasts.
    pub tcp_hold: Duration,
}

pub struct Sinks {
    pub ui: Ui,
    pub video: Arc<VideoRoutes>,
    pub audio: Sender<AudioPacket>,
    pub stats: Arc<Shared>,
    pub clip: Arc<crate::transfer::ClipFiles>,
}

enum End {
    UserQuit,
    Fatal(String),
    Lost(String),
    /// Reconnect over TCP (`true`) or UDP, for this reason.
    Switch(bool, String),
}

/// Run the session; reconnect for up to two minutes when the link drops.
pub async fn supervise(first: Link, mut p: Params, mut cmds: mpsc::UnboundedReceiver<NetCmd>, sinks: Sinks) {
    let mut link = Some(first);
    let mut lost_since: Option<Instant> = None;
    // The next connection's transport when switching (else the setting).
    let mut next: Option<Transport> = None;
    loop {
        let l = match link.take() {
            Some(l) => l,
            None => match connect(
                p.addr,
                &p.identity,
                Some(p.pinned),
                &p.name,
                None,
                next.take().unwrap_or(p.transport),
                p.prefer_tcp_until.is_some_and(|t| Instant::now() < t),
            )
            .await
            {
                Ok(l) => l,
                Err(e) => {
                    let since = *lost_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_secs(120) {
                        sinks.ui.send(UiEvent::Disconnected(format!("无法重新连接：{e:#}")));
                        return;
                    }
                    sinks.ui.send(UiEvent::Reconnecting(format!("{e:#}")));
                    // Drain commands meanwhile; honour Quit.
                    let deadline = tokio::time::sleep(Duration::from_secs(2));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            c = cmds.recv() => match c {
                                Some(NetCmd::Quit) | None => return,
                                Some(NetCmd::Control(m)) => track(&mut p, &m),
                                _ => {}
                            }
                        }
                    }
                    continue;
                }
            },
        };
        lost_since = None;
        sinks.video.reset_all();
        match run(l, &mut p, &mut cmds, &sinks).await {
            End::UserQuit => return,
            End::Fatal(msg) => {
                sinks.ui.send(UiEvent::Disconnected(msg));
                return;
            }
            End::Lost(msg) => {
                tracing::warn!("connection lost: {msg}");
                sinks.ui.send(UiEvent::Reconnecting(msg));
            }
            End::Switch(tcp, why) => {
                tracing::info!("switching to {}: {why}", if tcp { "TCP" } else { "UDP" });
                if tcp && p.transport == Transport::Auto {
                    p.prefer_tcp_until = Some(Instant::now() + p.tcp_hold);
                    p.tcp_hold = (p.tcp_hold * 2).min(Duration::from_secs(3600));
                } else if !tcp {
                    p.prefer_tcp_until = None;
                }
                next = Some(if tcp { Transport::Tcp } else { Transport::Udp });
                sinks.ui.send(UiEvent::Reconnecting(format!("{why}，正在改用 {}", if tcp { "TCP" } else { "UDP" })));
            }
        }
    }
}

/// Keep the replayable state current.
fn track(p: &mut Params, m: &pb::ControlMsg) {
    match &m.msg {
        Some(Msg::StartStream(s)) if s.slot != 0 => {
            p.extra.insert(s.slot, s.clone());
        }
        Some(Msg::StopStream(s)) if s.slot != 0 => {
            p.extra.remove(&s.slot);
        }
        Some(Msg::StartStream(s)) => p.start = s.clone(),
        Some(Msg::SetMode(m)) => {
            p.start.config.get_or_insert_with(Default::default).mode = m.mode;
            for s in p.extra.values_mut() {
                s.config.get_or_insert_with(Default::default).mode = m.mode;
            }
        }
        Some(Msg::ClientCaps(c)) => p.caps = c.clone(),
        _ => {}
    }
}

async fn run(link: Link, p: &mut Params, cmds: &mut mpsc::UnboundedReceiver<NetCmd>, sinks: &Sinks) -> End {
    let Link { endpoint: _endpoint, conn, mut send, mut recv, neg, via_tcp, .. } = link;
    sinks.ui.send(UiEvent::Connected);
    sinks.ui.send(UiEvent::Transport { tcp: via_tcp });
    // Auto: since when the host has reported bad loss on UDP.
    let mut bad_since: Option<Instant> = None;
    // Auto, on TCP: try UDP again now and then.
    let mut udp_probe = tokio::time::interval_at(tokio::time::Instant::now() + UDP_PROBE_EVERY, UDP_PROBE_EVERY);
    let mut probe: Option<tokio::task::JoinHandle<bool>> = None;

    let setup = async {
        write_msg(&mut send, &ctl(Msg::ClientCaps(p.caps.clone()))).await?;
        write_msg(&mut send, &ctl(Msg::StartStream(p.start.clone()))).await?;
        for s in p.extra.values() {
            write_msg(&mut send, &ctl(Msg::StartStream(s.clone()))).await?;
        }
        if neg.has(Feature::FolderMount) && !p.shares.0.is_empty() {
            write_msg(&mut send, &ctl(Msg::SharedFolders(p.shares.to_pb()))).await?;
        }
        let mut input = conn.open_uni().await?;
        input.set_priority(20)?;
        let mut prelude = Vec::new();
        encode_varint(stream_type::INPUT, &mut prelude);
        input.write_all(&prelude).await?;
        anyhow::Ok(input)
    };
    let mut input = match setup.await {
        Ok(i) => i,
        Err(e) => return End::Lost(format!("{e:#}")),
    };

    let files_on = neg.has(Feature::FileTransfer);
    let images_on = neg.has(Feature::ClipboardImage);
    // Copy on one side, paste on the other (folders too), both directions.
    let clip_on = files_on && neg.has(Feature::ClipboardFiles);
    let file_flags = (files_on, images_on, clip_on, neg.has(Feature::Print));
    let downloads = Arc::new(crate::transfer::Downloads::default());
    // Where files go: the TCP file channel once the host offered it and it
    // is up (FEATURE_TCP_FILES), FILE streams on this connection until then.
    let files_link = nya_transport::files::FileLink::new(conn.clone());
    let uni = tokio::spawn(accept_uni(
        conn.clone(),
        sinks.video.clone(),
        sinks.ui.clone(),
        sinks.stats.clone(),
        sinks.clip.clone(),
        downloads.clone(),
        files_link.cancels().clone(),
        file_flags,
        neg.has(Feature::MultiStream),
    ));
    let mut file_channel_task: Option<tokio::task::JoinHandle<()>> = None;
    // Control messages from spawned tasks (failed clipboard sends).
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<pb::ControlMsg>();
    let usb_on = neg.has(Feature::UsbRedirect);
    // The host reads our shared folders only if we shared some.
    let shares = (neg.has(Feature::FolderMount) && !p.shares.0.is_empty()).then(|| p.shares.clone());
    let bidi = tokio::spawn({
        let conn = conn.clone();
        async move {
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let shares = shares.clone();
                tokio::spawn(async move {
                    match read_varint(&mut recv).await {
                        Ok(Some(stream_type::TUNNEL)) if usb_on => {
                            let Ok(Some(port)) = read_varint(&mut recv).await else { return };
                            if let Err(e) = crate::usb::tunnel(send, recv, port).await {
                                tracing::debug!("usb tunnel: {e:#}");
                            }
                        }
                        Ok(Some(stream_type::FS)) if shares.is_some() => {
                            if let Err(e) = nya_transport::folders::serve_stream(send, recv, shares.unwrap()).await {
                                tracing::debug!("folder request: {e:#}");
                            }
                        }
                        _ => {
                            let _ = recv.stop(0u32.into());
                        }
                    }
                });
            }
        }
    });
    let dgram = tokio::spawn(read_datagrams(
        conn.clone(),
        sinks.audio.clone(),
        neg.has(Feature::Audio),
        sinks.video.clone(),
        sinks.stats.clone(),
        neg.has(Feature::VideoDatagram),
    ));
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let clipboard = neg.has(Feature::ClipboardText);

    let end = loop {
        tokio::select! {
            m = read_msg::<pb::ControlMsg, _>(&mut recv, MAX_MESSAGE_LEN) => {
                let m = match m {
                    Ok(Some(m)) => m,
                    Ok(None) => break End::Lost("被控端关闭了连接".into()),
                    Err(e) => break End::Lost(format!("{e}")),
                };
                match m.msg {
                    Some(Msg::SessionInfo(i)) => sinks.ui.send(UiEvent::SessionInfo(i)),
                    Some(Msg::SessionRole(r)) => sinks.ui.send(UiEvent::Role(r)),
                    // The decoder notices the new stream id itself; frames may arrive first.
                    Some(Msg::StreamStarted(s)) => sinks.ui.send(UiEvent::StreamStarted(s)),
                    Some(Msg::StreamError(e)) => sinks.ui.send(UiEvent::StreamError(e.slot, e.message)),
                    Some(Msg::DisplayChanged(d)) => tracing::info!("host displays changed: {} displays", d.displays.len()),
                    Some(Msg::ServerStats(s)) => {
                        if p.transport == Transport::Auto && !via_tcp && s.slot == 0 {
                            if s.path_loss_pct >= BAD_LOSS_PCT {
                                let since = *bad_since.get_or_insert_with(Instant::now);
                                if since.elapsed() >= BAD_FOR {
                                    break End::Switch(true, format!("UDP 丢包严重（{:.0}%）", s.path_loss_pct));
                                }
                            } else {
                                bad_since = None;
                            }
                        }
                        sinks.ui.send(UiEvent::ServerStats(s));
                    }
                    Some(Msg::ClipboardText(c)) if clipboard => sinks.ui.send(UiEvent::Clipboard(c.text)),
                    Some(Msg::Pong(p)) => sinks.stats.on_pong(p.t_us, p.server_t_us),
                    Some(Msg::FileOffer(o)) if clip_on => {
                        tracing::info!("host copied {} item(s) (offer {:016x})", o.files.len(), o.transfer_id);
                        sinks.clip.register(&o);
                        sinks.ui.send(UiEvent::ClipOffer(o));
                    }
                    Some(Msg::FileOffer(o)) if files_on => sinks.ui.send(UiEvent::FileOffer(o)),
                    Some(Msg::FileRequest(req)) if clip_on => {
                        let items = sinks.clip.outgoing.lock().unwrap().items(req.transfer_id);
                        match items {
                            Some(items) => {
                                tracing::info!("host is pasting our files (offer {:016x})", req.transfer_id);
                                tokio::spawn(crate::transfer::send_clipboard_files(files_link.clone(), req.transfer_id, items, sinks.ui.clone(), internal_tx.clone()));
                            }
                            None => {
                                let r = pb::FileResult { transfer_id: req.transfer_id, ok: false, message: "这批文件已过期，请在客户端重新复制".into(), saved_to: String::new() };
                                let _ = internal_tx.send(ctl(Msg::FileResult(r)));
                            }
                        }
                    }
                    Some(Msg::FileResult(r)) => {
                        if !r.ok {
                            if let Some(Err(e)) = sinks.clip.incoming.fail(r.transfer_id, r.message.clone()) {
                                sinks.clip.finish(r.transfer_id, Err(e));
                            }
                        }
                        sinks.ui.send(UiEvent::FileResult(r));
                    }
                    Some(Msg::UsbStatus(u)) => sinks.ui.send(UiEvent::UsbStatus(u)),
                    Some(Msg::FolderMountStatus(s)) => sinks.ui.send(UiEvent::FolderMount(s)),
                    Some(Msg::FileCancel(c)) => {
                        tracing::info!("the host cancelled transfer {:016x}", c.transfer_id);
                        cancel_transfer(&files_link, &sinks.clip, c.transfer_id, "被控端取消了传输");
                    }
                    Some(Msg::FileChannel(fc)) if neg.has(Feature::TcpFiles) => {
                        // Same address and port as QUIC (a port forward needs both).
                        let addr = conn.remote_address();
                        let (identity, pinned, link) = (p.identity.clone(), p.pinned, files_link.clone());
                        let (ui, downloads, clip, cancels) = (sinks.ui.clone(), downloads.clone(), sinks.clip.clone(), files_link.cancels().clone());
                        if let Some(t) = file_channel_task.take() {
                            t.abort();
                        }
                        file_channel_task = Some(tokio::spawn(async move {
                            let on_file: nya_transport::filechan::OnFile = Arc::new(move |h, mut r| {
                                let (ui, downloads, clip, cancels) = (ui.clone(), downloads.clone(), clip.clone(), cancels.clone());
                                tokio::spawn(async move { crate::transfer::receive_body(h, &mut r, ui, downloads, clip, cancels, file_flags).await });
                            });
                            match nya_transport::filechan::connect(addr, &identity, pinned, &fc.token, on_file).await {
                                Ok(ch) => {
                                    tracing::info!("files go over the TCP file channel ({addr})");
                                    link.set_tcp(Some(ch));
                                }
                                Err(e) => tracing::warn!("file channel (TCP {addr}): {e:#}; files go over QUIC"),
                            }
                        }));
                    }
                    Some(Msg::GamepadRumble(r)) => sinks.ui.send(UiEvent::GamepadRumble(r)),
                    Some(Msg::Bye(b)) => break End::Fatal(format!("被控端断开：{}", b.reason)),
                    Some(other) => tracing::debug!("ignoring {other:?}"),
                    None => tracing::debug!("ignoring unknown control message"),
                }
            }
            c = cmds.recv() => match c {
                Some(NetCmd::Input(m)) => {
                    if let Err(e) = write_msg(&mut input, &m).await {
                        break End::Lost(format!("input: {e}"));
                    }
                }
                Some(NetCmd::Control(m)) => {
                    track(p, &m);
                    if matches!(m.msg, Some(Msg::ClipboardText(_))) && !clipboard {
                        continue;
                    }
                    if let Err(e) = write_msg(&mut send, &m).await {
                        break End::Lost(format!("control: {e}"));
                    }
                }
                Some(NetCmd::SendFiles(paths)) => {
                    if files_on {
                        tokio::spawn(crate::transfer::upload(files_link.clone(), paths, sinks.ui.clone()));
                    } else {
                        sinks.ui.send(UiEvent::FileResult(pb::FileResult {
                            ok: false,
                            message: "被控端版本不支持文件传输，请升级被控端".into(),
                            ..Default::default()
                        }));
                    }
                }
                Some(NetCmd::Mic(d)) => {
                    if neg.has(Feature::Microphone) {
                        let _ = conn.send_datagram(d.into());
                    }
                }
                Some(NetCmd::SendImage(dib)) => {
                    if images_on {
                        tokio::spawn(crate::transfer::send_image(files_link.clone(), dib));
                    }
                }
                Some(NetCmd::SetTransport(t)) => {
                    p.transport = t;
                    match t {
                        Transport::Tcp if !via_tcp => break End::Switch(true, "已选择 TCP".into()),
                        Transport::Udp if via_tcp => break End::Switch(false, "已选择 UDP".into()),
                        _ => {}
                    }
                }
                Some(NetCmd::CancelTransfer(id)) => {
                    tracing::info!("transfer {id:016x} cancelled here");
                    cancel_transfer(&files_link, &sinks.clip, id, "已取消");
                    if let Err(e) = write_msg(&mut send, &ctl(Msg::FileCancel(pb::FileCancel { transfer_id: id }))).await {
                        break End::Lost(format!("control: {e}"));
                    }
                }
                Some(NetCmd::OfferFiles(paths)) => {
                    if clip_on {
                        let offer = sinks.clip.outgoing.lock().unwrap().offer(&paths, true);
                        if let Some(o) = offer {
                            tracing::info!("offering {} copied item(s) to the host", o.files.len());
                            if let Err(e) = write_msg(&mut send, &ctl(Msg::FileOffer(o))).await {
                                break End::Lost(format!("control: {e}"));
                            }
                        }
                    }
                }
                Some(NetCmd::ClipboardPaste(id, reply)) => {
                    use nya_transport::clipfiles::Paste;
                    match sinks.clip.incoming.paste(id) {
                        Paste::Ready(p) => { let _ = reply.send(Ok(p)); }
                        Paste::Request => {
                            sinks.clip.restart(id);
                            sinks.clip.wait(id, reply);
                            let req = pb::FileRequest { transfer_id: id, purpose: pb::FilePurpose::Clipboard as i32 };
                            if let Err(e) = write_msg(&mut send, &ctl(Msg::FileRequest(req))).await {
                                break End::Lost(format!("control: {e}"));
                            }
                        }
                        Paste::Wait => sinks.clip.wait(id, reply),
                        Paste::Failed(e) => { let _ = reply.send(Err(e)); }
                        Paste::Unknown => { let _ = reply.send(Err("这批文件已过期，请在被控端重新复制".into())); }
                    }
                }
                Some(NetCmd::Quit) | None => {
                    let _ = write_msg(&mut send, &ctl(Msg::Bye(pb::Bye { reason: "用户断开".into() }))).await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    conn.close(0u32.into(), b"bye");
                    break End::UserQuit;
                }
            },
            _ = udp_probe.tick(), if p.transport == Transport::Auto && via_tcp && probe.is_none()
                && p.prefer_tcp_until.is_none_or(|t| Instant::now() >= t) => {
                let (addr, id, pinned) = (p.addr, p.identity.clone(), p.pinned);
                probe = Some(tokio::spawn(async move {
                    match open_one(addr, id, Some(pinned), false).await {
                        Ok((ep, conn, _)) => {
                            conn.close(0u32.into(), b"probe");
                            ep.wait_idle().await;
                            true
                        }
                        Err(e) => {
                            tracing::info!("UDP still not usable: {e:#}");
                            false
                        }
                    }
                }));
            }
            Some(udp_ok) = async { match probe.as_mut() { Some(h) => h.await.ok(), None => std::future::pending().await } } => {
                probe = None;
                if udp_ok {
                    break End::Switch(false, "UDP 已恢复".into());
                }
            }
            Some(m) = internal_rx.recv() => {
                if let Err(e) = write_msg(&mut send, &m).await {
                    break End::Lost(format!("control: {e}"));
                }
            }
            _ = ping.tick() => {
                let _ = write_msg(&mut send, &ctl(Msg::Ping(pb::Ping { t_us: nya_proto::now_us() }))).await;
            }
            e = conn.closed() => break End::Lost(format!("{e}")),
        }
    };
    uni.abort();
    bidi.abort();
    dgram.abort();
    if let Some(t) = file_channel_task {
        t.abort();
    }
    if let Some(ch) = files_link.tcp() {
        ch.close().await;
    }
    // The host starts over after a reconnect: pastes in progress can't finish.
    sinks.clip.fail_all("与被控端的连接断开了");
    end
}

/// Stop transfer `id` here: sending stops, receiving fails (partial files
/// removed), a paste waiting for it fails with `why`.
fn cancel_transfer(link: &nya_transport::files::FileLink, clip: &crate::transfer::ClipFiles, id: u64, why: &str) {
    link.cancels().cancel(id);
    if clip.incoming.in_progress(id) {
        if let Some(done) = clip.incoming.fail(id, why.to_owned()) {
            clip.finish(id, done);
        }
    }
}

async fn accept_uni(
    conn: Connection,
    video: Arc<VideoRoutes>,
    ui: Ui,
    stats: Arc<Shared>,
    clip: Arc<crate::transfer::ClipFiles>,
    downloads: Arc<crate::transfer::Downloads>,
    cancels: Arc<nya_transport::files::Cancels>,
    flags: (bool, bool, bool, bool),
    multi: bool,
) {
    while let Ok(mut r) = conn.accept_uni().await {
        let (video, ui, stats, downloads, clip, cancels) = (video.clone(), ui.clone(), stats.clone(), downloads.clone(), clip.clone(), cancels.clone());
        tokio::spawn(async move {
            match read_varint(&mut r).await {
                Ok(Some(stream_type::FILE)) => crate::transfer::receive(r, ui, downloads, clip, cancels, flags).await,
                Ok(Some(stream_type::VIDEO)) => {
                    let Ok(Some(stream_id)) = read_varint(&mut r).await else { return };
                    // With FEATURE_MULTI_STREAM the prelude names the window (slot).
                    let slot = if multi {
                        let Ok(Some(slot)) = read_varint(&mut r).await else { return };
                        slot as u32
                    } else {
                        0
                    };
                    let Some(video) = video.get(slot) else {
                        tracing::debug!("ignoring video stream {stream_id}: no window for slot {slot}");
                        let _ = r.stop(0u32.into());
                        return;
                    };
                    tracing::info!("video stream {stream_id} (slot {slot}) opened by host");
                    loop {
                        let mut len = [0u8; 4];
                        if r.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let len = u32::from_le_bytes(len) as usize;
                        if len < VideoFrameHeader::LEN_V1 || len > MAX_VIDEO_FRAME_LEN {
                            tracing::warn!("bad video frame length {len}");
                            return;
                        }
                        let mut buf = vec![0u8; len];
                        if r.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        deliver(&video, &stats, stream_id, buf);
                    }
                }
                Ok(Some(stream_type::CURSOR)) => {
                    while let Ok(Some(m)) = read_msg::<pb::CursorMsg, _>(&mut r, MAX_MESSAGE_LEN).await {
                        ui.send(UiEvent::Cursor(m));
                    }
                }
                Ok(Some(other)) => {
                    tracing::debug!("unknown stream type {other}");
                    let _ = r.stop(0u32.into());
                }
                _ => {}
            }
        });
    }
}

/// A received frame (video frame header + payload) to its window's decoder.
fn deliver(video: &Sender<VideoIn>, stats: &Shared, stream_id: u64, buf: Vec<u8>) {
    let len = buf.len() as u64;
    let first = stats.with(|s| {
        s.bytes += len;
        s.total_rx_bytes += len;
        s.total_rx_frames += 1;
        s.total_rx_frames == 1
    });
    if first {
        tracing::info!("first video frame received: stream {stream_id}, {len} bytes");
    }
    if video.try_send(VideoIn::Frame { stream_id, buf }).is_err() {
        tracing::warn!("decoder queue full; dropping frame");
    }
}

/// Audio, and video sent as datagrams with FEC (FEATURE_VIDEO_DATAGRAM).
async fn read_datagrams(
    conn: Connection,
    audio: Sender<AudioPacket>,
    audio_on: bool,
    video: Arc<VideoRoutes>,
    stats: Arc<Shared>,
    video_on: bool,
) {
    let mut frames = nya_transport::videodgram::Reassembler::new(MAX_VIDEO_FRAME_LEN);
    let mut published = Instant::now();
    let mut first = true;
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&datagram_type::AUDIO) if audio_on => {
                if let Some(p) = AudioPacket::decode(&d) {
                    let _ = audio.try_send(p);
                }
            }
            Some(&datagram_type::VIDEO) if video_on => {
                if first {
                    first = false;
                    tracing::info!("video arrives as datagrams with FEC");
                }
                if let Some(f) = frames.push(&d) {
                    match video.get(f.slot as u32) {
                        Some(tx) => deliver(&tx, &stats, f.stream_id, f.data),
                        None => tracing::debug!("ignoring video frame for slot {}: no window", f.slot),
                    }
                }
                if published.elapsed() >= Duration::from_millis(250) {
                    published = Instant::now();
                    let st = frames.take_stats();
                    stats.with(|s| s.dgram.add(&st));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host reachable over TCP only (UDP blocked): auto connects over TCP
    /// after UDP's head start; UDP only does not connect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_falls_back_to_tcp() {
        use tokio::io::AsyncReadExt;
        let (host_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let socket = nya_transport::tcptunnel::TunnelSocket::new(addr);
        let server = nya_transport::tcptunnel::server_endpoint(socket.clone(), &host_id).unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, peer)) = listener.accept().await {
                let mut pre = [0u8; 8];
                if tcp.read_exact(&mut pre).await.is_ok() {
                    socket.add(tcp, peer);
                }
            }
        });
        tokio::spawn(async move {
            while let Some(i) = server.accept().await {
                tokio::spawn(async move {
                    if let Ok(c) = i.await {
                        c.closed().await;
                    }
                });
            }
        });
        let pin = Some(host_id.fingerprint());
        let start = Instant::now();
        let (_ep, conn, tcp) = open(addr, &client_id, pin, Transport::Auto, false).await.unwrap();
        assert!(tcp, "over TCP");
        assert!(start.elapsed() >= HEAD_START, "UDP had its head start");
        conn.close(0u32.into(), b"");
        // Preferring TCP (moved there for loss): connects at once.
        let start = Instant::now();
        let (_ep, _conn, tcp) = open(addr, &client_id, pin, Transport::Auto, true).await.unwrap();
        assert!(tcp && start.elapsed() < HEAD_START);
        assert!(open(addr, &client_id, pin, Transport::Udp, false).await.is_err(), "no UDP there");
    }

    /// A host that pairs with `key` (the host's side of the handshake).
    fn pairing_host(id: &Identity, key: PairingKey) -> SocketAddr {
        let server = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), id).unwrap();
        let addr = server.local_addr().unwrap();
        let server_fp = id.fingerprint();
        tokio::spawn(async move {
            while let Some(i) = server.accept().await {
                let key = key.clone();
                tokio::spawn(async move {
                    let Ok(conn) = i.await else { return };
                    let client_fp = peer_fingerprint(&conn).unwrap();
                    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                    let hello: pb::Hello = expect_msg(&mut recv, MAX_MESSAGE_LEN).await.unwrap();
                    let n = negotiate::negotiate(&hello, &LocalVersion::current()).unwrap();
                    let welcome = pb::Welcome {
                        proto_major: n.major,
                        proto_minor: n.minor,
                        server_name: "host".into(),
                        server_version: "test".into(),
                        features: n.features.iter().copied().collect(),
                        needs_pairing: true,
                    };
                    write_msg(&mut send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Welcome(welcome)) }).await.unwrap();
                    let server_nonce = pairing::nonce();
                    write_msg(&mut send, &ctl(Msg::AuthChallenge(pb::AuthChallenge { server_nonce: server_nonce.to_vec() }))).await.unwrap();
                    let r: pb::ControlMsg = expect_msg(&mut recv, MAX_MESSAGE_LEN).await.unwrap();
                    let Some(Msg::AuthResponse(r)) = r.msg else { panic!("expected AuthResponse") };
                    let t = Transcript { server_nonce: &server_nonce, client_nonce: &r.client_nonce, server_fp, client_fp };
                    let ok = t.verify_client(&key, &r.mac);
                    let server_mac = if ok { t.server_mac(&key) } else { vec![] };
                    write_msg(&mut send, &ctl(Msg::AuthResult(pb::AuthResult { ok, server_mac, message: "配对码错误".into() }))).await.unwrap();
                    conn.closed().await;
                });
            }
        });
        addr
    }

    /// A pairing link: of a dead address, another host (not the link's
    /// certificate) and the host, the host is used, paired with the link's code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pairing_link_finds_the_host_and_pairs() {
        let (host_id, other_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let key = PairingKey::generate();
        let host = pairing_host(&host_id, key.clone()).to_string();
        let other = pairing_host(&other_id, key.clone()).to_string();
        let dead = "127.0.0.1:9".to_owned();
        let invite = nya_transport::invite::Invite {
            name: "host".into(),
            addresses: vec![dead, other, host.clone()],
            fingerprint: Some(host_id.fingerprint()),
            code: key.to_code(),
        };
        let invite = nya_transport::invite::Invite::parse(&invite.to_link()).unwrap();
        let code = invite.code.clone();
        let prompt: PairPrompt = Arc::new(move || Some(code.clone()));
        let (used, link) =
            connect_any(&invite.addresses, &client_id, invite.fingerprint, "client", Some(prompt), Transport::Udp).await.unwrap();
        assert_eq!(used, host);
        assert_eq!(link.server_fp, host_id.fingerprint());
        assert!(link.welcome.needs_pairing);

        // A stale code (the host's was regenerated): pairing fails.
        let prompt: PairPrompt = Arc::new(|| Some(PairingKey::generate().to_code()));
        let e = connect_any(&[host], &client_id, invite.fingerprint, "client", Some(prompt), Transport::Udp).await.err().unwrap();
        assert!(format!("{e:#}").contains("配对失败"), "{e:#}");
    }
}
