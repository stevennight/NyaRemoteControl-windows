//! Network side of the host: accept QUIC connections, run the handshake
//! (version negotiation + pairing), then bridge the client with the hub.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nya_proto::frame::stream_type;
use nya_proto::framing::{encode_varint, expect_msg, read_msg, read_varint, write_msg};
use nya_proto::negotiate::{self, LocalVersion, Negotiated};
use nya_proto::pb::{self, control_msg::Msg, Feature};
use nya_proto::MAX_MESSAGE_LEN;
use nya_transport::identity::peer_fingerprint;
use nya_transport::pairing::{self, Transcript};
use nya_transport::quinn::{self, Connection, RecvStream, SendStream};
use nya_transport::Fingerprint;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::control_pb::{self as cpb, event::Kind};
use crate::hub::Hub;
use crate::ipc_pb::{host_command::Cmd, host_event::Ev, FrameSent, SetAudio};
use crate::state::{unix_now, State};

/// Accept connections until the endpoint is closed.
pub async fn serve_endpoint(endpoint: quinn::Endpoint, state: Arc<State>) -> Result<()> {
    tracing::info!("listening on UDP {} (certificate {})", endpoint.local_addr()?, state.fingerprint);
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle(conn, &state).await {
                        tracing::info!("{remote}: session ended: {e:#}");
                    }
                }
                Err(e) => tracing::debug!("{remote}: handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

fn ctl(msg: Msg) -> pb::ControlMsg {
    pb::ControlMsg { msg: Some(msg) }
}

/// Version negotiation and (if needed) pairing. Returns the negotiated session.
async fn handshake(
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    state: &State,
    server_fp: Fingerprint,
) -> Result<(Negotiated, pb::Hello)> {
    let auth = &state.auth;
    let hello: pb::Hello = timeout(Duration::from_secs(10), expect_msg(recv, MAX_MESSAGE_LEN))
        .await
        .context("hello timeout")??;
    let client_fp = peer_fingerprint(conn).ok_or_else(|| anyhow!("no client certificate"))?;
    let remote = conn.remote_address();
    let negotiated = match negotiate::negotiate(&hello, &LocalVersion::current()) {
        Ok(n) => n,
        Err(reject) => {
            tracing::warn!("rejecting {}: {}", hello.client_name, reject.message);
            state.event(Kind::Rejected, format!("拒绝 {}（{remote}）：{}", hello.client_name, reject.message));
            write_msg(send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Reject(reject)) }).await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            bail!("version mismatch");
        }
    };

    let paired = auth.is_paired(&client_fp);
    if paired {
        if let Err(e) = auth.update_name(&client_fp, &hello.client_name) {
            tracing::warn!("update client name: {e:#}");
        }
    }
    if !paired && auth.locked_out() {
        state.event(Kind::Rejected, format!("拒绝 {}（{remote}）：配对失败次数过多", hello.client_name));
        let reject = pb::Reject {
            reason: pb::RejectReason::AuthFailed as i32,
            message: "配对失败次数过多，请 10 分钟后再试".into(),
            server_proto_major: nya_proto::PROTO_MAJOR,
            server_min_proto_major: nya_proto::MIN_PROTO_MAJOR,
        };
        write_msg(send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Reject(reject)) }).await?;
        let _ = send.finish();
        bail!("pairing locked out");
    }
    let welcome = pb::Welcome {
        proto_major: negotiated.major,
        proto_minor: negotiated.minor,
        server_name: state.server_name(),
        server_version: crate::version(),
        features: negotiated.features.iter().copied().collect(),
        needs_pairing: !paired,
    };
    write_msg(send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Welcome(welcome)) }).await?;

    if !paired {
        let server_nonce = pairing::nonce();
        write_msg(send, &ctl(Msg::AuthChallenge(pb::AuthChallenge { server_nonce: server_nonce.to_vec() }))).await?;
        // The user may need a while to type the code.
        let resp: pb::ControlMsg = timeout(Duration::from_secs(300), expect_msg(recv, MAX_MESSAGE_LEN))
            .await
            .context("pairing timeout")??;
        let Some(Msg::AuthResponse(r)) = resp.msg else { bail!("expected AuthResponse") };
        let t = Transcript { server_nonce: &server_nonce, client_nonce: &r.client_nonce, server_fp, client_fp };
        let key = auth.key();
        if r.client_nonce.len() != pairing::NONCE_LEN || !t.verify_client(&key, &r.mac) {
            auth.record_failure();
            state.event(Kind::PairingFailed, format!("{}（{remote}）配对码错误", hello.client_name));
            tokio::time::sleep(Duration::from_secs(1)).await;
            write_msg(
                send,
                &ctl(Msg::AuthResult(pb::AuthResult { ok: false, server_mac: vec![], message: "配对码错误".into() })),
            )
            .await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            bail!("wrong pairing code from {}", hello.client_name);
        }
        auth.add(&client_fp, &hello.client_name)?;
        tracing::info!("paired new client {} ({client_fp})", hello.client_name);
        state.event(Kind::Paired, format!("新客户端 {}（{remote}）配对成功", hello.client_name));
        write_msg(
            send,
            &ctl(Msg::AuthResult(pb::AuthResult { ok: true, server_mac: t.server_mac(&key), message: String::new() })),
        )
        .await?;
    }
    Ok((negotiated, hello))
}

async fn handle(conn: Connection, state: &State) -> Result<()> {
    let remote = conn.remote_address();
    let server_fp = auth_server_fp(&conn)?;
    let (mut send, mut recv) = timeout(Duration::from_secs(10), conn.accept_bi()).await.context("control stream timeout")??;
    let (neg, hello) = handshake(&conn, &mut send, &mut recv, state, server_fp).await?;
    tracing::info!(
        "{remote}: client {} {} (proto {}.{}, features {:?})",
        hello.client_name,
        hello.client_version,
        neg.major,
        neg.minor,
        neg.features
    );

    let hub = &state.hub;
    // Clients that can watch join next to a running session; older ones replace it.
    let fingerprint = peer_fingerprint(&conn).map(|f| f.to_hex()).unwrap_or_default();
    let att = hub.attach(&hello.client_name, &fingerprint, neg.has(Feature::MultiClient));
    let token = att.token;
    state.session_started(
        token,
        cpb::Session {
            client_name: hello.client_name.clone(),
            client_version: hello.client_version.clone(),
            fingerprint,
            remote_addr: remote.to_string(),
            since_unix: unix_now(),
        },
    );
    state.event(Kind::Connected, format!("{} 已连接（{remote}）", hello.client_name));
    let result = run_session(&conn, send, recv, hub, att, &neg, state, &hello.client_name).await;
    hub.detach(token);
    state.session_ended(token);
    let why = match &result {
        Ok(()) => "客户端断开".to_owned(),
        Err(e) => format!("{e:#}"),
    };
    state.event(Kind::Disconnected, format!("{} 已断开：{why}", hello.client_name));
    conn.close(0u32.into(), b"bye");
    result
}

/// Our own certificate fingerprint as seen on this connection (for the pairing transcript).
fn auth_server_fp(conn: &Connection) -> Result<Fingerprint> {
    // quinn doesn't expose the local certificate; the service identity is loaded once.
    let _ = conn;
    SERVER_FP.get().copied().ok_or_else(|| anyhow!("server identity not set"))
}

pub static SERVER_FP: std::sync::OnceLock<Fingerprint> = std::sync::OnceLock::new();

/// Everything the client asked for, so it can be replayed when the helper restarts.
#[derive(Default)]
struct Replay {
    caps: Option<pb::ClientCaps>,
    /// Stream requests by slot (client window).
    starts: std::collections::BTreeMap<u32, pb::StartStream>,
    audio: bool,
    /// Folders the client wants on the host as a drive (FOLDER_MOUNT).
    folders: Option<pb::SharedFolders>,
}

/// The drive showing the client's shared folders, if mounted.
type FolderMount = Arc<tokio::sync::Mutex<Option<crate::winfsp::Mount>>>;

/// Mount, keep or unmount the client's folders to match `want`, and tell
/// the client how it went.
fn apply_folders(
    mount: &FolderMount,
    want: pb::SharedFolders,
    conn: &Connection,
    client: &str,
    ctl_tx: &mpsc::Sender<pb::ControlMsg>,
    hub: &Arc<Hub>,
) {
    let (mount, conn, client, ctl_tx, hub) = (mount.clone(), conn.clone(), client.to_owned(), ctl_tx.clone(), hub.clone());
    tokio::spawn(async move {
        let mut m = mount.lock().await;
        let status = if want.folders.is_empty() {
            if let Some(old) = m.take() {
                let point = old.point.clone();
                let _ = tokio::task::spawn_blocking(move || drop(old)).await;
                tracing::info!("client folders unmounted from {point}");
                announce_drive(&hub, &point, false);
            }
            pb::FolderMountStatus { mounted: false, mount_point: String::new(), message: String::new() }
        } else if let Some(cur) = m.as_ref() {
            // The client answers the listing itself: a changed list shows at once.
            pb::FolderMountStatus { mounted: true, mount_point: cur.point.clone(), message: String::new() }
        } else {
            let rt = tokio::runtime::Handle::current();
            // The drive going away on its own: the client hears it.
            let on_end = {
                let (mount, ctl_tx, hub, rt) = (mount.clone(), ctl_tx.clone(), hub.clone(), rt.clone());
                move |message: String| {
                    rt.spawn(async move {
                        let mut m = mount.lock().await;
                        let Some(point) = m.as_ref().filter(|m| m.ended()).map(|m| m.point.clone()) else { return };
                        m.take();
                        announce_drive(&hub, &point, false);
                        let status = pb::FolderMountStatus { mounted: false, mount_point: String::new(), message };
                        let _ = ctl_tx.send(ctl(Msg::FolderMountStatus(status))).await;
                    });
                }
            };
            let r = tokio::task::spawn_blocking(move || crate::winfsp::Mount::start(conn, rt, &client, on_end)).await;
            match r.map_err(anyhow::Error::from).and_then(|r| r) {
                Ok(new) => {
                    tracing::info!("client folders mounted on {} ({} folder(s))", new.point, want.folders.len());
                    let point = new.point.clone();
                    *m = Some(new);
                    announce_drive(&hub, &point, true);
                    pb::FolderMountStatus { mounted: true, mount_point: point, message: String::new() }
                }
                Err(e) => {
                    tracing::warn!("mount client folders: {e:#}");
                    pb::FolderMountStatus { mounted: false, mount_point: String::new(), message: format!("{e:#}") }
                }
            }
        };
        let _ = ctl_tx.send(ctl(Msg::FolderMountStatus(status))).await;
    });
}

