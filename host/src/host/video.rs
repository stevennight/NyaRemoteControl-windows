//! Video thread: owns the GPU topology, probes encoders, builds/rebuilds the
//! pipelines and drives them. One pipeline per stream slot (client window:
//! slot 0 is the main window, others show further displays at the same
//! time); they take turns in this thread.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use nya_proto::pb;
use nya_win::desktop::DesktopTracker;
use nya_win::topology::Topology;

use super::input::InputCmd;
use super::pipeline::{Pipeline, Step};
use super::select::{self, EncoderProbe, Plan};
use super::vdisplay::{self, HostDisplays, Setup};
use super::{HostConfig, Sink};
use crate::ipc_pb::host_event::Ev;

pub enum VideoCmd {
    Start(pb::StartStream),
    /// Stop one slot's stream.
    Stop(u32),
    /// The client left: stop everything.
    StopAll,
    Keyframe(u32),
    SetMode(pb::StreamMode),
    Caps(pb::ClientCaps),
    /// (stream id, frame id) handed to the network.
    FrameSent(u64, u64),
    /// Total for all streams (adaptive bitrate); split between them.
    SetBitrate(u32),
    Shutdown,
}

/// One stream (client window).
struct Slot {
    req: pb::StartStream,
    pipe: Option<Pipeline>,
    retry_at: Option<Instant>,
    /// Rebuild before the next step.
    rebuild: bool,
    /// Frame ids continue across rebuilds of the slot's pipeline.
    frame_counter: u64,
    /// Plans that opened fine but failed while encoding; skipped after two failures.
    failed: HashMap<Plan, u32>,
}

impl Slot {
    fn new(req: pb::StartStream) -> Self {
        Self { req, pipe: None, retry_at: None, rebuild: true, frame_counter: 0, failed: HashMap::new() }
    }

    fn drop_pipe(&mut self) {
        if let Some(p) = self.pipe.take() {
            self.frame_counter = self.frame_counter.max(p.frame_id);
        }
    }
}

struct State {
    topo: Topology,
    probes: Vec<EncoderProbe>,
    caps: Option<pb::ClientCaps>,
    slots: std::collections::BTreeMap<u32, Slot>,
    next_stream_id: u64,
    /// Latest total bitrate from the network side.
    bitrate_total: Option<u32>,
    /// The session's virtual display, if it asked for one.
    vd: Option<HostDisplays>,
    /// Client gone: remove the virtual display at this time unless it comes
    /// back (network hiccup, reconnect).
    vd_release_at: Option<Instant>,
    vd_available: bool,
}

/// How long a virtual display outlives its client.
const VD_GRACE: Duration = Duration::from_secs(15);

