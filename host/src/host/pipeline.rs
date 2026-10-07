//! One video stream for one display (design doc §3.1, §3.5):
//!
//! ```text
//! DXGI duplication ─► desktop copy (BGRA, capture GPU)
//!    ─► colour convert ─┬─ same GPU ───────────────► encoder pool texture ─► encode
//!                       ├─ other GPU ─ GpuToGpu (T2 / T1) ─► encoder pool texture ─► encode
//!                       └─ software ── Readback ────► CPU NV12 ────────────► OpenH264
//! ```

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use nya_media::encoder::{Backend, EncoderConfig, InputFormat, VideoEncoder};
use nya_proto::frame::{frame_flags, VideoFrameHeader};
use nya_proto::{now_us, pb};
use nya_win::convert::{Converter, TargetFormat};
use nya_win::d3d::{tex_desc, D3dDevice};
use nya_win::desktop::DesktopTracker;
use nya_win::display_config;
use nya_win::duplication::{DupError, Duplicator, PointerUpdate};
use nya_win::input::DisplayRect;
use nya_win::topology::{OutputInfo, Topology};
use nya_win::transfer::Readback;
use nya_win::transfer12::GpuToGpu;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT;
use windows::Win32::Graphics::Dxgi::IDXGIOutput;

use super::cursor::{self, CursorTracker};
use super::select::{self, Plan};
use super::{HostConfig, Sink};
use crate::ipc_pb::{host_event::Ev, VideoFrame};

/// Frames handed to the network but not yet written out.
const MAX_INFLIGHT: u32 = 2;
/// Static-scene refinement: extra frames after the picture stops changing.
const REFINE_FRAMES: u32 = 4;
const REFINE_DELAY: Duration = Duration::from_millis(60);
const MIN_KEYFRAME_GAP: Duration = Duration::from_millis(300);

pub enum Step {
    Ok,
    Rebuild(String),
}

struct DesktopCopy {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    desc: D3D11_TEXTURE2D_DESC,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    bytes: u64,
    encode_ms: Vec<f32>,
    transfer_ms: Vec<f32>,
}

pub struct Pipeline {
    /// Client window this stream is for (0 = main window).
    pub slot: u32,
    pub plan: Plan,
    pub stream_id: u64,
    pub started: pb::StreamStarted,
    pub rect: DisplayRect,
    pub frame_id: u64,
    first_frame_id: u64,
    output: IDXGIOutput,
    output_name: String,
    /// Re-read the desktop format / HDR white level with the next image.
    recheck_format: bool,
    native: (u32, u32),
    capture: D3dDevice,
    dup: Option<Duplicator>,
    dup_failures: u32,
    next_dup_retry: Instant,
    desktop: Option<DesktopCopy>,
    converter: Converter,
    target: TargetFormat,
    intermediate: Option<ID3D11Texture2D>,
    xcopy: Option<GpuToGpu>,
    readback: Option<Readback>,
    cpu_buf: Vec<u8>,
    /// Software path reads back BGRA and converts to NV12 on the CPU.
    cpu_bgra: bool,
    nv12_buf: Vec<u8>,
    encoder: VideoEncoder,
    width: u32,
    height: u32,
    fps: u32,
    game: bool,
    codec: pb::Codec,
    chroma: pb::Chroma,
    inflight: u32,
    last_frame_sent: Instant,
    have_image: bool,
    dirty: bool,
    last_change: Instant,
    refine_left: u32,
    last_encode: Instant,
    keyframe_pending: bool,
    last_keyframe: Instant,
    cursor: CursorTracker,
    stats: Stats,
    /// Encode times of the last 10 s, for the 99th percentile.
    encode_window: nya_proto::stats::Rolling,
    built_at: Instant,
    warned_no_image: bool,
    diag: CaptureCounters,
    /// Consecutive access-lost errors without an image; switches API after 5.
    lost_streak: u32,
    /// Input desktop of the last copied image: a duplication re-created on
    /// the same desktop keeps that picture rather than an unpresented image.
    image_desktop: String,
    legacy_dup: bool,
}