/// The drive letter came or went: the helper tells Explorer in the user's
/// session (Windows doesn't announce letters a service creates).
fn announce_drive(hub: &Hub, point: &str, added: bool) {
    hub.send(Cmd::DriveChanged(crate::ipc_pb::DriveChanged { letter: point.to_owned(), added }));
}

async fn run_session(

    conn: &Connection,
    send: SendStream,
    mut recv: RecvStream,
    hub: &Arc<Hub>,
    mut att: crate::hub::Attachment,
    neg: &Negotiated,
    state: &State,
    client_name: &str,
) -> Result<()> {
    // Control writer task.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<pb::ControlMsg>(64);
    let mut ctl_send = send;
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_msg(&mut ctl_send, &m).await.is_err() {
                break;
            }
        }
    });

    let info = hub.session_info.borrow().clone();
    if let Some(info) = info {
        let _ = ctl_tx.send(ctl(Msg::SessionInfo(info))).await;
    }

    // Only the operating client's requests reach the host (FEATURE_MULTI_CLIENT).
    let roles_on = neg.has(Feature::MultiClient);
    let controlling = Arc::new(AtomicBool::new(att.role.borrow().controlling));
    if !controlling.load(Ordering::Relaxed) {
        // Joining a running session to watch: the picture as it is.
        let (streams, displays) = hub.picture();
        if let Some(d) = displays {
            let _ = ctl_tx.send(ctl(Msg::DisplayChanged(d))).await;
        }
        for s in streams {
            hub.send(Cmd::RequestKeyframe(pb::RequestKeyframe { slot: s.slot }));
            let _ = ctl_tx.send(ctl(Msg::StreamStarted(s))).await;
        }
    }
    // Keyframes asked for by a watching client (rate-limited: they cost the operator bandwidth).
    let mut last_keyframe = Instant::now() - Duration::from_secs(10);
    // A watching client starts mid-stream: nothing before a keyframe is any use to it.
    let mut watch_needs_key = true;


    // Video as datagrams + FEC (game mode by default) or on a stream.
    let dgram_on = neg.has(Feature::VideoDatagram);
    let dgram = Arc::new(DgramState::default());

    // Video / cursor writer tasks.
    let (video_tx, video_rx) = mpsc::channel::<crate::ipc_pb::VideoFrame>(4);
    let (cursor_tx, cursor_rx) = mpsc::channel::<pb::CursorMsg>(256);
    let written = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let video_task = tokio::spawn({
        let (conn, hub, written, controlling, dgram) = (conn.clone(), hub.clone(), written.clone(), controlling.clone(), dgram.clone());
        let multi = neg.has(Feature::MultiStream);
        async move {
            if let Err(e) = video_writer(conn, video_rx, hub, written, multi, controlling, dgram).await {
                tracing::warn!("video writer: {e:#}");
            }
        }
    });
    let cursor_task = tokio::spawn({
        let conn = conn.clone();
        async move {
            if let Err(e) = cursor_writer(conn, cursor_rx).await {
                tracing::warn!("cursor writer: {e:#}");
            }
        }
    });
    let mut cursor_dropped = 0u64;
    let mic_on = neg.has(Feature::Microphone);
    let mic_task = tokio::spawn({
        let (conn, hub, controlling) = (conn.clone(), hub.clone(), controlling.clone());
        async move {
            while let Ok(d) = conn.read_datagram().await {
                if mic_on && controlling.load(Ordering::Relaxed) && d.first() == Some(&nya_proto::frame::datagram_type::MIC) {
                    hub.send(Cmd::MicAudio(crate::ipc_pb::MicAudio { datagram: d.to_vec() }));
                }
            }
        }
    });
    let usb_on = neg.has(Feature::UsbRedirect);
    let usb = Arc::new(tokio::sync::Mutex::new(crate::usb::UsbHost::new(conn.clone())));
    let files_on = neg.has(Feature::FileTransfer);
    let images_on = neg.has(Feature::ClipboardImage);
    // Copy on one side, paste on the other (folders too), both directions.
    let clip_files_on = files_on && neg.has(Feature::ClipboardFiles);
    let incoming = nya_transport::clipfiles::Incoming::default();
    // Where files go: the TCP file channel once the client has opened it
    // (FEATURE_TCP_FILES), FILE streams on this connection until then.
    let link = nya_transport::files::FileLink::new(conn.clone());
    let file_ctx = Arc::new(FileCtx {
        ctl_tx: ctl_tx.clone(),
        files_on,
        images_on,
        clip_files_on,
        incoming: incoming.clone(),
        batches: Default::default(),
        controlling: controlling.clone(),
        cancels: link.cancels().clone(),
    });
    let input_task = tokio::spawn(client_streams(conn.clone(), hub.clone(), file_ctx.clone()));
    let file_channel_task = match (neg.has(Feature::TcpFiles), peer_fingerprint(conn)) {
        (true, Some(client)) => {
            let place = state.file_channels.expect(client);
            let _ = ctl_tx.send(ctl(Msg::FileChannel(pb::FileChannel { token: place.token.clone() }))).await;
            let (link, hub, ctx) = (link.clone(), hub.clone(), file_ctx.clone());
            Some(tokio::spawn(async move {
                let on_file: nya_transport::filechan::OnFile = Arc::new(move |h, mut r| {
                    let (hub, ctx) = (hub.clone(), ctx.clone());
                    tokio::spawn(async move {
                        if ctx.controlling.load(Ordering::Relaxed) {
                            if let Err(e) = receive_file_body(h, &mut r, hub, ctx).await {
                                tracing::warn!("file (TCP): {e:#}");
                            }
                        }
                    });
                });
                match place.accept(Duration::from_secs(20), on_file).await {
                    Ok(ch) => {
                        tracing::info!("files go over the TCP file channel");
                        link.set_tcp(Some(ch));
                    }
                    Err(e) => tracing::warn!("{e:#}; files go over QUIC"),
                }
            }))
        }
        _ => None,
    };
    // Files copied on the host and offered to the client.
    let mut offers = nya_transport::clipfiles::Outgoing::default();
    // Where the client's files are fetched to when they are pasted here.
    let clip_cache = if clip_files_on {
        let dir = tokio::task::spawn_blocking(crate::winutil::clipboard_cache_dir).await.unwrap_or_else(|_| std::env::temp_dir());
        let d = dir.clone();
        tokio::task::spawn_blocking(move || nya_transport::files::prune_cache(&d));
        dir
    } else {
        std::path::PathBuf::new()
    };

    let mut replay = Replay::default();
    // QUIC packets sent / lost at the last ServerStats (ServerStats.path_loss_pct).
    let mut path_seen = (0u64, 0u64);
    let folders_on = neg.has(Feature::FolderMount);
    let print_on = neg.has(Feature::Print);
    let mut prints = state.prints.subscribe();
    let mount: FolderMount = Default::default();
    // Several streams at once (one per client window).
    let multi_on = neg.has(Feature::MultiStream);
    // Configured bitrate of each running stream, by slot.
    let mut stream_max: std::collections::BTreeMap<u32, u32> = Default::default();
    let mut generation = hub.generation.subscribe();
    generation.mark_unchanged();
    let audio_on = neg.has(Feature::Audio);
    let clipboard_on = neg.has(Feature::ClipboardText);

    let mut abr: Option<crate::abr::Abr> = None;
    let mut abr_tick = tokio::time::interval(Duration::from_millis(250));
    // udp_tx bytes not caused by video (handshake, control, audio, overhead)
    let mut baseline: i64 = conn.stats().udp_tx.bytes as i64;

    let outcome: Result<()> = loop {
        tokio::select! {
            _ = abr_tick.tick() => {
                if let Some(a) = abr.as_mut() {
                    let st = conn.stats();
                    let backlog = backlog_estimate(written.load(std::sync::atomic::Ordering::Relaxed), st.udp_tx.bytes, &mut baseline);
                    let sample = crate::abr::Sample {
                        now: std::time::Instant::now(),
                        rtt: st.path.rtt,
                        lost_packets: st.path.lost_packets,
                        sent_packets: st.path.sent_packets,
                        sent_bytes: st.udp_tx.bytes,
                        backlog_bytes: backlog,
                    };
                    if let Some(k) = a.update(sample).filter(|_| controlling.load(Ordering::Relaxed)) {
                        tracing::info!("adaptive bitrate -> {k} kbps ({}; rtt {} ms)", a.note, st.path.rtt.as_millis());
                        hub.send(Cmd::SetBitrate(crate::ipc_pb::SetBitrate { kbps: k }));
                    }
                }
            }
            m = read_msg::<pb::ControlMsg, _>(&mut recv, MAX_MESSAGE_LEN) => {
                let m = match m {
                    Ok(Some(m)) => m,
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e.into()),
                };
                // A watching client's requests are kept (for when it takes
                // over) but do not reach the host.
                let in_control = controlling.load(Ordering::Relaxed);
                match m.msg {
                    Some(Msg::ClientCaps(c)) => {
                        replay.caps = Some(c.clone());
                        replay.audio = audio_on;
                        // Watching clients count too: the picture's format is one they all decode.
                        hub.set_caps(att.token, c);
                        if in_control && audio_on {
                            hub.send(Cmd::SetAudio(SetAudio { enabled: true }));
                        }
                    }
                    Some(Msg::StartStream(mut s)) => {
                        if !multi_on {
                            s.slot = 0;
                        }
                        replay.starts.insert(s.slot, s.clone());
                        if in_control {
                            hub.send(Cmd::StartStream(s));
                        }
                        // A (re)started stream begins with a keyframe anyway.
                        choose_transport(&dgram, dgram_on, &replay, None);
                    }
                    Some(Msg::StopStream(mut s)) => {
                        if !multi_on {
                            s.slot = 0;
                        }
                        // Stopping the main window's stream stops them all.
                        if s.slot == 0 {
                            replay.starts.clear();
                        } else {
                            replay.starts.remove(&s.slot);
                        }
                        if in_control {
                            stream_max.remove(&s.slot);
                            hub.send(Cmd::StopStream(s));
                        }
                    }
                    Some(Msg::SetMode(m)) => {
                        for s in replay.starts.values_mut() {
                            s.config.get_or_insert_with(Default::default).mode = m.mode;
                        }
                        if in_control {
                            hub.send(Cmd::SetMode(m));
                        }
                        choose_transport(&dgram, dgram_on, &replay, in_control.then_some(&**hub));
                    }
                    Some(Msg::RequestKeyframe(k)) => {
                        if in_control || last_keyframe.elapsed() >= Duration::from_secs(1) {
                            last_keyframe = Instant::now();
                            hub.send(Cmd::RequestKeyframe(k));
                        }
                    }
                    Some(Msg::SharedFolders(f)) if folders_on => {
                        replay.folders = Some(f.clone());
                        if in_control {
                            apply_folders(&mount, f, conn, client_name, &ctl_tx, hub);
                        } else {
                            let message = "正在观看，接管操作后才会挂载共享文件夹".to_string();
                            let _ = ctl_tx.send(ctl(Msg::FolderMountStatus(pb::FolderMountStatus { message, ..Default::default() }))).await;
                        }
                    }
                    Some(Msg::TakeControl(t)) if roles_on => {
                        tracing::info!("client takes over the host{}", if t.kick { " (disconnecting the previous operator)" } else { "" });
                        hub.take_control(att.token, t.kick);
                    }
                    Some(Msg::UsbAttach(a)) if usb_on && !in_control => {
                        let message = "正在观看，接管操作后才能透传 USB 设备".to_string();
                        let _ = ctl_tx.send(ctl(Msg::UsbStatus(pb::UsbStatus { busid: a.busid, attached: false, message }))).await;
                    }
                    Some(Msg::UsbAttach(a)) if usb_on => {
                        let (usb, tx) = (usb.clone(), ctl_tx.clone());
                        tokio::spawn(async move {
                            let r = usb.lock().await.attach(&a.busid).await;
                            let (attached, message) = match r {
                                Ok(m) => (true, m),
                                Err(e) => (false, format!("{e:#}")),
                            };
                            let _ = tx.send(ctl(Msg::UsbStatus(pb::UsbStatus { busid: a.busid, attached, message }))).await;
                        });
                    }
                    Some(Msg::UsbDetach(d)) if usb_on => {
                        let (usb, tx) = (usb.clone(), ctl_tx.clone());
                        tokio::spawn(async move {
                            let message = match usb.lock().await.detach(&d.busid).await {
                                Ok(m) => m,
                                Err(e) => format!("{e:#}"),
                            };
                            let _ = tx.send(ctl(Msg::UsbStatus(pb::UsbStatus { busid: d.busid, attached: false, message }))).await;
                        });
                    }
                    Some(Msg::FileRequest(req)) if files_on => {
                        let purpose = match pb::FilePurpose::try_from(req.purpose) {
                            Ok(pb::FilePurpose::Clipboard) if clip_files_on => pb::FilePurpose::Clipboard,
                            _ => pb::FilePurpose::Save,
                        };
                        match offers.items(req.transfer_id) {
                            Some(items) => {
                                tokio::spawn(send_offered(link.clone(), req.transfer_id, items, purpose, ctl_tx.clone()));
                            }
                            None => {
                                let _ = ctl_tx.try_send(ctl(Msg::FileResult(pb::FileResult {
                                    transfer_id: req.transfer_id,
                                    ok: false,
                                    message: "这批文件已过期，请在被控端重新复制".into(),
                                    saved_to: String::new(),
                                })));
                            }
                        }
                    }
                    Some(Msg::FileOffer(o)) if clip_files_on && in_control => {
                        tracing::info!("client copied {} item(s) (offer {:016x})", o.files.len(), o.transfer_id);
                        incoming.register(&o, &clip_cache);
                        hub.send(Cmd::ClipboardOffer(crate::ipc_pb::ClipboardOffer { transfer_id: o.transfer_id, files: o.files.clone() }));
                    }
                    Some(Msg::FileCancel(c)) => {
                        tracing::info!("the client cancelled transfer {:016x}", c.transfer_id);
                        link.cancels().cancel(c.transfer_id);
                        if incoming.in_progress(c.transfer_id) {
                            if let Some(done) = incoming.fail(c.transfer_id, "客户端取消了传输".into()) {
                                hub.send(paste_done(c.transfer_id, done));
                            }
                        }
                    }
                    Some(Msg::FileResult(r)) if !r.ok => {
                        if let Some(Err(e)) = incoming.fail(r.transfer_id, r.message.clone()) {
                            hub.send(paste_done(r.transfer_id, Err(e)));
                        }
                    }
                    Some(Msg::Ping(p)) => {
                        let _ = ctl_tx.try_send(ctl(Msg::Pong(pb::Pong { t_us: p.t_us, server_t_us: nya_proto::now_us() })));
                    }
                    Some(Msg::ClientStats(s)) => {
                        tracing::debug!("client stats: {s:?}");
                        if dgram.on.load(Ordering::Relaxed) {
                            let cur = dgram.fec.load(Ordering::Relaxed);
                            let next = nya_transport::videodgram::next_fec_percent(cur, s.video_shards_received, s.video_shards_lost, s.video_frames_lost);
                            if next != cur {
                                tracing::info!(
                                    "video FEC {cur}% -> {next}% (shards {} received / {} lost, frames {} recovered / {} lost)",
                                    s.video_shards_received, s.video_shards_lost, s.video_frames_recovered, s.video_frames_lost
                                );
                                dgram.fec.store(next, Ordering::Relaxed);
                            }
                        }
                    }
                    Some(Msg::ClipboardText(c)) if clipboard_on && in_control => hub.send(Cmd::Clipboard(c)),
                    Some(Msg::SendSas(_)) if neg.has(Feature::Sas) && in_control => {
                        if let Err(e) = crate::winutil::send_sas() {
                            tracing::warn!("SendSAS: {e:#}");
                        }
                    }
                    Some(Msg::Bye(b)) => {
                        tracing::info!("client said bye: {}", b.reason);
                        break Ok(());
                    }
                    Some(other) => tracing::debug!("ignoring control message {other:?}"),
                    None => tracing::debug!("ignoring unknown control message"),
                }
            }
            ev = att.events.recv() => {
                let Some(ev) = ev else { break Ok(()) };
                match ev.ev {
                    Some(Ev::Video(f)) if controlling.load(Ordering::Relaxed) => {
                        if video_tx.send(f).await.is_err() {
                            break Err(anyhow!("video writer stopped"));
                        }
                    }
                    // Watching shows the main picture only (the operator's extra
                    // windows have no window on this client, which would stop the stream).
                    Some(Ev::Video(f)) if f.slot != 0 => {}
                    Some(Ev::Video(f)) => {
                        // Watching: never hold up the host for long. Frames after a
                        // loss are useless until the next keyframe, so they are not
                        // sent (the queue drains); a keyframe waits a little for room
                        // instead of being dropped, or the watcher would never recover.
                        let slot = f.slot;
                        let key = nya_proto::frame::VideoFrameHeader::parse(&f.header).is_ok_and(|(h, _)| h.is_keyframe());
                        if key || !watch_needs_key {
                            let sent = if key {
                                matches!(tokio::time::timeout(Duration::from_millis(500), video_tx.send(f)).await, Ok(Ok(())))
                            } else {
                                video_tx.try_send(f).is_ok()
                            };
                            if sent {
                                if key {
                                    watch_needs_key = false;
                                }
                            } else {
                                watch_needs_key = true;
                                att.video_dropped.store(true, Ordering::Relaxed);
                            }
                        }
                        if att.video_dropped.load(Ordering::Relaxed) && last_keyframe.elapsed() >= Duration::from_secs(1) {
                            att.video_dropped.store(false, Ordering::Relaxed);
                            last_keyframe = Instant::now();
                            hub.send(Cmd::RequestKeyframe(pb::RequestKeyframe { slot }));
                        }
                    }
                    Some(Ev::Cursor(c)) => {
                        if cursor_tx.try_send(c).is_err() {
                            cursor_dropped += 1;
                            if cursor_dropped.is_power_of_two() {
                                tracing::warn!("cursor stream backed up: {cursor_dropped} cursor updates dropped");
                            }
                        }
                    }
                    Some(Ev::Audio(a)) => {
                        if audio_on {
                            if let Err(e) = conn.send_datagram(a.datagram.into()) {
                                tracing::trace!("audio datagram: {e}");
                            }
                        }
                    }
                    Some(Ev::SessionInfo(i)) => { let _ = ctl_tx.send(ctl(Msg::SessionInfo(i))).await; }
                    Some(Ev::StreamStarted(s)) => {
                        // Adaptive bitrate works on the whole connection: its
                        // ceiling is the sum of the streams' configured rates.
                        stream_max.insert(s.slot, s.config.as_ref().map(|c| c.bitrate_kbps).unwrap_or(0));
                        let max: u32 = stream_max.values().sum();
                        let main = replay.starts.get(&0);
                        let requested = main.and_then(|r| r.config.as_ref()).map(|c| c.bitrate_policy).unwrap_or(0);
                        let mode = s.config.as_ref().map(|c| c.mode).unwrap_or(0);
                        let policy = crate::abr::resolve(requested, mode);
                        abr = if max > 0 { crate::abr::Abr::new(max, policy, std::time::Instant::now()) } else { None };
                        tracing::info!("bitrate policy: {} (max {max} kbps, {} stream(s))", crate::abr::policy_name(policy), stream_max.len());
                        if let Some(c) = &s.config {
                            let codec = pb::Codec::try_from(c.codec).map(|c| c.as_str_name()).unwrap_or("?");
                            let more = if stream_max.len() > 1 { format!(" · 共 {} 路画面", stream_max.len()) } else { String::new() };
                            if s.slot == 0 || stream_max.len() > 1 {
                                state.set_stream(format!("{}x{} {} fps · {codec} · {}{more}", c.width, c.height, c.fps, s.encoder_name));
                            }
                        }
                        let _ = ctl_tx.send(ctl(Msg::StreamStarted(s))).await;
                    }
                    Some(Ev::StreamError(e)) => { let _ = ctl_tx.send(ctl(Msg::StreamError(e))).await; }
                    Some(Ev::DisplayChanged(d)) => { let _ = ctl_tx.send(ctl(Msg::DisplayChanged(d))).await; }
                    Some(Ev::Stats(mut s)) => {
                        s.fec_percent = if dgram.on.load(Ordering::Relaxed) { dgram.fec.load(Ordering::Relaxed) } else { 0 };
                        // Loss and round trip of the connection since the last report.
                        let path = conn.stats().path;
                        let (sent, lost) = (path.sent_packets - path_seen.0, path.lost_packets - path_seen.1);
                        path_seen = (path.sent_packets, path.lost_packets);
                        s.path_loss_pct = if sent > 0 { lost as f32 * 100.0 / sent as f32 } else { 0.0 };
                        s.path_rtt_ms = path.rtt.as_secs_f32() * 1000.0;
                        s.bitrate_note = match &abr {
                            Some(a) => a.summary(std::time::Instant::now()),
                            None => "固定码率".into(),
                        };
                        let _ = ctl_tx.try_send(ctl(Msg::ServerStats(s)));
                    }
                    Some(Ev::ClipboardFiles(f)) if files_on => {
                        let paths: Vec<std::path::PathBuf> = f.paths.iter().map(std::path::PathBuf::from).collect();
                        let count = paths.len();
                        let items = tokio::task::spawn_blocking(move || user_items(&paths, clip_files_on)).await.unwrap_or_default();
                        match offers.offer_items(items) {
                            Some(o) => {
                                tracing::info!("host copied {} item(s) (offer {:016x})", o.files.len(), o.transfer_id);
                                let _ = ctl_tx.send(ctl(Msg::FileOffer(o))).await;
                            }
                            None => tracing::info!("nothing to offer from {count} copied item(s)"),
                        }
                    }
                    Some(Ev::ClipboardPaste(p)) => {
                        use nya_transport::clipfiles::Paste;
                        let id = p.transfer_id;
                        match incoming.paste(id) {
                            Paste::Ready(paths) => hub.send(paste_done(id, Ok(paths))),
                            Paste::Request => {
                                tracing::info!("fetching the client's files for a paste (offer {id:016x})");
                                let req = pb::FileRequest { transfer_id: id, purpose: pb::FilePurpose::Clipboard as i32 };
                                let _ = ctl_tx.send(ctl(Msg::FileRequest(req))).await;
                            }
                            Paste::Wait => {}
                            Paste::Failed(e) => hub.send(paste_done(id, Err(e))),
                            Paste::Unknown => hub.send(paste_done(id, Err("这批文件已过期，请在客户端重新复制".into()))),
                        }
                    }
                    Some(Ev::ClipboardImage(img)) if images_on => {
                        let link = link.clone();
                        tokio::spawn(async move {
                            let h = pb::FileHeader {
                                transfer_id: rand::random(),
                                name: "clipboard.dib".into(),
                                size: img.dib.len() as u64,
                                purpose: pb::FilePurpose::ClipboardImage as i32,
                                index: 0,
                                count: 1,
                                path: String::new(),
                            };
                            if let Err(e) = link.send_bytes(h, &img.dib).await {
                                tracing::debug!("clipboard image: {e:#}");
                            }
                        });
                    }
                    Some(Ev::ClipboardFiles(_)) | Some(Ev::ClipboardImage(_)) => {}
                    Some(Ev::GamepadRumble(r)) => { let _ = ctl_tx.send(ctl(Msg::GamepadRumble(r))).await; }
                    Some(Ev::Clipboard(c)) => {
                        if clipboard_on {
                            let _ = ctl_tx.send(ctl(Msg::ClipboardText(c))).await;
                        }
                    }
                    None => {}
                }
            }
            _ = att.role.changed() => {
                let role = att.role.borrow_and_update().clone();
                let was = controlling.swap(role.controlling, Ordering::SeqCst);
                state.set_controlling(att.token, role.controlling);
                if roles_on {
                    let _ = ctl_tx.send(ctl(Msg::SessionRole(role.to_pb()))).await;
                }
                if role.controlling && !was {
                    // Taking over: the host follows this client's requests now.
                    tracing::info!("client operates the host now; applying its stream requests");
                    if replay.caps.is_some() { hub.push_caps(); }
                    if replay.audio { hub.send(Cmd::SetAudio(SetAudio { enabled: true })); }
                    for s in replay.starts.values() { hub.send(Cmd::StartStream(s.clone())); }
                    if let Some(f) = &replay.folders { apply_folders(&mount, f.clone(), conn, client_name, &ctl_tx, hub); }
                }
                if !role.controlling && was && replay.folders.as_ref().is_some_and(|f| !f.folders.is_empty()) {
                    // Someone else operates now: their files, not ours, belong on the host.
                    apply_folders(&mount, pb::SharedFolders::default(), conn, client_name, &ctl_tx, hub);
                }
            }
            p = prints.recv() => {
                // The operating client prints what the host prints.
                if let Ok(path) = p {
                    if print_on && controlling.load(Ordering::Relaxed) {
                        tokio::spawn(crate::print::send(link.clone(), path));
                    }
                }
            }
            _ = generation.changed(), if controlling.load(Ordering::Relaxed) => {
                // The helper restarted (session switch): replay the client's requests.
                tracing::info!("host restarted; replaying stream request");
                if replay.caps.is_some() { hub.push_caps(); }
                if replay.audio { hub.send(Cmd::SetAudio(SetAudio { enabled: true })); }
                for s in replay.starts.values() { hub.send(Cmd::StartStream(s.clone())); }
            }
            _ = att.kicked.notify.notified() => {
                let reason = att.kicked.reason();
                let _ = ctl_tx.send(ctl(Msg::Bye(pb::Bye { reason: reason.clone() }))).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                break Err(anyhow!("{reason}"));
            }
            e = conn.closed() => break Err(anyhow!("connection closed: {e}")),
        }
    };

    // Give attached USB devices back to the client, take its folders off the host.
    let _ = timeout(Duration::from_secs(8), async { usb.lock().await.detach_all().await }).await;
    if let Some(m) = mount.lock().await.take() {
        let point = m.point.clone();
        let _ = timeout(Duration::from_secs(12), tokio::task::spawn_blocking(move || drop(m))).await;
        announce_drive(hub, &point, false);
    }

    if let Some(t) = file_channel_task {
        t.abort();
    }
    if let Some(ch) = link.tcp() {
        ch.close().await;
    }
    drop(ctl_tx);
    video_task.abort();
    mic_task.abort();
    cursor_task.abort();
    input_task.abort();
    let _ = timeout(Duration::from_millis(200), writer).await;
    outcome
}