fn session_info(st: &State, cfg: &HostConfig) -> pb::SessionInfo {
    let (topo, probes) = (&st.topo, &st.probes);
    pb::SessionInfo {
        virtual_display_available: st.vd_available,
        mic_device: super::mic::cable_device_name().unwrap_or_default(),
        usb_available: crate::usb::usbip_exe().is_some(),
        gamepad_available: super::gamepad::available(),
        host_name: cfg.name.clone(),
        displays: display_infos(st),
        gpus: topo
            .adapters
            .iter()
            .map(|a| {
                let probe = probes.iter().find(|p| p.adapter_index == a.index);
                pb::GpuInfo {
                    index: a.index,
                    name: a.name.clone(),
                    vendor_id: a.vendor_id,
                    luid: a.luid,
                    encoders: probe
                        .map(|p| {
                            p.caps
                                .iter()
                                .map(|&(c, yuv444)| pb::CodecCap {
                                    codec: select::to_pb_codec(c) as i32,
                                    chroma: if yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 } as i32,
                                    hardware: true,
                                    ..Default::default()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    encoder_backend: probe.map(|p| p.backend.name().to_owned()).unwrap_or_default(),
                }
            })
            .collect(),
    }
}

fn display_infos(st: &State) -> Vec<pb::DisplayInfo> {
    let virt = st.vd.as_ref().map(|v| v.gdi_names()).unwrap_or(&[]);
    st.topo
        .outputs
        .iter()
        .map(|o| pb::DisplayInfo {
            id: o.id,
            name: o.device_name.clone(),
            width: o.width(),
            height: o.height(),
            x: o.left,
            y: o.top,
            refresh_hz: o.refresh_hz,
            primary: o.primary,
            gpu_index: o.adapter_index,
            is_virtual: virt.iter().any(|n| n.eq_ignore_ascii_case(&o.device_name)),
            virtual_index: virt.iter().position(|n| n.eq_ignore_ascii_case(&o.device_name)).map_or(0, |i| i as u32 + 1),
            hdr: o.hdr,
        })
        .collect()
}

fn luids(t: &Topology) -> Vec<u64> {
    let mut v: Vec<u64> = t.adapters.iter().map(|a| a.luid).collect();
    v.sort();
    v
}

pub fn thread(rx: Receiver<VideoCmd>, sink: Sink, input_tx: Sender<InputCmd>, cfg: HostConfig) {
    nya_win::com_init();
    nya_win::mmcss_boost("Capture");
    let mut desktop = DesktopTracker::new();
    if let Err(e) = desktop.sync() {
        tracing::warn!("cannot attach to input desktop: {e:#}");
    }
    let topo = loop {
        match Topology::enumerate() {
            Ok(t) => break t,
            Err(e) => {
                tracing::error!("GPU enumeration failed: {e:#}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    let probes = select::probe_all(&topo);

    let mut st = State {
        topo,
        probes,
        caps: None,
        slots: Default::default(),
        next_stream_id: nya_proto::now_us(),
        bitrate_total: None,
        vd: None,
        vd_release_at: None,
        vd_available: vdisplay::available(),
    };
    sink.send(Ev::SessionInfo(session_info(&st, &cfg)));
    let mut last_topo_check = Instant::now();
    let mut last_vd_check = Instant::now();

    loop {
        // --- commands ---
        let running = st.slots.values().any(|x| x.pipe.is_some());
        let next_retry = st.slots.values().filter(|x| x.pipe.is_none()).filter_map(|x| x.retry_at).min();
        let first = if running {
            rx.try_recv().ok()
        } else {
            let wait = match next_retry {
                Some(at) => at.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(1),
            };
            match rx.recv_timeout(wait) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        for cmd in first.into_iter().chain(rx.try_iter()) {
            match cmd {
                VideoCmd::Start(s) => {
                    // The display setup is the main window's business.
                    if s.slot == 0 && apply_virtual_display(&mut st, &s, &sink) {
                        refresh_topology(&mut st, &sink, &cfg);
                        for x in st.slots.values_mut() {
                            x.drop_pipe();
                            x.rebuild = true;
                        }
                    }
                    match st.slots.get_mut(&s.slot) {
                        Some(x) => {
                            x.req = s;
                            x.rebuild = true;
                        }
                        None => {
                            tracing::info!("stream slot {} started", s.slot);
                            st.slots.insert(s.slot, Slot::new(s));
                        }
                    }
                    split_bitrate(&mut st);
                }
                VideoCmd::Stop(slot) => {
                    if let Some(mut x) = st.slots.remove(&slot) {
                        x.drop_pipe();
                        tracing::info!("stream slot {slot} stopped");
                    }
                    if slot == 0 {
                        stop_all(&mut st);
                    }
                    split_bitrate(&mut st);
                }
                VideoCmd::StopAll => stop_all(&mut st),
                VideoCmd::Keyframe(slot) => {
                    if let Some(p) = st.slots.get_mut(&slot).and_then(|x| x.pipe.as_mut()) {
                        p.request_keyframe();
                    }
                }
                VideoCmd::SetMode(m) => {
                    for x in st.slots.values_mut() {
                        let c = x.req.config.get_or_insert_with(Default::default);
                        if c.mode != m as i32 {
                            c.mode = m as i32;
                            // Let the server pick codec/chroma/bitrate/fps for the new mode.
                            c.chroma = 0;
                            c.fps = 0;
                            c.bitrate_kbps = 0;
                            x.rebuild = true;
                        }
                    }
                }
                VideoCmd::Caps(c) => {
                    // What every attached client decodes (the hub combines them): a
                    // watcher joining or leaving can change the best format.
                    let changed = st.caps.as_ref() != Some(&c);
                    st.caps = Some(c);
                    if changed {
                        let (probes, caps) = (&st.probes, st.caps.as_ref());
                        for x in st.slots.values_mut() {
                            let Some(p) = x.pipe.as_ref() else { continue };
                            let best = select::plans(probes, p.plan.capture_adapter, &x.req, caps, &cfg.encoder, p.plan.hdr).into_iter().next();
                            if best.as_ref().is_some_and(|b| (b.codec, b.yuv444, b.hdr) != (p.plan.codec, p.plan.yuv444, p.plan.hdr)) {
                                tracing::info!("attached clients decode differently now: slot {} {:?} -> {:?}", p.slot, p.plan, best);
                                x.rebuild = true;
                            }
                        }
                    }
                }

                VideoCmd::SetBitrate(k) => {
                    st.bitrate_total = Some(k);
                    split_bitrate(&mut st);
                }
                VideoCmd::FrameSent(stream_id, id) => {
                    if let Some(p) = st.slots.values_mut().filter_map(|x| x.pipe.as_mut()).find(|p| p.stream_id == stream_id) {
                        p.frame_sent(id);
                    }
                }
                VideoCmd::Shutdown => return,
            }
        }

        if st.vd_release_at.is_some_and(|t| Instant::now() >= t) {
            st.vd_release_at = None;
            st.vd = None;
            refresh_topology(&mut st, &sink, &cfg);
        }

        // --- topology changes (hot-plug, MUX switch, driver reset) ---
        if last_topo_check.elapsed() > Duration::from_secs(1) {
            last_topo_check = Instant::now();
            if !st.topo.is_current() {
                // A monitor plugged in (or Windows restoring a layout) may have
                // switched a physical display back on next to the virtual one.
                if let Some(vd) = st.vd.as_mut() {
                    match vd.enforce() {
                        Ok(true) => tracing::info!("physical display switched off again"),
                        Ok(false) => {}
                        Err(e) => tracing::warn!("virtual display layout: {e:#}"),
                    }
                }
                refresh_topology(&mut st, &sink, &cfg);
                for x in st.slots.values_mut() {
                    if x.pipe.is_some() {
                        x.rebuild = true;
                    }
                }
            }
        }

        // --- virtual display driver installed / removed in the manager ---
        // The helper outlives client sessions, and new clients get the last
        // SessionInfo: without this they would keep hearing "not installed".
        if last_vd_check.elapsed() > Duration::from_secs(5) {
            last_vd_check = Instant::now();
            let now = vdisplay::available();
            if now != st.vd_available {
                tracing::info!("virtual display driver {}", if now { "installed" } else { "removed" });
                st.vd_available = now;
                sink.send(Ev::SessionInfo(session_info(&st, &cfg)));
            }
        }

        // --- (re)build ---
        let ids: Vec<u32> = st.slots.keys().copied().collect();
        let mut built = false;
        for id in &ids {
            let x = &st.slots[id];
            let retry_due = x.retry_at.map_or(true, |t| Instant::now() >= t);
            if !(x.rebuild || (x.pipe.is_none() && retry_due)) {
                continue;
            }
            let x = st.slots.get_mut(id).unwrap();
            x.drop_pipe();
            x.rebuild = false;
            match build(&mut st, *id, &cfg, &mut desktop) {
                Ok(p) => {
                    sink.send(Ev::StreamStarted(p.started.clone()));
                    let _ = input_tx.send(InputCmd::SetRect(*id, p.rect));
                    let x = st.slots.get_mut(id).unwrap();
                    x.pipe = Some(p);
                    x.retry_at = None;
                    built = true;
                }
                Err(msg) => {
                    tracing::error!("cannot start stream (slot {id}): {msg}");
                    sink.send(Ev::StreamError(pb::StreamError { message: msg, slot: *id }));
                    st.slots.get_mut(id).unwrap().retry_at = Some(Instant::now() + Duration::from_secs(3));
                }
            }
        }
        if built {
            split_bitrate(&mut st);
        }

        // --- run ---
        let n = st.slots.values().filter(|x| x.pipe.is_some()).count().max(1) as u32;
        // Each pipeline may block this long waiting for its display.
        let max_wait = Duration::from_millis((16 / n as u64).max(2));
        let mut lost = false;
        for x in st.slots.values_mut() {
            let Some(p) = x.pipe.as_mut() else { continue };
            if let Step::Rebuild(why) = p.step(&sink, &mut desktop, max_wait) {
                tracing::warn!("rebuilding stream (slot {}): {why}", p.slot);
                if why.starts_with("encode failed") {
                    *x.failed.entry(p.plan.clone()).or_default() += 1;
                }
                x.drop_pipe();
                x.retry_at = Some(Instant::now() + Duration::from_millis(300));
                lost = true;
            }
        }
        if lost {
            refresh_topology(&mut st, &sink, &cfg);
        }
    }
}

/// The client left: all streams stop; the virtual display outlives it briefly.
fn stop_all(st: &mut State) {
    for (_, mut x) in std::mem::take(&mut st.slots) {
        x.drop_pipe();
    }
    st.bitrate_total = None;
    if st.vd.is_some() {
        st.vd_release_at = Some(Instant::now() + VD_GRACE);
    }
}

/// The adaptive bitrate is a total for the connection: share it between the
/// streams (each still capped at its own configured rate).
fn split_bitrate(st: &mut State) {
    let Some(total) = st.bitrate_total else { return };
    let n = st.slots.values().filter(|x| x.pipe.is_some()).count().max(1) as u32;
    for p in st.slots.values_mut().filter_map(|x| x.pipe.as_mut()) {
        p.set_bitrate(total / n);
    }
}

fn refresh_topology(st: &mut State, sink: &Sink, cfg: &HostConfig) {
    match Topology::enumerate() {
        Ok(t) => {
            if luids(&t) != luids(&st.topo) {
                tracing::info!("GPU set changed; re-probing encoders");
                st.probes = select::probe_all(&t);
            }
            st.topo = t;
            for x in st.slots.values_mut() {
                x.failed.clear();
            }
            sink.send(Ev::DisplayChanged(pb::DisplayChanged { displays: display_infos(st) }));
            sink.send(Ev::SessionInfo(session_info(st, cfg)));
        }
        Err(e) => tracing::warn!("GPU enumeration failed: {e:#}"),
    }
}

/// Create, change or remove the session's display setup (virtual screens,
/// physical displays, local input). Returns true if the displays changed.
fn apply_virtual_display(st: &mut State, req: &pb::StartStream, sink: &Sink) -> bool {
    let want = Setup::from_pb(req.display_setup.as_ref());
    st.vd_release_at = None;
    let current = st.vd.as_ref().map(|v| v.setup().clone()).unwrap_or_default();
    if want == current {
        return false;
    }
    // The pipelines capture displays that are about to change.
    for x in st.slots.values_mut() {
        x.drop_pipe();
    }
    if want.is_default() {
        st.vd = None;
        return true;
    }
    let res = match st.vd.as_mut() {
        Some(vd) => vd.update(want),
        None => HostDisplays::open(want).map(|vd| st.vd = Some(vd)),
    };
    if let Err(e) = res {
        tracing::error!("virtual display: {e:#}");
        st.vd = None;
        sink.send(Ev::StreamError(pb::StreamError { message: format!("虚拟显示器不可用，改用物理显示器：{e:#}"), slot: 0 }));
    }
    true
}

fn build(st: &mut State, slot: u32, cfg: &HostConfig, desktop: &mut DesktopTracker) -> Result<Pipeline, String> {
    let req = st.slots[&slot].req.clone();
    if slot != 0 {
        // Extra windows show exactly the display they asked for.
        if st.topo.output(req.display_id).is_none() {
            return Err(format!("被控端没有这个显示器（{}），可能已被移除", req.display_id));
        }
    }
    let virtual_output = st
        .vd
        .as_ref()
        .and_then(|vd| vd.gdi_names().first())
        .and_then(|n| st.topo.outputs.iter().find(|o| o.device_name.eq_ignore_ascii_case(n)));
    let output = st
        .topo
        .output(req.display_id)
        .or(virtual_output)
        .or_else(|| st.topo.outputs.first())
        .ok_or_else(|| "没有可用的显示器".to_string())?
        .clone();
    check_capture(&st.topo, &output, desktop)?;
    let plans = select::plans(&st.probes, output.adapter_index, &req, st.caps.as_ref(), &cfg.encoder, output.hdr);
    let mut errors = Vec::new();
    for plan in plans {
        if st.slots[&slot].failed.get(&plan).copied().unwrap_or(0) >= 2 {
            tracing::info!("skipping plan that failed while encoding: {plan:?}");
            continue;
        }
        st.next_stream_id += 1;
        match Pipeline::build(
            &st.topo,
            &output,
            &plan,
            &req,
            st.caps.as_ref(),
            cfg,
            st.next_stream_id,
            st.slots[&slot].frame_counter,
            desktop,
            slot,
        ) {
            Ok(p) => {
                if !errors.is_empty() {
                    tracing::info!("fell back to {:?} {:?} after {} failures", p.backend(), plan.codec, errors.len());
                }
                return Ok(p);
            }
            Err(e) => {
                tracing::warn!("plan {plan:?} failed: {e:#}");
                errors.push(format!("{:?}/{:?}: {e:#}", plan.backend, plan.codec));
            }
        }
    }
    Err(if errors.is_empty() { "客户端不支持任何可用的编码格式".into() } else { errors.join("; ") })
}

/// Capture problems are independent of the encoder plan: report them clearly
/// instead of letting every plan fail with the same error.
fn check_capture(topo: &Topology, output: &nya_win::topology::OutputInfo, desktop: &mut DesktopTracker) -> Result<(), String> {
    use nya_win::d3d::D3dDevice;
    use nya_win::duplication::{DupError, Duplicator};
    let _ = desktop.sync();
    let adapter = topo.adapter(output.adapter_index).ok_or("显示器所在的显卡不存在")?;
    let dev = D3dDevice::for_adapter(&adapter.adapter).map_err(|e| format!("无法创建 D3D11 设备：{e:#}"))?;
    match Duplicator::new(&dev, &output.output) {
        Ok(_) => Ok(()),
        Err(DupError::Other(e)) if e.code().0 as u32 == 0x8007_0005 => Err(format!(
            "无法截取屏幕（拒绝访问）：被控端当前处于锁屏、登录界面、UAC 安全桌面，或会话已断开/最小化（桌面 {}）。\
             开发模式截不到这些界面，请解锁被控端，或改用服务模式（nya-server install）",
            desktop.name()
        )),
        Err(e) => Err(format!("无法截取屏幕：{e}")),
    }
}