/// Capture counters, logged every 5 s while the stream isn't producing frames.
#[derive(Default)]
struct CaptureCounters {
    since: Option<Instant>,
    acquired: u32,
    images: u32,
    timeouts: u32,
    access_lost: u32,
    dup_failures: u32,
    encoded: u32,
}

fn even(v: u32) -> u32 {
    v & !1
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        topo: &Topology,
        output: &OutputInfo,
        plan: &Plan,
        req: &pb::StartStream,
        caps: Option<&pb::ClientCaps>,
        cfg: &HostConfig,
        stream_id: u64,
        first_frame_id: u64,
        desktop: &mut DesktopTracker,
        slot: u32,
    ) -> Result<Self> {
        let cap_info = topo.adapter(plan.capture_adapter).ok_or_else(|| anyhow!("capture GPU missing"))?;
        let capture = D3dDevice::for_adapter(&cap_info.adapter).context("capture device")?;
        let (encode_dev, cross) = match plan.encode_adapter {
            Some(i) if i == plan.capture_adapter => (Some(capture.clone()), false),
            Some(i) => {
                let a = topo.adapter(i).ok_or_else(|| anyhow!("encode GPU missing"))?;
                (Some(D3dDevice::for_adapter(&a.adapter).context("encode device")?), true)
            }
            None => (None, false),
        };

        if let Err(e) = desktop.sync() {
            tracing::debug!("desktop sync: {e:#}");
        }
        let dup = Duplicator::new(&capture, &output.output).map_err(|e| anyhow!("DuplicateOutput: {e}"))?;
        if dup.rotation > 1 {
            tracing::warn!("display {} is rotated; rotation is not handled yet", output.device_name);
        }
        let native = (dup.width, dup.height);

        // Stream parameters.
        let sc = req.config.clone().unwrap_or_default();
        let game = sc.mode == pb::StreamMode::Game as i32;
        let (mut w, mut h) = native;
        if sc.width > 0 && sc.height > 0 && (sc.width < w || sc.height < h) {
            // Keep the aspect ratio inside the requested box.
            let s = (sc.width as f64 / w as f64).min(sc.height as f64 / h as f64);
            w = (w as f64 * s) as u32;
            h = (h as f64 * s) as u32;
        }
        if let Some(c) = caps {
            if c.max_width > 0 && c.max_height > 0 && (w > c.max_width || h > c.max_height) {
                let s = (c.max_width as f64 / w as f64).min(c.max_height as f64 / h as f64);
                w = (w as f64 * s) as u32;
                h = (h as f64 * s) as u32;
            }
        }
        let (w, h) = (even(w).max(64), even(h).max(64));
        let client_max_fps = caps.map(|c| c.max_fps).filter(|&f| f > 0).unwrap_or(240);
        let default_fps = if game { output.refresh_hz } else { output.refresh_hz.min(60) };
        let fps = if sc.fps > 0 { sc.fps } else { default_fps }.min(cfg.max_fps).min(client_max_fps).clamp(10, 240);
        let configured = if game { cfg.game_bitrate_kbps } else { cfg.office_bitrate_kbps };
        let bitrate = if sc.bitrate_kbps > 0 {
            sc.bitrate_kbps
        } else if configured > 0 {
            configured
        } else {
            // 10-bit HDR needs a little more for the same look.
            select::auto_bitrate(w, h, fps, game, plan.yuv444) * if plan.hdr { 5 } else { 4 } / 4
        };

        let enc_cfg = EncoderConfig {
            backend: plan.backend,
            codec: plan.codec,
            yuv444: plan.yuv444,
            width: w,
            height: h,
            fps,
            bitrate_kbps: bitrate,
            game_mode: game,
            hdr: plan.hdr,
        };
        let raw = encode_dev.as_ref().map(|d| d.device_raw_owned()).unwrap_or(std::ptr::null_mut());
        let mut encoder = VideoEncoder::open(&enc_cfg, raw).with_context(|| format!("open {:?}", plan))?;
        let mut target = match encoder.input_format() {
            InputFormat::Nv12 | InputFormat::CpuNv12 => TargetFormat::Nv12,
            InputFormat::Bgra => TargetFormat::Bgra,
            InputFormat::Ayuv => TargetFormat::Ayuv,
            InputFormat::P010 => TargetFormat::P010,
        };
        let cpu = encoder.input_format() == InputFormat::CpuNv12;
        let mut converter = Converter::new(&capture)?;
        // Intermediate texture on the capture GPU (cross-GPU and software paths).
        let make_intermediate = |converter: &mut Converter, t: TargetFormat| -> Result<ID3D11Texture2D> {
            let tex = capture.texture(&tex_desc(w, h, t.dxgi(), D3D11_BIND_RENDER_TARGET))?;
            converter.prepare_target(&tex, 0, t)?;
            Ok(tex)
        };
        let mut cpu_bgra = false;
        let intermediate = if cpu {
            match make_intermediate(&mut converter, TargetFormat::Nv12) {
                Ok(t) => Some(t),
                Err(e) => {
                    // No NV12 render targets on this GPU: read back BGRA and convert on the CPU.
                    tracing::info!("NV12 render target unavailable ({e:#}); converting on CPU");
                    target = TargetFormat::Bgra;
                    cpu_bgra = true;
                    Some(make_intermediate(&mut converter, TargetFormat::Bgra)?)
                }
            }
        } else if cross {
            Some(make_intermediate(&mut converter, target).context("intermediate texture")?)
        } else {
            None
        };
        let xcopy = match (&encode_dev, cross) {
            (Some(dst), true) => Some(GpuToGpu::new(&capture, dst, target.dxgi(), w, h)?),
            _ => None,
        };
        let readback = if cpu { Some(Readback::new(&capture, target.dxgi(), w, h)?) } else { None };
        if !cross && !cpu {
            // Make sure we can render into the encoder's pool textures.
            let surf = encoder.surface().context("encoder surface")?;
            let tex = unsafe { ID3D11Texture2D::from_raw_borrowed(&surf.texture) }
                .ok_or_else(|| anyhow!("null encoder surface"))?
                .clone();
            converter.prepare_target(&tex, surf.index, target).context("render target on encoder surface")?;
        }

        let codec = select::to_pb_codec(plan.codec);
        let chroma = if plan.yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 };
        let started = pb::StreamStarted {
            display_id: output.id,
            config: Some(pb::StreamConfig {
                codec: codec as i32,
                chroma: chroma as i32,
                width: w,
                height: h,
                fps,
                bitrate_kbps: bitrate,
                mode: if game { pb::StreamMode::Game } else { pb::StreamMode::Office } as i32,
                bitrate_policy: sc.bitrate_policy,
                video_transport: sc.video_transport,
                hdr: plan.hdr,
            }),
            stream_id,
            encoder_name: encoder.name().to_owned(),
            capture_gpu_index: plan.capture_adapter,
            encode_gpu_index: plan.encode_adapter.unwrap_or(plan.capture_adapter),
            cross_gpu: cross,
            source_width: native.0,
            source_height: native.1,
            hdr_tonemapped: output.hdr && !plan.hdr,
            slot,
        };
        tracing::info!(
            "stream {stream_id}: display {} {}x{} -> {w}x{h}@{fps} {} {:?} {} kbps{}",
            output.device_name,
            native.0,
            native.1,
            encoder.name(),
            chroma,
            bitrate,
            if cross { " (cross-GPU)" } else { "" }
        );
        let now = Instant::now();
        Ok(Self {
            slot,
            plan: plan.clone(),
            stream_id,
            started,
            rect: DisplayRect { left: output.left, top: output.top, width: output.width(), height: output.height() },
            frame_id: first_frame_id,
            first_frame_id,
            output: output.output.clone(),
            output_name: output.device_name.clone(),
            recheck_format: true,
            native,
            capture,
            dup: Some(dup),
            dup_failures: 0,
            next_dup_retry: now,
            desktop: None,
            converter,
            target,
            intermediate,
            xcopy,
            readback,
            cpu_buf: Vec::new(),
            cpu_bgra,
            nv12_buf: Vec::new(),
            encoder,
            width: w,
            height: h,
            fps,
            game,
            codec,
            chroma,
            inflight: 0,
            last_frame_sent: now,
            have_image: false,
            dirty: false,
            last_change: now,
            refine_left: 0,
            last_encode: now - Duration::from_secs(1),
            keyframe_pending: true,
            last_keyframe: now - Duration::from_secs(1),
            cursor: CursorTracker::default(),
            stats: Stats::default(),
            encode_window: Default::default(),
            built_at: now,
            warned_no_image: false,
            diag: CaptureCounters::default(),
            lost_streak: 0,
            image_desktop: String::new(),
            legacy_dup: false,
        })
    }

    pub fn backend(&self) -> Backend {
        self.encoder.config().backend
    }

    /// Adaptive bitrate from the network side, capped at the stream's configured rate.
    pub fn set_bitrate(&mut self, kbps: u32) {
        let max = self.started.config.as_ref().map(|c| c.bitrate_kbps).unwrap_or(kbps);
        let k = kbps.clamp(500, max.max(500));
        if self.encoder.set_bitrate(k) {
            tracing::info!("bitrate -> {k} kbps");
        }
    }

    pub fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    pub fn frame_sent(&mut self, frame_id: u64) {
        if frame_id > self.first_frame_id {
            self.inflight = self.inflight.saturating_sub(1);
            self.last_frame_sent = Instant::now();
        }
    }

    fn copy_desktop(&mut self, img: &ID3D11Texture2D) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { img.GetDesc(&mut desc) };
        let matches = self.desktop.as_ref().is_some_and(|d| {
            d.desc.Width == desc.Width && d.desc.Height == desc.Height && d.desc.Format == desc.Format
        });
        if !matches || self.recheck_format {
            self.recheck_format = false;
            // FP16 = scRGB image of an HDR desktop: tone-map it to SDR.
            let hdr = (desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT).then(|| {
                let nits = display_config::sdr_white_nits(&self.output_name).unwrap_or(80.0);
                let how = if self.plan.hdr { "streaming HDR10" } else { "tone-mapping to SDR" };
                tracing::info!("{}: HDR desktop, SDR white {nits:.0} nits; {how}", self.output_name);
                nits
            });
            self.converter.set_hdr(hdr);
        }
        if !matches {
            let tex = self.capture.texture(&tex_desc(desc.Width, desc.Height, desc.Format, D3D11_BIND_SHADER_RESOURCE))?;
            let srv = self.capture.srv(&tex)?;
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut d) };
            self.desktop = Some(DesktopCopy { tex, srv, desc: d });
        }
        let dc = self.desktop.as_ref().unwrap();
        unsafe { self.capture.context.CopyResource(&dc.tex, img) };
        Ok(())
    }

    /// One round: wait up to `max_wait` for a desktop update, encode when due.
    /// With several pipelines in one thread, `max_wait` keeps them all moving.
    pub fn step(&mut self, sink: &Sink, desktop: &mut DesktopTracker, max_wait: Duration) -> Step {
        let now = Instant::now();
        // Before any early return, so a capture loop that never gets an image is visible.
        self.log_capture_status(desktop);
        if !self.have_image && !self.warned_no_image && now - self.built_at > Duration::from_secs(3) {
            self.warned_no_image = true;
            tracing::warn!("no desktop image after 3 s; desktop {}", desktop.name());
        }
        let interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        let next_due = self.last_encode + interval;

        if self.dup.is_none() {
            if now < self.next_dup_retry {
                std::thread::sleep(Duration::from_millis(5));
                return Step::Ok;
            }
            let _ = desktop.sync();
            let created = if self.legacy_dup {
                Duplicator::new_legacy(&self.capture, &self.output)
            } else {
                Duplicator::new(&self.capture, &self.output)
            };
            match created {
                Ok(mut d) => {
                    if (d.width, d.height) != self.native {
                        return Step::Rebuild(format!("resolution changed to {}x{}", d.width, d.height));
                    }
                    // Access lost on the same desktop (some HDR displays keep losing
                    // it): the picture we have is current; no black first image, no
                    // keyframe each time. A new desktop (lock screen, UAC) starts afresh.
                    if self.have_image && desktop.name() == self.image_desktop {
                        d.skip_unpresented_first();
                    } else {
                        self.keyframe_pending = true;
                    }
                    self.dup = Some(d);
                    self.dup_failures = 0;
                    // HDR may have been switched on or off, or its SDR brightness changed.
                    self.recheck_format = true;
                }
                Err(DupError::DeviceLost) => return Step::Rebuild("GPU device lost".into()),
                Err(e) => {
                    self.diag.dup_failures += 1;
                    self.dup_failures += 1;
                    if self.dup_failures == 1 || self.dup_failures % 50 == 0 {
                        tracing::warn!("DuplicateOutput failed ({}x): {e}", self.dup_failures);
                    }
                    if self.dup_failures > 150 {
                        return Step::Rebuild(format!("cannot duplicate output: {e}"));
                    }
                    self.next_dup_retry = now + Duration::from_millis(200);
                    return Step::Ok;
                }
            }
        }

        let wait_ms = next_due.saturating_duration_since(now).min(max_wait).as_millis().max(1) as u32;
        let dup = self.dup.as_mut().unwrap();
        match dup.acquire(wait_ms) {
            Ok(Some(frame)) => {
                self.diag.acquired += 1;
                if frame.image.is_some() {
                    self.diag.images += 1;
                    self.lost_streak = 0;
                }
                if let Some(img) = &frame.image {
                    if let Err(e) = self.copy_desktop(img) {
                        tracing::warn!("copy desktop: {e:#}");
                    } else {
                        self.have_image = true;
                        if self.image_desktop != desktop.name() {
                            self.image_desktop = desktop.name().to_string();
                        }
                        self.dirty = true;
                        self.last_change = Instant::now();
                        self.refine_left = REFINE_FRAMES;
                    }
                }
                if let Some(d) = self.dup.as_mut() {
                    d.release();
                }
                self.send_cursor(sink, frame.pointer);
            }
            Ok(None) => {
                self.diag.timeouts += 1;
                // The pointer moves without desktop updates too.
                self.send_cursor(sink, PointerUpdate::default());
            }
            Err(DupError::AccessLost) => {
                // Desktop switch (lock screen, UAC), mode change or fullscreen transition.
                self.diag.access_lost += 1;
                if self.diag.access_lost <= 3 {
                    tracing::info!("duplication access lost (desktop {})", desktop.name());
                }
                self.lost_streak += 1;
                if self.lost_streak >= 5 {
                    self.lost_streak = 0;
                    self.legacy_dup = !self.legacy_dup;
                    tracing::warn!(
                        "duplication keeps losing access; switching to {} API",
                        if self.legacy_dup { "legacy DuplicateOutput" } else { "DuplicateOutput1" }
                    );
                }
                // Don't spin: give the desktop switch a moment.
                self.next_dup_retry = Instant::now() + Duration::from_millis(50);
                self.dup = None;
                return Step::Ok;
            }
            Err(DupError::DeviceLost) => return Step::Rebuild("GPU device lost".into()),
            Err(DupError::Other(e)) => {
                tracing::warn!("AcquireNextFrame: {e}");
                self.dup = None;
                self.next_dup_retry = Instant::now() + Duration::from_millis(100);
                return Step::Ok;
            }
        }

        let now = Instant::now();
        if self.inflight >= MAX_INFLIGHT && now - self.last_frame_sent > Duration::from_secs(3) {
            tracing::warn!("no FrameSent for 3 s; resetting flow control");
            self.inflight = 0;
        }
        let due = now >= next_due;
        let refine = !self.dirty && self.refine_left > 0 && now - self.last_change >= REFINE_DELAY;
        let want = self.have_image
            && (self.keyframe_pending || (due && (self.dirty || self.game || refine)));
        if want && self.inflight < MAX_INFLIGHT {
            if let Err(e) = self.encode(sink, refine) {
                return Step::Rebuild(format!("encode failed: {e:#}"));
            }
        }
        self.report_stats(sink);
        Step::Ok
    }

    /// Shape from DXGI; position and visibility from Windows, as DXGI's are
    /// unreliable (see [`cursor::os_pointer`]). DXGI's when Windows won't say.
    fn send_cursor(&mut self, sink: &Sink, pointer: PointerUpdate) {
        let mut msgs = Vec::new();
        match cursor::os_pointer(&self.rect) {
            Some(pos) => self.cursor.update_os(pointer.shape, pos, &mut msgs),
            None => self.cursor.update(pointer, &mut msgs),
        }
        for mut m in msgs {
            if let Some(pb::cursor_msg::Msg::State(st)) = m.msg.as_mut() {
                st.slot = self.slot;
            }
            sink.send(Ev::Cursor(m));
        }
    }

    fn encode(&mut self, sink: &Sink, refine: bool) -> Result<()> {
        let t0 = Instant::now();
        let key = self.keyframe_pending && self.last_keyframe.elapsed() >= MIN_KEYFRAME_GAP;
        let (w, h) = (self.width, self.height);
        let srv = self.desktop.as_ref().ok_or_else(|| anyhow!("no desktop image"))?.srv.clone();
        let mut packets = Vec::new();
        let mut transfer_ms = 0.0;
        match self.encoder.input_format() {
            InputFormat::CpuNv12 => {
                let inter = self.intermediate.as_ref().unwrap();
                self.converter.convert(&srv, inter, 0, self.target, w, h)?;
                let t = Instant::now();
                self.readback.as_mut().unwrap().read(inter, &mut self.cpu_buf)?;
                if self.cpu_bgra {
                    bgra_to_nv12(&self.cpu_buf, w as usize, h as usize, &mut self.nv12_buf);
                    std::mem::swap(&mut self.cpu_buf, &mut self.nv12_buf);
                }
                transfer_ms = t.elapsed().as_secs_f32() * 1000.0;
                self.encoder.encode_nv12_cpu(&self.cpu_buf, key, &mut packets)?;
            }
            _ => {
                let surf = self.encoder.surface()?;
                let tex = unsafe { ID3D11Texture2D::from_raw_borrowed(&surf.texture) }
                    .ok_or_else(|| anyhow!("null encoder surface"))?
                    .clone();
                if let Some(x) = self.xcopy.as_mut() {
                    let inter = self.intermediate.as_ref().unwrap();
                    self.converter.convert(&srv, inter, 0, self.target, w, h)?;
                    let t = Instant::now();
                    x.copy(inter, &tex, surf.index)?;
                    transfer_ms = t.elapsed().as_secs_f32() * 1000.0;
                } else {
                    self.converter.convert(&srv, &tex, surf.index, self.target, w, h)?;
                    // The encoder runs on another engine: finish our rendering first.
                    self.capture.flush_wait()?;
                }
                drop(tex);
                self.encoder.encode(surf, key, &mut packets)?;
            }
        }
        let capture_ts = now_us();
        let now = Instant::now();
        for p in packets {
            self.frame_id += 1;
            let mut flags = 0;
            if p.keyframe {
                flags |= frame_flags::KEYFRAME;
                self.keyframe_pending = false;
                self.last_keyframe = now;
            }
            if refine {
                flags |= frame_flags::STATIC_REFINE;
            }
            let header = VideoFrameHeader {
                flags,
                frame_id: self.frame_id,
                capture_ts_us: capture_ts,
                width: w as u16,
                height: h as u16,
                codec: self.codec as u8,
                chroma: self.chroma as u8,
            };
            let mut hb = Vec::with_capacity(VideoFrameHeader::LEN_V1);
            header.write(&mut hb);
            self.stats.bytes += p.data.len() as u64;
            sink.send(Ev::Video(VideoFrame { stream_id: self.stream_id, frame_id: self.frame_id, header: hb, data: p.data, slot: self.slot }));
            self.inflight += 1;
        }
        if key && self.keyframe_pending {
            tracing::debug!("keyframe requested but encoder produced none yet");
        }
        self.stats.frames += 1;
        self.diag.encoded += 1;
        self.stats.encode_ms.push(t0.elapsed().as_secs_f32() * 1000.0 - transfer_ms);
        if self.xcopy.is_some() || self.readback.is_some() {
            self.stats.transfer_ms.push(transfer_ms);
        }
        self.last_encode = now;
        if refine {
            self.refine_left -= 1;
        }
        self.dirty = false;
        Ok(())
    }

    /// While frames aren't flowing, say why every 5 s.
    fn log_capture_status(&mut self, desktop: &DesktopTracker) {
        let since = *self.diag.since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_secs(5) {
            return;
        }
        let d = std::mem::take(&mut self.diag);
        if d.encoded >= 5 && d.access_lost > 0 {
            tracing::info!(
                "{}: duplication access lost {}x in 5 s ({} images, {} encoded)",
                self.output_name,
                d.access_lost,
                d.images,
                d.encoded
            );
        }
        if d.encoded < 5 {
            tracing::warn!(
                "capture status (5 s): acquired={} images={} timeouts={} access_lost={} dup_failures={} encoded={} inflight={} have_image={} desktop={}",
                d.acquired,
                d.images,
                d.timeouts,
                d.access_lost,
                d.dup_failures,
                d.encoded,
                self.inflight,
                self.have_image,
                desktop.name()
            );
        }
        self.diag.since = Some(Instant::now());
    }

    fn report_stats(&mut self, sink: &Sink) {
        let since = *self.stats.since.get_or_insert_with(Instant::now);
        let elapsed = since.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let secs = elapsed.as_secs_f32();
        let s = std::mem::take(&mut self.stats);
        let (encode_ms_p50, encode_ms_p99) = self.encode_window.close(s.encode_ms);
        let mut xfer = s.transfer_ms;
        sink.send(Ev::Stats(pb::ServerStats {
            capture_ms_p50: 0.0,
            encode_ms_p50,
            encode_ms_p99,
            transfer_ms_p50: nya_proto::stats::percentile(&mut xfer, 0.5),
            fps: (s.frames as f32 / secs).round() as u32,
            bitrate_kbps: (s.bytes as f32 * 8.0 / 1000.0 / secs) as u32,
            target_kbps: self.encoder.config().bitrate_kbps,
            bitrate_note: String::new(),
            fec_percent: 0, // filled in by the network side
            slot: self.slot,
            path_loss_pct: 0.0, // filled in by the network side
            path_rtt_ms: 0.0,
        }));
        self.stats.since = Some(Instant::now());
    }
}