/// Video bytes handed to QUIC but not yet on the wire: `written` (video
/// accepted by QUIC) minus `udp_tx` (everything sent) plus `baseline`, the
/// non-video share of `udp_tx`. Overhead (control, audio, headers,
/// retransmissions) makes the estimate drift low; when it goes negative the
/// baseline is moved so that it reads 0.
fn backlog_estimate(written: u64, udp_tx: u64, baseline: &mut i64) -> u64 {
    let backlog = written as i64 - (udp_tx as i64 - *baseline);
    if backlog < 0 {
        *baseline -= backlog;
        0
    } else {
        backlog as u64
    }
}

/// How this connection's video travels (FEATURE_VIDEO_DATAGRAM).
struct DgramState {
    /// Frames go out as datagrams + FEC instead of on a stream.
    on: AtomicBool,
    /// Parity percentage, adapted to the loss the client reports.
    fec: std::sync::atomic::AtomicU32,
}

impl Default for DgramState {
    fn default() -> Self {
        Self { on: AtomicBool::new(false), fec: nya_transport::videodgram::DEFAULT_FEC_PERCENT.into() }
    }
}

/// Datagrams or a stream, from the main window's request and the mode. A
/// switch asks for keyframes: frames on the two paths may arrive out of order.
fn choose_transport(d: &DgramState, supported: bool, replay: &Replay, hub: Option<&Hub>) {
    let Some(cfg) = replay.starts.get(&0).and_then(|s| s.config.as_ref()) else { return };
    let game = cfg.mode == pb::StreamMode::Game as i32;
    let on = supported
        && match pb::VideoTransport::try_from(cfg.video_transport) {
            Ok(pb::VideoTransport::Datagram) => true,
            Ok(pb::VideoTransport::Stream) => false,
            _ => game,
        };
    if d.on.swap(on, Ordering::Relaxed) != on {
        tracing::info!("video now {}", if on { "as datagrams with FEC" } else { "on a stream" });
        if let Some(hub) = hub {
            for &slot in replay.starts.keys() {
                hub.send(Cmd::RequestKeyframe(pb::RequestKeyframe { slot }));
            }
        }
    }
}

/// Writes frames on one uni stream per video stream id, or as datagrams with
/// FEC; acknowledges each frame to the host once quinn accepted it (flow
/// control, §6.2). With FEATURE_MULTI_STREAM several streams run at once
/// (one per slot) and the prelude (or the datagram header) names the slot.
async fn video_writer(
    conn: Connection,
    mut rx: mpsc::Receiver<crate::ipc_pb::VideoFrame>,
    hub: Arc<Hub>,
    written: Arc<std::sync::atomic::AtomicU64>,
    multi: bool,
    controlling: Arc<AtomicBool>,
    dgram: Arc<DgramState>,
) -> Result<()> {
    // slot -> (stream id, QUIC stream)
    let mut open: std::collections::HashMap<u32, (u64, SendStream)> = Default::default();
    let (mut frames, mut bytes, mut since) = (0u64, 0u64, std::time::Instant::now());
    let mut warned = false;
    while let Some(f) = rx.recv().await {
        let slot = if multi { f.slot } else { 0 };
        let max = conn.max_datagram_size().filter(|&m| m >= nya_transport::videodgram::HEADER_LEN + 64);
        if let (true, Some(max)) = (dgram.on.load(Ordering::Relaxed), max) {
            let mut frame = Vec::with_capacity(f.header.len() + f.data.len());
            frame.extend_from_slice(&f.header);
            frame.extend_from_slice(&f.data);
            let fec = dgram.fec.load(Ordering::Relaxed);
            let shards = nya_transport::videodgram::split(slot as u8, f.stream_id, f.frame_id, &frame, max, fec)?;
            let mut len = 0u64;
            for d in shards {
                len += d.len() as u64;
                // Waits while quinn's datagram buffer is full (congestion).
                if let Err(e) = conn.send_datagram_wait(d.into()).await {
                    match e {
                        quinn::SendDatagramError::ConnectionLost(e) => return Err(e.into()),
                        e if !warned => {
                            warned = true;
                            tracing::warn!("video datagram: {e}");
                        }
                        _ => {}
                    }
                }
            }
            if controlling.load(Ordering::Relaxed) {
                hub.send(Cmd::FrameSent(FrameSent { frame_id: f.frame_id, stream_id: f.stream_id }));
            }
            written.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
            frames += 1;
            bytes += len;
            continue;
        }
        if open.get(&slot).map(|c| c.0) != Some(f.stream_id) {
            tracing::info!("opening video stream {} (slot {slot}) to client (first frame {} bytes)", f.stream_id, f.data.len());
            if let Some((_, mut old)) = open.remove(&slot) {
                let _ = old.finish();
            }
            let mut s = conn.open_uni().await?;
            s.set_priority(1)?;
            let mut prelude = Vec::new();
            encode_varint(stream_type::VIDEO, &mut prelude);
            encode_varint(f.stream_id, &mut prelude);
            if multi {
                encode_varint(slot as u64, &mut prelude);
            }
            s.write_all(&prelude).await?;
            open.insert(slot, (f.stream_id, s));
        }
        let s = &mut open.get_mut(&slot).unwrap().1;
        let len = (f.header.len() + f.data.len()) as u32;
        s.write_all(&len.to_le_bytes()).await?;
        s.write_all(&f.header).await?;
        s.write_all(&f.data).await?;
        // The host paces itself on the operating client only.
        if controlling.load(Ordering::Relaxed) {
            hub.send(Cmd::FrameSent(FrameSent { frame_id: f.frame_id, stream_id: f.stream_id }));
        }
        written.fetch_add(len as u64 + 4, std::sync::atomic::Ordering::Relaxed);
        frames += 1;
        bytes += len as u64;
        if since.elapsed() >= Duration::from_secs(5) {
            tracing::info!("video sent (5 s): {frames} frames, {} KB", bytes / 1024);
            (frames, bytes, since) = (0, 0, std::time::Instant::now());
        }
    }
    Ok(())
}