/// BGRA → NV12, BT.709 limited range (software-encoder fallback only).
fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    out.clear();
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let px = |x: usize, y: usize| {
        let o = (y * w + x) * 4;
        (bgra[o + 2] as f32, bgra[o + 1] as f32, bgra[o] as f32)
    };
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, y);
            y_plane[y * w + x] = (16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0).round() as u8;
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = px(2 * x + dx, 2 * y + dy);
                r += p.0 / 4.0;
                g += p.1 / 4.0;
                b += p.2 / 4.0;
            }
            let u = 128.0 + (-0.114572 * r - 0.385428 * g + 0.5 * b) * 224.0 / 255.0;
            let v = 128.0 + (0.5 * r - 0.454153 * g - 0.045847 * b) * 224.0 / 255.0;
            uv_plane[y * w + 2 * x] = u.round().clamp(0.0, 255.0) as u8;
            uv_plane[y * w + 2 * x + 1] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_nv12_levels() {
        let (w, h) = (4, 2);
        let white = vec![255u8; w * h * 4];
        let mut out = Vec::new();
        bgra_to_nv12(&white, w, h, &mut out);
        assert!(out[..w * h].iter().all(|&y| y == 235));
        assert!(out[w * h..].iter().all(|&c| c == 128));
        let black: Vec<u8> = (0..w * h).flat_map(|_| [0, 0, 0, 255]).collect();
        bgra_to_nv12(&black, w, h, &mut out);
        assert!(out[..w * h].iter().all(|&y| y == 16));
    }
}