async fn cursor_writer(conn: Connection, mut rx: mpsc::Receiver<pb::CursorMsg>) -> Result<()> {
    let mut s = conn.open_uni().await?;
    // Cursor updates are tiny and latency sensitive.
    s.set_priority(10)?;
    let mut prelude = Vec::new();
    encode_varint(stream_type::CURSOR, &mut prelude);
    s.write_all(&prelude).await?;
    while let Some(m) = rx.recv().await {
        write_msg(&mut s, &m).await?;
    }
    Ok(())
}

/// Accept client uni streams; the input stream feeds the host.
/// The user's copied files as the user sees them (mapped drives, shares),
/// else as the service does.
fn user_items(paths: &[std::path::PathBuf], folders: bool) -> Vec<nya_transport::files::Item> {
    use nya_transport::clipfiles::Outgoing;
    let items = crate::winutil::as_console_user(|| Outgoing::expand(paths, folders));
    if items.is_empty() { Outgoing::expand(paths, folders) } else { items }
}

/// Opens offered files as the logged-on user (else as the service).
fn user_opener() -> nya_transport::files::Opener {
    Arc::new(|p: &std::path::Path| crate::winutil::as_console_user(|| std::fs::File::open(p)).or_else(|_| std::fs::File::open(p)))
}

/// Send files the client asked for (from a FileOffer).
async fn send_offered(
    link: nya_transport::files::FileLink,
    id: u64,
    items: Vec<nya_transport::files::Item>,
    purpose: pb::FilePurpose,
    ctl_tx: mpsc::Sender<pb::ControlMsg>,
) {
    let files = items.iter().filter(|i| !i.is_dir).count();
    let open = user_opener();
    if let Err(e) = nya_transport::clipfiles::send_items_with(&link, id, &items, purpose, Some(&open), |_, _| {}).await {
        tracing::warn!("sending offer {id:016x}: {e:#}");
        let _ = ctl_tx
            .send(ctl(Msg::FileResult(pb::FileResult {
                transfer_id: id,
                ok: false,
                message: format!("被控端发送文件失败：{e:#}"),
                saved_to: String::new(),
            })))
            .await;
        return;
    }
    tracing::info!("sent {files} offered file(s) to client ({purpose:?})");
}

fn paste_done(id: u64, r: Result<Vec<std::path::PathBuf>, String>) -> Cmd {
    let (paths, error) = match r {
        Ok(p) => (p.into_iter().map(|p| p.to_string_lossy().into_owned()).collect(), String::new()),
        Err(e) => (Vec::new(), e),
    };
    Cmd::ClipboardPasteDone(crate::ipc_pb::ClipboardPasteDone { transfer_id: id, paths, error })
}

struct FileCtx {
    ctl_tx: mpsc::Sender<pb::ControlMsg>,
    files_on: bool,
    images_on: bool,
    clip_files_on: bool,
    /// Client offers being pasted on the host.
    incoming: nya_transport::clipfiles::Incoming,
    /// transfer id -> files received so far
    batches: Arc<std::sync::Mutex<std::collections::HashMap<u64, Vec<String>>>>,
    /// This client operates the host (input and files are taken only then).
    controlling: Arc<AtomicBool>,
    /// Transfers cancelled by the client (FileCancel).
    cancels: Arc<nya_transport::files::Cancels>,
}

/// A FILE stream from the client: save an upload, or apply a clipboard image.
async fn receive_file(mut r: RecvStream, hub: Arc<Hub>, ctx: Arc<FileCtx>) -> Result<()> {
    let h = nya_transport::files::read_header(&mut r).await?;
    receive_file_body(h, &mut r, hub, ctx).await
}

/// A file from the client (QUIC FILE stream or the TCP file channel).
/// Returning without reading it refuses it.
async fn receive_file_body<R: tokio::io::AsyncRead + Unpin>(h: pb::FileHeader, r: &mut R, hub: Arc<Hub>, ctx: Arc<FileCtx>) -> Result<()> {
    use nya_transport::files;
    // Cancelled by the client: reads fail, partial files are removed.
    let r = &mut files::Cancellable::new(r, ctx.cancels.flag(h.transfer_id));
    match pb::FilePurpose::try_from(h.purpose).unwrap_or(pb::FilePurpose::Unspecified) {
        pb::FilePurpose::Save if ctx.files_on => {
            let dir = tokio::task::spawn_blocking(crate::winutil::receive_dir).await?;
            let result = files::receive_to_dir(r, &h, &dir, |_| {}).await;
            let done_batch = {
                let mut b = ctx.batches.lock().unwrap();
                if let Ok(p) = &result {
                    b.entry(h.transfer_id).or_default().push(p.to_string_lossy().into_owned());
                }
                if h.index + 1 >= h.count || result.is_err() {
                    b.remove(&h.transfer_id)
                } else {
                    None
                }
            };
            let msg = match &result {
                Ok(_) => done_batch.map(|paths| {
                    tracing::info!("received {} file(s) into {}", paths.len(), dir.display());
                    let n = paths.len();
                    hub.send(Cmd::ClipboardFiles(crate::ipc_pb::ClipboardFiles { paths }));
                    pb::FileResult {
                        transfer_id: h.transfer_id,
                        ok: true,
                        message: format!("已保存 {n} 个文件（已放入被控端剪贴板）"),
                        saved_to: dir.to_string_lossy().into_owned(),
                    }
                }),
                Err(e) => Some(pb::FileResult {
                    transfer_id: h.transfer_id,
                    ok: false,
                    message: format!("接收 {} 失败：{e:#}", h.name),
                    saved_to: String::new(),
                }),
            };
            if let Some(m) = msg {
                let _ = ctx.ctl_tx.send(ctl(Msg::FileResult(m))).await;
            }
        }
        pb::FilePurpose::Clipboard if ctx.clip_files_on => {
            let Some(root) = ctx.incoming.root(h.transfer_id) else {
                return Ok(());
            };
            let result = files::receive_to_tree(r, &h, &root, |_| {}).await;
            let r = result.map(|_| ()).map_err(|e| format!("接收 {} 失败：{e:#}", if h.path.is_empty() { &h.name } else { &h.path }));
            if let Some(done) = ctx.incoming.file_done(h.transfer_id, r) {
                match &done {
                    Ok(p) => tracing::info!("client files for the paste are here: {} item(s) in {}", p.len(), root.display()),
                    Err(e) => tracing::warn!("paste of client files failed: {e}"),
                }
                hub.send(paste_done(h.transfer_id, done));
            }
        }
        pb::FilePurpose::ClipboardImage if ctx.images_on => {
            let dib = files::receive_to_vec(r, &h, files::MAX_IMAGE_BYTES).await?;
            hub.send(Cmd::ClipboardImage(crate::ipc_pb::ClipboardImage { dib }));
        }
        _ => {}
    }
    Ok(())
}

/// Accept client uni streams: input events and file transfers.
async fn client_streams(conn: Connection, hub: Arc<Hub>, ctx: Arc<FileCtx>) -> Result<()> {
    loop {
        let mut r = conn.accept_uni().await?;
        let hub = hub.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            match read_varint(&mut r).await {
                Ok(Some(stream_type::FILE)) if !ctx.controlling.load(Ordering::Relaxed) => {
                    let _ = r.stop(0u32.into());
                }
                Ok(Some(stream_type::FILE)) => {
                    if let Err(e) = receive_file(r, hub, ctx).await {
                        tracing::warn!("file stream: {e:#}");
                    }
                }
                Ok(Some(stream_type::INPUT)) => loop {
                    match read_msg::<pb::InputMsg, _>(&mut r, MAX_MESSAGE_LEN).await {
                        Ok(Some(m)) => {
                            if ctx.controlling.load(Ordering::Relaxed) {
                                hub.send(Cmd::Input(m));
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!("input stream: {e}");
                            break;
                        }
                    }
                },
                Ok(Some(other)) => {
                    tracing::debug!("unknown client stream type {other}");
                    let _ = r.stop(0u32.into());
                }
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    //! Loopback integration test of the whole network protocol with a fake
    //! host (no GPU involved).

    #[test]
    fn backlog_estimate_reanchors_without_running_away() {
        // Overhead only (audio, acks, headers): sent grows faster than video
        // written, for an hour of 250 ms ticks. The estimate stays 0, and the
        // baseline tracks the overhead instead of doubling until it wraps.
        let mut baseline = 1_000;
        let (mut written, mut sent) = (0u64, 1_000u64);
        for _ in 0..14_400 {
            written += 100_000;
            sent += 103_000;
            assert_eq!(super::backlog_estimate(written, sent, &mut baseline), 0);
        }
        assert_eq!(baseline, (sent - written) as i64);
        // Then 2 MB of video queues up: reported as such.
        written += 2_000_000;
        sent += 50_000;
        assert_eq!(super::backlog_estimate(written, sent, &mut baseline), 1_950_000);
    }

    use super::*;
    use std::net::SocketAddr;

    use crate::auth::AuthStore;
    use crate::ipc_pb::{HostCommand, HostEvent, VideoFrame};
    use nya_proto::frame::VideoFrameHeader;
    use nya_proto::pb::input_msg::Ev as InEv;
    use nya_transport::pairing::PairingKey;
    use nya_transport::Identity;

    async fn fake_host(hub: Arc<Hub>, mut cmds: mpsc::UnboundedReceiver<HostCommand>, inputs: mpsc::UnboundedSender<pb::InputMsg>) {
        let mut sent_acks = 0;
        while let Some(HostCommand { cmd: Some(c) }) = cmds.recv().await {
            match c {
                Cmd::StartStream(s) => {
                    let started = pb::StreamStarted { display_id: s.display_id, stream_id: 7, encoder_name: "fake".into(), ..Default::default() };
                    hub.publish(HostEvent { ev: Some(Ev::StreamStarted(started)) }).await;
                    for i in 1..=3u64 {
                        let h = VideoFrameHeader { frame_id: i, width: 64, height: 64, codec: pb::Codec::H264 as u8, ..Default::default() };
                        let mut hb = Vec::new();
                        h.write(&mut hb);
                        let f = VideoFrame { stream_id: 7, frame_id: i, header: hb, data: vec![i as u8; 1000], slot: 0 };
                        hub.publish(HostEvent { ev: Some(Ev::Video(f)) }).await;
                    }
                }
                Cmd::FrameSent(_) => sent_acks += 1,
                Cmd::Input(m) => {
                    let _ = inputs.send(m);
                }
                Cmd::ClientGone(_) => assert!(sent_acks >= 3 || sent_acks == 0),
                _ => {}
            }
        }
    }

    /// One server identity for all tests: the server fingerprint is process-wide.
    fn server_identity() -> Identity {
        static ID: std::sync::OnceLock<Identity> = std::sync::OnceLock::new();
        let id = ID.get_or_init(|| Identity::generate().unwrap()).clone();
        let _ = SERVER_FP.set(id.fingerprint());
        id
    }

    struct Client {
        _ep: quinn::Endpoint,
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
    }

    async fn connect(addr: SocketAddr, id: &Identity, pin: Fingerprint) -> (Client, pb::Welcome) {
        let ep = nya_transport::endpoint::client_endpoint(addr).unwrap();
        let conn = nya_transport::endpoint::connect(&ep, addr, id, Some(pin)).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = negotiate::hello(&LocalVersion::current(), "test", "0");
        write_msg(&mut send, &hello).await.unwrap();
        let reply: pb::HelloReply = expect_msg(&mut recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(pb::hello_reply::Reply::Welcome(w)) = reply.reply else { panic!("rejected") };
        (Client { _ep: ep, conn, send, recv }, w)
    }

    /// [`connect`] over TCP (QUIC over TCP).
    async fn connect_tcp(addr: SocketAddr, id: &Identity, pin: Fingerprint) -> (Client, pb::Welcome) {
        let ep = nya_transport::tcptunnel::client_endpoint(addr).await.unwrap();
        let conn = nya_transport::endpoint::connect(&ep, addr, id, Some(pin)).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = negotiate::hello(&LocalVersion::current(), "test", "0");
        write_msg(&mut send, &hello).await.unwrap();
        let reply: pb::HelloReply = expect_msg(&mut recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(pb::hello_reply::Reply::Welcome(w)) = reply.reply else { panic!("rejected") };
        (Client { _ep: ep, conn, send, recv }, w)
    }

    async fn pair(c: &mut Client, id: &Identity, server_fp: Fingerprint, key: &PairingKey) -> pb::AuthResult {
        let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(Msg::AuthChallenge(ch)) = m.msg else { panic!("no challenge") };
        let cn = pairing::nonce();
        let t = Transcript { server_nonce: &ch.server_nonce, client_nonce: &cn, server_fp, client_fp: id.fingerprint() };
        write_msg(&mut c.send, &ctl(Msg::AuthResponse(pb::AuthResponse { client_nonce: cn.to_vec(), mac: t.client_mac(key) })))
            .await
            .unwrap();
        let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(Msg::AuthResult(r)) = m.msg else { panic!("no result") };
        if r.ok {
            assert!(t.verify_server(key, &r.server_mac));
        }
        r
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handshake_pairing_stream_and_input() {
        let dir = std::env::temp_dir().join(format!("nya-net-test-{}", nya_proto::now_us()));
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        let (in_tx, mut in_rx) = mpsc::unbounded_channel();
        tokio::spawn(fake_host(hub.clone(), cmd_rx, in_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let cfg = crate::config::ServerConfig { name: "test-host".into(), ..Default::default() };
        let state = State::new(cpb::Mode::Standalone, dir.clone(), cfg, server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state.clone()));

        let client_id = Identity::generate().unwrap();

        // 1. Wrong pairing code is refused.
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.needs_pairing);
        let r = pair(&mut c, &client_id, server_fp, &PairingKey::generate()).await;
        assert!(!r.ok);

        // 2. Correct code pairs.
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.needs_pairing);
        assert_eq!(w.server_name, "test-host");
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");

        // 3. Paired client skips pairing; stream + input work.
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(!w.needs_pairing);
        write_msg(&mut c.send, &ctl(Msg::ClientCaps(pb::ClientCaps::default()))).await.unwrap();
        write_msg(&mut c.send, &ctl(Msg::StartStream(pb::StartStream { display_id: 1, ..Default::default() }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
            if let Some(Msg::StreamStarted(s)) = m.msg {
                assert_eq!(s.stream_id, 7);
                break;
            }
        }
        // Streams arrive in any order (the cursor stream opens at session start).
        let mut r = loop {
            let mut r = c.conn.accept_uni().await.unwrap();
            match read_varint(&mut r).await.unwrap() {
                Some(stream_type::VIDEO) => break r,
                Some(stream_type::CURSOR) => continue,
                other => panic!("unexpected stream type {other:?}"),
            }
        };
        assert_eq!(read_varint(&mut r).await.unwrap(), Some(7));
        assert_eq!(read_varint(&mut r).await.unwrap(), Some(0), "slot (multi-stream client)");
        for i in 1..=3u64 {
            let mut len = [0u8; 4];
            r.read_exact(&mut len).await.unwrap();
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            r.read_exact(&mut buf).await.unwrap();
            let (h, payload) = VideoFrameHeader::parse(&buf).unwrap();
            assert_eq!(h.frame_id, i);
            assert_eq!(payload.len(), 1000);
        }

        let mut input = c.conn.open_uni().await.unwrap();
        let mut prelude = Vec::new();
        encode_varint(stream_type::INPUT, &mut prelude);
        input.write_all(&prelude).await.unwrap();
        let key_msg = pb::InputMsg { ev: Some(InEv::Key(pb::Key { scancode: 0x1e, extended: false, down: true })) };
        write_msg(&mut input, &key_msg).await.unwrap();
        let got = timeout(Duration::from_secs(5), in_rx.recv()).await.unwrap().unwrap();
        assert_eq!(got, key_msg);

        // Ping/pong.
        write_msg(&mut c.send, &ctl(Msg::Ping(pb::Ping { t_us: 42 }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
            if let Some(Msg::Pong(p)) = m.msg {
                assert_eq!(p.t_us, 42);
                break;
            }
        }

        // The control pipe's view: the session and what happened before it.
        let st = state.status(true);
        let s = st.session.expect("session in status");
        assert_eq!(s.client_name, "test");
        assert_eq!(s.fingerprint, client_id.fingerprint().to_hex());
        let kinds: Vec<i32> = st.recent.iter().map(|e| e.kind).collect();
        for k in [Kind::PairingFailed, Kind::Paired, Kind::Connected] {
            assert!(kinds.contains(&(k as i32)), "missing {k:?} in {kinds:?}");
        }
        assert!(state.status(false).session.unwrap().remote_addr.is_empty());

        // Disconnecting from the control side ends the session with a Bye.
        assert!(state.hub.kick("管理员断开了连接"));
        loop {
            let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
            if let Some(Msg::Bye(b)) = m.msg {
                assert_eq!(b.reason, "管理员断开了连接");
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host that streams two frames per started slot and reports what it saw.
    async fn multi_host(hub: Arc<Hub>, mut cmds: mpsc::UnboundedReceiver<HostCommand>, seen: mpsc::UnboundedSender<String>) {
        while let Some(HostCommand { cmd: Some(c) }) = cmds.recv().await {
            match c {
                Cmd::StartStream(s) => {
                    let stream_id = 100 + s.slot as u64;
                    let started = pb::StreamStarted { display_id: s.display_id, stream_id, slot: s.slot, ..Default::default() };
                    hub.publish(HostEvent { ev: Some(Ev::StreamStarted(started)) }).await;
                    for i in 1..=2u64 {
                        let h = VideoFrameHeader { frame_id: i, width: 64, height: 64, codec: pb::Codec::H264 as u8, ..Default::default() };
                        let mut hb = Vec::new();
                        h.write(&mut hb);
                        let f = VideoFrame { stream_id, frame_id: i, header: hb, data: vec![s.slot as u8; 100], slot: s.slot };
                        hub.publish(HostEvent { ev: Some(Ev::Video(f)) }).await;
                    }
                }
                Cmd::FrameSent(f) => {
                    let _ = seen.send(format!("sent {} {}", f.stream_id, f.frame_id));
                }
                Cmd::StopStream(s) => {
                    let _ = seen.send(format!("stop {}", s.slot));
                }
                _ => {}
            }
        }
    }

    /// Two windows: two slots stream at once on separate QUIC streams whose
    /// prelude names the slot; acknowledgements go to the right stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_streams_at_once() {
        let dir = std::env::temp_dir().join(format!("nya-multi-test-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(multi_host(hub.clone(), cmd_rx, seen_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::MultiStream as u32)));

        for slot in [0u32, 1] {
            let s = pb::StartStream { display_id: 1 + slot, slot, ..Default::default() };
            write_msg(&mut c.send, &ctl(Msg::StartStream(s))).await.unwrap();
        }
        let mut started = std::collections::BTreeMap::new();
        while started.len() < 2 {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(Msg::StreamStarted(s)) = m.msg {
                started.insert(s.slot, s.stream_id);
            }
        }
        assert_eq!(started, [(0, 100), (1, 101)].into_iter().collect());

        // Two video streams, each: stream id, slot, then its frames.
        let mut got = std::collections::BTreeMap::new();
        while got.len() < 2 {
            let mut r = timeout(Duration::from_secs(5), c.conn.accept_uni()).await.unwrap().unwrap();
            if read_varint(&mut r).await.unwrap() != Some(stream_type::VIDEO) {
                continue;
            }
            let stream_id = read_varint(&mut r).await.unwrap().unwrap();
            let slot = read_varint(&mut r).await.unwrap().unwrap() as u32;
            let mut len = [0u8; 4];
            r.read_exact(&mut len).await.unwrap();
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            r.read_exact(&mut buf).await.unwrap();
            let (_, payload) = VideoFrameHeader::parse(&buf).unwrap();
            assert!(payload.iter().all(|&b| b == slot as u8), "frames of slot {slot} on its own stream");
            got.insert(slot, stream_id);
        }
        assert_eq!(got, started);

        // Every frame was acknowledged to its own pipeline.
        let mut acks = std::collections::BTreeSet::new();
        while acks.len() < 4 {
            let s = timeout(Duration::from_secs(5), seen.recv()).await.unwrap().unwrap();
            if s.starts_with("sent ") {
                acks.insert(s);
            }
        }
        assert!(acks.contains("sent 100 2") && acks.contains("sent 101 2"), "{acks:?}");

        // Closing the extra window stops only its stream.
        write_msg(&mut c.send, &ctl(Msg::StopStream(pb::StopStream { slot: 1 }))).await.unwrap();
        loop {
            let s = timeout(Duration::from_secs(5), seen.recv()).await.unwrap().unwrap();
            if s.starts_with("stop") {
                assert_eq!(s, "stop 1");
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host that reports the requests reaching it.
    async fn role_host(hub: Arc<Hub>, mut cmds: mpsc::UnboundedReceiver<HostCommand>, seen: mpsc::UnboundedSender<String>) {
        while let Some(HostCommand { cmd: Some(c) }) = cmds.recv().await {
            match c {
                Cmd::StartStream(s) => {
                    let _ = seen.send(format!("start {}", s.display_id));
                    let started = pb::StreamStarted { display_id: s.display_id, stream_id: s.display_id as u64, ..Default::default() };
                    hub.publish(HostEvent { ev: Some(Ev::StreamStarted(started)) }).await;
                }
                Cmd::RequestKeyframe(k) => {
                    let _ = seen.send(format!("keyframe {}", k.slot));
                }
                Cmd::ControllerChanged(_) => {
                    let _ = seen.send("controller".into());
                }
                _ => {}
            }
        }
    }

    async fn next_ctl(c: &mut Client, mut want: impl FnMut(&Msg) -> bool) -> Msg {
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(m) = m.msg {
                if want(&m) {
                    return m;
                }
            }
        }
    }

    async fn next_seen(seen: &mut mpsc::UnboundedReceiver<String>) -> String {
        timeout(Duration::from_secs(5), seen.recv()).await.unwrap().unwrap()
    }

    /// A host sending large frames: three when a stream starts, one per keyframe request.
    async fn big_frame_host(hub: Arc<Hub>, mut cmds: mpsc::UnboundedReceiver<HostCommand>) {
        let mut next = 1u64;
        let send = |hub: Arc<Hub>, n: u64| async move {
            let h = VideoFrameHeader { frame_id: n, width: 64, height: 64, codec: pb::Codec::H264 as u8, ..Default::default() };
            let mut hb = Vec::new();
            h.write(&mut hb);
            let data: Vec<u8> = (0..60_000u32).map(|i| (i as u64 * 7 + n) as u8).collect();
            hub.publish(HostEvent { ev: Some(Ev::Video(VideoFrame { stream_id: 5, frame_id: n, header: hb, data, slot: 0 })) }).await;
        };
        while let Some(HostCommand { cmd: Some(c) }) = cmds.recv().await {
            match c {
                Cmd::StartStream(s) => {
                    let started = pb::StreamStarted { display_id: s.display_id, stream_id: 5, ..Default::default() };
                    hub.publish(HostEvent { ev: Some(Ev::StreamStarted(started)) }).await;
                    for _ in 0..3 {
                        send(hub.clone(), next).await;
                        next += 1;
                    }
                }
                Cmd::RequestKeyframe(_) => {
                    send(hub.clone(), next).await;
                    next += 1;
                }
                _ => {}
            }
        }
    }

    /// Shared folders without WinFsp on the host: the client hears why, the
    /// session goes on. (Skipped where WinFsp is installed: it would mount a drive.)
    #[tokio::test(flavor = "multi_thread")]
    async fn shared_folders_report_a_missing_winfsp() {
        if nya_server_core::components::winfsp_dll().is_some() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("nya-folders-test-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        tokio::spawn(big_frame_host(hub.clone(), cmd_rx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::FolderMount as u32)));

        let f = pb::SharedFolders { folders: vec![pb::SharedFolder { name: "文档".into(), read_only: false }] };
        write_msg(&mut c.send, &ctl(Msg::SharedFolders(f))).await.unwrap();
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(Msg::FolderMountStatus(s)) = m.msg {
                assert!(!s.mounted);
                assert!(s.message.contains("WinFsp"), "{}", s.message);
                break;
            }
        }
        // Still alive.
        write_msg(&mut c.send, &ctl(Msg::Ping(pb::Ping { t_us: 7 }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(Msg::Pong(p)) = m.msg {
                assert_eq!(p.t_us, 7);
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A finished print job goes to the operating client as a PRINT file and
    /// is deleted on the host.
    #[tokio::test(flavor = "multi_thread")]
    async fn print_jobs_reach_the_operating_client() {
        let dir = std::env::temp_dir().join(format!("nya-print-test-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        tokio::spawn(big_frame_host(hub.clone(), cmd_rx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state.clone()));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::Print as u32)));
        // Wait until the session runs (it answers a ping), then "print".
        write_msg(&mut c.send, &ctl(Msg::Ping(pb::Ping { t_us: 1 }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if matches!(m.msg, Some(Msg::Pong(_))) {
                break;
            }
        }
        let job = dir.join("被控端打印 test.pdf");
        std::fs::write(&job, b"%PDF-1.4 fake").unwrap();
        state.prints.send(job.clone()).unwrap();
        let mut r = loop {
            let mut r = timeout(Duration::from_secs(5), c.conn.accept_uni()).await.unwrap().unwrap();
            if read_varint(&mut r).await.unwrap() == Some(stream_type::FILE) {
                break r;
            }
        };
        let h = nya_transport::files::read_header(&mut r).await.unwrap();
        assert_eq!(h.purpose, pb::FilePurpose::Print as i32);
        assert_eq!(h.name, "被控端打印 test.pdf");
        let body = nya_transport::files::receive_to_vec(&mut r, &h, 1 << 20).await.unwrap();
        assert_eq!(body, b"%PDF-1.4 fake");
        for _ in 0..50 {
            if !job.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!job.exists(), "sent jobs are deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Game mode with FEATURE_VIDEO_DATAGRAM: frames arrive as FEC datagrams
    /// and rebuild intact; switching to office mode moves them to a stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn game_mode_video_travels_as_datagrams() {
        let dir = std::env::temp_dir().join(format!("nya-dgram-test-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        tokio::spawn(big_frame_host(hub.clone(), cmd_rx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::VideoDatagram as u32)));

        let config = pb::StreamConfig { mode: pb::StreamMode::Game as i32, ..Default::default() };
        let s = pb::StartStream { display_id: 1, config: Some(config), ..Default::default() };
        write_msg(&mut c.send, &ctl(Msg::StartStream(s))).await.unwrap();

        let mut re = nya_transport::videodgram::Reassembler::new(nya_proto::MAX_VIDEO_FRAME_LEN);
        let mut frames = Vec::new();
        while frames.len() < 3 {
            let d = timeout(Duration::from_secs(5), c.conn.read_datagram()).await.unwrap().unwrap();
            assert_eq!(d[0], nya_proto::frame::datagram_type::VIDEO);
            frames.extend(re.push(&d));
        }
        for (i, f) in frames.iter().enumerate() {
            let n = i as u64 + 1;
            let (h, payload) = VideoFrameHeader::parse(&f.data).unwrap();
            assert_eq!((f.stream_id, f.frame_id, h.frame_id), (5, n, n));
            assert_eq!(payload.len(), 60_000);
            assert!(payload.iter().enumerate().all(|(i, &b)| b == (i as u64 * 7 + n) as u8));
        }
        let st = re.take_stats();
        assert!(st.frames_completed == 3 && st.shards_received >= 3 * 40, "{st:?}");
        assert_eq!(st.frames_lost, 0);

        // The client reports loss: the host adds parity, and says so.
        let report = pb::ClientStats { video_shards_received: 900, video_shards_lost: 100, ..Default::default() };
        write_msg(&mut c.send, &ctl(Msg::ClientStats(report))).await.unwrap();

        // Office mode: back to a stream, starting with a fresh keyframe.
        write_msg(&mut c.send, &ctl(Msg::SetMode(pb::SetMode { mode: pb::StreamMode::Office as i32 }))).await.unwrap();
        let mut r = loop {
            let mut r = timeout(Duration::from_secs(5), c.conn.accept_uni()).await.unwrap().unwrap();
            if read_varint(&mut r).await.unwrap() == Some(stream_type::VIDEO) {
                break r;
            }
        };
        assert_eq!(read_varint(&mut r).await.unwrap(), Some(5));
        assert_eq!(read_varint(&mut r).await.unwrap(), Some(0));
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await.unwrap();
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(VideoFrameHeader::parse(&buf).unwrap().0.frame_id, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two clients: the first operates the host, the second watches the
    /// running picture (its requests ignored) until it takes over.
    #[tokio::test(flavor = "multi_thread")]
    async fn second_client_watches_then_takes_over() {
        let dir = std::env::temp_dir().join(format!("nya-roles-test-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(role_host(hub.clone(), cmd_rx, seen_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state.clone()));
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        for id in [&a_id, &b_id] {
            let (mut c, _) = connect(addr, id, server_fp).await;
            assert!(pair(&mut c, id, server_fp, &key).await.ok);
            c.conn.close(0u32.into(), b"");
        }
        // Until the pairing connections are gone (they attach like any session).
        for _ in 0..100 {
            let st = state.status(true);
            if st.session.is_none() && st.viewers.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        while seen.try_recv().is_ok() {}

        // A operates and streams display 1.
        let (mut a, w) = connect(addr, &a_id, server_fp).await;
        assert!(w.features.contains(&(Feature::MultiClient as u32)));
        let Msg::SessionRole(r) = next_ctl(&mut a, |m| matches!(m, Msg::SessionRole(_))).await else { unreachable!() };
        assert!(r.controlling);
        write_msg(&mut a.send, &ctl(Msg::StartStream(pb::StartStream { display_id: 1, ..Default::default() }))).await.unwrap();
        assert_eq!(next_seen(&mut seen).await, "start 1");

        // B joins: it watches, gets the running stream and a fresh keyframe.
        let (mut b, _) = connect(addr, &b_id, server_fp).await;
        let Msg::StreamStarted(s) = next_ctl(&mut b, |m| matches!(m, Msg::StreamStarted(_))).await else { unreachable!() };
        assert_eq!(s.display_id, 1);
        assert_eq!(next_seen(&mut seen).await, "keyframe 0");
        let Msg::SessionRole(r) = next_ctl(&mut b, |m| matches!(m, Msg::SessionRole(_))).await else { unreachable!() };
        assert!(!r.controlling);
        assert_eq!(r.viewers.len(), 1);
        let Msg::SessionRole(r) = next_ctl(&mut a, |m| matches!(m, Msg::SessionRole(_))).await else { unreachable!() };
        assert!(r.controlling && r.viewers.len() == 1, "A sees its viewer");
        assert_eq!(state.status(true).viewers.len(), 1);

        // B's own request is kept but does not reach the host until it takes over.
        write_msg(&mut b.send, &ctl(Msg::StartStream(pb::StartStream { display_id: 2, ..Default::default() }))).await.unwrap();
        write_msg(&mut b.send, &ctl(Msg::TakeControl(pb::TakeControl { kick: false }))).await.unwrap();
        assert_eq!(next_seen(&mut seen).await, "controller");
        assert_eq!(next_seen(&mut seen).await, "start 2");
        let Msg::SessionRole(r) = next_ctl(&mut a, |m| matches!(m, Msg::SessionRole(_))).await else { unreachable!() };
        assert!(!r.controlling, "A watches now");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host whose user pastes the client's files as soon as they are offered.
    async fn pasting_host(hub: Arc<Hub>, mut cmds: mpsc::UnboundedReceiver<HostCommand>, done: mpsc::UnboundedSender<crate::ipc_pb::ClipboardPasteDone>) {
        while let Some(HostCommand { cmd: Some(c) }) = cmds.recv().await {
            match c {
                Cmd::ClipboardOffer(o) => {
                    let paste = crate::ipc_pb::ClipboardPaste { transfer_id: o.transfer_id };
                    hub.publish(HostEvent { ev: Some(Ev::ClipboardPaste(paste)) }).await;
                }
                Cmd::ClipboardPasteDone(d) => {
                    let _ = done.send(d);
                }
                _ => {}
            }
        }
    }

    /// A whole session over TCP (QUIC over TCP): the host's TCP listener
    /// hands it to the second endpoint, which serves it like a UDP one.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_over_tcp() {
        let dir = std::env::temp_dir().join(format!("nya-over-tcp-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        tokio::spawn(big_frame_host(hub.clone(), cmd_rx));
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let tunnel = nya_transport::tcptunnel::TunnelSocket::new(addr);
        let ep = nya_transport::tcptunnel::server_endpoint(tunnel.clone(), &server_id).unwrap();
        let (sid, expected) = (server_id.clone(), state.file_channels.clone());
        tokio::spawn(async move { nya_transport::filechan::listen(addr, &sid, expected, Some(tunnel)).await.unwrap() });
        tokio::spawn(serve_endpoint(ep, state));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect_tcp(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok, "pairing over TCP");
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect_tcp(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::TcpFiles as u32)));
        write_msg(&mut c.send, &ctl(Msg::Ping(pb::Ping { t_us: 42 }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(Msg::Pong(p)) = m.msg {
                assert_eq!(p.t_us, 42);
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The client cancels files the host is pasting: the paste fails at
    /// once and what had arrived is removed.
    #[tokio::test(flavor = "multi_thread")]
    async fn client_cancels_a_paste() {
        let dir = std::env::temp_dir().join(format!("nya-cancel-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        tokio::spawn(pasting_host(hub.clone(), cmd_rx, done_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, _) = connect(addr, &client_id, server_fp).await;

        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"first").unwrap();
        std::fs::write(src.join("b.bin"), vec![0u8; 1 << 20]).unwrap();
        let mut outgoing = nya_transport::clipfiles::Outgoing::default();
        let offer = outgoing.offer(&[src.join("a.txt"), src.join("b.bin")], true).unwrap();
        let id = offer.transfer_id;
        write_msg(&mut c.send, &ctl(Msg::FileOffer(offer))).await.unwrap();
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if matches!(m.msg, Some(Msg::FileRequest(ref r)) if r.transfer_id == id) {
                break;
            }
        }
        // One file arrives, then the client cancels.
        let items = outgoing.items(id).unwrap();
        nya_transport::clipfiles::send_items(&c.conn, id, &items[..1], pb::FilePurpose::Clipboard, |_, _| {}).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        write_msg(&mut c.send, &ctl(Msg::FileCancel(pb::FileCancel { transfer_id: id }))).await.unwrap();
        let done = timeout(Duration::from_secs(5), done_rx.recv()).await.unwrap().unwrap();
        assert_eq!(done.error, "客户端取消了传输");
        assert!(done.paths.is_empty());
        let cache = crate::winutil::clipboard_cache_dir().join(format!("{id:016x}"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!cache.exists(), "the paste's cache folder is removed: {}", cache.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FEATURE_TCP_FILES: the session hands the client a token, the client
    /// opens the TCP file channel on the same port, and the pasted files
    /// travel over it.
    #[tokio::test(flavor = "multi_thread")]
    async fn clipboard_files_over_tcp_file_channel() {
        let dir = std::env::temp_dir().join(format!("nya-tcp-files-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        tokio::spawn(pasting_host(hub.clone(), cmd_rx, done_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        let (sid, expected) = (server_id.clone(), state.file_channels.clone());
        tokio::spawn(async move { nya_transport::filechan::listen(addr, &sid, expected, None).await.unwrap() });
        tokio::spawn(serve_endpoint(ep, state));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::TcpFiles as u32)));

        // The host offers the file channel; the client opens it.
        let token = loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(Msg::FileChannel(f)) = m.msg {
                break f.token;
            }
        };
        let on_file: nya_transport::filechan::OnFile = Arc::new(|_, _| {});
        let ch = nya_transport::filechan::connect(c.conn.remote_address(), &client_id, server_fp, &token, on_file).await.unwrap();
        let link = nya_transport::files::FileLink::new(c.conn.clone());
        link.set_tcp(Some(ch));

        let src = dir.join("src");
        std::fs::create_dir_all(src.join("docs")).unwrap();
        std::fs::write(src.join("docs").join("a.txt"), b"over tcp").unwrap();
        let big: Vec<u8> = (0..2_000_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(src.join("big.bin"), &big).unwrap();
        let mut outgoing = nya_transport::clipfiles::Outgoing::default();
        let offer = outgoing.offer(&[src.join("docs"), src.join("big.bin")], true).unwrap();
        let id = offer.transfer_id;
        write_msg(&mut c.send, &ctl(Msg::FileOffer(offer))).await.unwrap();
        loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if matches!(m.msg, Some(Msg::FileRequest(ref r)) if r.transfer_id == id) {
                break;
            }
        }
        assert!(link.tcp().is_some(), "sent over the TCP file channel");
        let items = outgoing.items(id).unwrap();
        nya_transport::clipfiles::send_items_with(&link, id, &items, pb::FilePurpose::Clipboard, None, |_, _| {}).await.unwrap();

        let done = timeout(Duration::from_secs(10), done_rx.recv()).await.unwrap().unwrap();
        assert_eq!(done.error, "");
        let paths: Vec<std::path::PathBuf> = done.paths.iter().map(std::path::PathBuf::from).collect();
        assert_eq!(std::fs::read(paths[0].join("a.txt")).unwrap(), b"over tcp");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), big);
        let _ = std::fs::remove_dir_all(paths[0].parent().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Client copies a folder and a file; the host pastes: the service asks
    /// for the files, receives them into its paste cache and hands the host
    /// the top-level items.
    #[tokio::test(flavor = "multi_thread")]
    async fn clipboard_files_client_to_host() {
        let dir = std::env::temp_dir().join(format!("nya-clip-test-{}", nya_proto::now_us()));
        std::fs::create_dir_all(&dir).unwrap();
        let server_id = server_identity();
        let server_fp = server_id.fingerprint();
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key();
        let (hub, cmd_rx) = Hub::new();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        tokio::spawn(pasting_host(hub.clone(), cmd_rx, done_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        let state = State::new(cpb::Mode::Standalone, dir.clone(), Default::default(), server_fp.to_string(), auth, hub);
        tokio::spawn(serve_endpoint(ep, state));
        let client_id = Identity::generate().unwrap();
        let (mut c, _) = connect(addr, &client_id, server_fp).await;
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.features.contains(&(Feature::ClipboardFiles as u32)));

        // What the client copied.
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("docs").join("empty")).unwrap();
        std::fs::write(src.join("docs").join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("b.txt"), b"bye").unwrap();
        let mut outgoing = nya_transport::clipfiles::Outgoing::default();
        let offer = outgoing.offer(&[src.join("docs"), src.join("b.txt")], true).unwrap();
        let id = offer.transfer_id;
        write_msg(&mut c.send, &ctl(Msg::FileOffer(offer))).await.unwrap();

        // The paste makes the service request the files for the clipboard.
        let req = loop {
            let m: pb::ControlMsg = timeout(Duration::from_secs(5), expect_msg(&mut c.recv, MAX_MESSAGE_LEN)).await.unwrap().unwrap();
            if let Some(Msg::FileRequest(r)) = m.msg {
                break r;
            }
        };
        assert_eq!((req.transfer_id, req.purpose), (id, pb::FilePurpose::Clipboard as i32));
        let items = outgoing.items(id).unwrap();
        nya_transport::clipfiles::send_items(&c.conn, id, &items, pb::FilePurpose::Clipboard, |_, _| {}).await.unwrap();

        let done = timeout(Duration::from_secs(10), done_rx.recv()).await.unwrap().unwrap();
        assert_eq!(done.error, "");
        let paths: Vec<std::path::PathBuf> = done.paths.iter().map(std::path::PathBuf::from).collect();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("docs") && paths[1].ends_with("b.txt"), "{paths:?}");
        assert_eq!(std::fs::read(paths[0].join("a.txt")).unwrap(), b"hello");
        assert!(paths[0].join("empty").is_dir(), "empty folders are recreated");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"bye");
        let _ = std::fs::remove_dir_all(paths[0].parent().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
