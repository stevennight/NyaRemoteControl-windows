//! Control pipe: protocol constants shared with the service, and the tool
//! side client (GUI, CLI). See `proto/control.proto`. The client is blocking:
//! keep it off the UI thread where a slow answer would matter.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use prost::Message;

use crate::config::ServerConfig;
use crate::control_pb::{self as cpb, error::Code, request::Req, response::Resp, Mode};

pub const SERVICE_PIPE: &str = "NyaRemoteControl.control";
pub const STANDALONE_PIPE: &str = "NyaRemoteControl.control.standalone";
/// Bumped when requests are added (see control.proto).
pub const CONTROL_VERSION: u32 = 2;
pub const MAX_MSG: usize = 1 << 20;
/// Log files the control pipe may return.
pub const LOG_NAMES: [&str; 4] = ["service", "helper", "standalone", "gui"];

pub fn pipe_path(mode: Mode) -> String {
    let name = if mode == Mode::Service { SERVICE_PIPE } else { STANDALONE_PIPE };
    format!(r"\\.\pipe\{name}")
}

pub fn config_to_pb(c: &ServerConfig) -> cpb::Config {
    cpb::Config {
        port: c.port.into(),
        bind: c.bind.clone(),
        name: c.name.clone(),
        encoder: c.encoder.clone(),
        office_bitrate_kbps: c.office_bitrate_kbps,
        game_bitrate_kbps: c.game_bitrate_kbps,
        max_fps: c.max_fps,
        audio: c.audio,
        log_level: c.log_level.clone(),
        no_update_check: !c.check_updates,
        public_address: c.public_address.clone(),
    }
}

pub fn config_from_pb(c: cpb::Config) -> ServerConfig {
    ServerConfig {
        // Out-of-range ports become 0 and fail validation.
        port: u16::try_from(c.port).unwrap_or(0),
        bind: c.bind,
        name: c.name,
        encoder: c.encoder,
        office_bitrate_kbps: c.office_bitrate_kbps,
        game_bitrate_kbps: c.game_bitrate_kbps,
        max_fps: c.max_fps,
        audio: c.audio,
        log_level: c.log_level,
        check_updates: !c.no_update_check,
        public_address: c.public_address,
    }
}


/// Errors a caller may want to tell apart (use `anyhow::Error::downcast_ref`).
#[derive(Debug)]
pub enum ControlError {
    /// Nothing is serving the pipe (service stopped / not installed).
    NotRunning,
    /// The host answered with an error.
    Remote { code: Code, message: String },
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::NotRunning => f.write_str("被控端没有运行"),
            ControlError::Remote { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for ControlError {}

pub fn is_not_running(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ControlError>(), Some(ControlError::NotRunning))
}

pub struct ControlClient {
    pipe: File,
    next_id: u64,
    pub hello: cpb::HelloReply,
}

impl ControlClient {
    /// Connect to the service (`Mode::Service`) or a standalone host.
    pub fn connect(target: Mode, tool: &str) -> Result<Self> {
        Self::connect_path(&pipe_path(target), target == Mode::Service, tool)
    }

    /// Connect to a specific pipe; `service`: check it is served by the installed service.
    pub fn connect_path(path: &str, service: bool, tool: &str) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        const ERROR_PIPE_BUSY: i32 = 231;
        const SECURITY_IDENTIFICATION: u32 = 1 << 16;
        // Least privilege: read + write data, nothing else.
        const GENERIC_READ: u32 = 0x8000_0000;
        const FILE_WRITE_DATA: u32 = 0x2;
        let deadline = Instant::now() + Duration::from_secs(2);
        let pipe = loop {
            let mut opts = std::fs::OpenOptions::new();
            opts.access_mode(GENERIC_READ | FILE_WRITE_DATA)
                // Identification only: the host may check who we are, not act as us.
                .security_qos_flags(SECURITY_IDENTIFICATION);
            match opts.open(path) {
                Ok(f) => break f,
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => return Err(ControlError::NotRunning.into()),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => return Err(e).with_context(|| format!("打开控制管道 {path}")),
            }
        };
        if service {
            verify_service_owns(&pipe)?;
        }
        let mut c = Self { pipe, next_id: 1, hello: Default::default() };
        let tool = format!("{tool} {}", env!("CARGO_PKG_VERSION"));
        match c.call(Req::Hello(cpb::Hello { control_version: CONTROL_VERSION, tool }))? {
            Resp::Hello(h) => c.hello = h,
            other => bail!("unexpected reply to Hello: {other:?}"),
        }
        Ok(c)
    }

    pub fn is_admin(&self) -> bool {
        self.hello.admin
    }

    fn call(&mut self, req: Req) -> Result<Resp> {
        let id = self.next_id;
        self.next_id += 1;
        let buf = cpb::Request { id, req: Some(req) }.encode_length_delimited_to_vec();
        self.pipe.write_all(&buf).context("写入控制管道")?;
        let len = read_varint(&mut self.pipe).context("读取控制管道")?;
        if len > MAX_MSG as u64 {
            bail!("control reply too large ({len} bytes)");
        }
        let mut body = vec![0u8; len as usize];
        self.pipe.read_exact(&mut body).context("读取控制管道")?;
        let resp = cpb::Response::decode(body.as_slice())?;
        if resp.id != id {
            bail!("control reply out of order ({} != {id})", resp.id);
        }
        match resp.resp {
            Some(Resp::Error(e)) => Err(ControlError::Remote {
                code: Code::try_from(e.code).unwrap_or(Code::Internal),
                message: e.message,
            }
            .into()),
            Some(r) => Ok(r),
            None => bail!("empty control reply"),
        }
    }

    pub fn status(&mut self) -> Result<cpb::Status> {
        match self.call(Req::GetStatus(Default::default()))? {
            Resp::Status(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    pub fn pairing(&mut self) -> Result<cpb::Pairing> {
        match self.call(Req::GetPairing(Default::default()))? {
            Resp::Pairing(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    pub fn reset_pairing_code(&mut self) -> Result<cpb::Pairing> {
        match self.call(Req::ResetPairingCode(Default::default()))? {
            Resp::Pairing(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    pub fn config(&mut self) -> Result<ServerConfig> {
        match self.call(Req::GetConfig(Default::default()))? {
            Resp::Config(c) => Ok(config_from_pb(c)),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_config(&mut self, cfg: &ServerConfig) -> Result<cpb::SetConfigReply> {
        match self.call(Req::SetConfig(cpb::SetConfig { config: Some(config_to_pb(cfg)) }))? {
            Resp::SetConfig(r) => Ok(r),
            other => Err(unexpected(other)),
        }
    }

    pub fn clients(&mut self) -> Result<Vec<cpb::PairedClient>> {
        match self.call(Req::ListClients(Default::default()))? {
            Resp::Clients(c) => Ok(c.clients),
            other => Err(unexpected(other)),
        }
    }

    /// `fingerprint`: the full fingerprint or a unique prefix (8+ hex digits).
    pub fn remove_client(&mut self, fingerprint: &str) -> Result<String> {
        match self.call(Req::RemoveClient(cpb::RemoveClient { fingerprint: fingerprint.into() }))? {
            Resp::Done(d) => Ok(d.message),
            other => Err(unexpected(other)),
        }
    }

    pub fn disconnect(&mut self, reason: &str) -> Result<String> {
        match self.call(Req::Disconnect(cpb::Disconnect { reason: reason.into() }))? {
            Resp::Done(d) => Ok(d.message),
            other => Err(unexpected(other)),
        }
    }

    pub fn tail_log(&mut self, name: &str, lines: u32) -> Result<String> {
        match self.call(Req::TailLog(cpb::TailLog { name: name.into(), lines }))? {
            Resp::Log(l) => Ok(l.text),
            other => Err(unexpected(other)),
        }
    }

    /// Does the host know the update requests (control version 2)?
    pub fn has_updates(&self) -> bool {
        self.hello.control_version >= 2
    }

    /// Check for a newer release now (waits for the answer from GitHub).
    pub fn check_update(&mut self) -> Result<cpb::UpdateStatus> {
        match self.call(Req::CheckUpdate(cpb::CheckUpdate {}))? {
            Resp::Status(s) => Ok(s.update.unwrap_or_default()),
            other => Err(unexpected(other)),
        }
    }

    /// Download and install the newer release; the service restarts.
    pub fn apply_update(&mut self) -> Result<String> {
        match self.call(Req::ApplyUpdate(cpb::ApplyUpdate {}))? {
            Resp::Done(d) => Ok(d.message),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(r: Resp) -> anyhow::Error {
    anyhow!("unexpected control reply: {r:?}")
}

fn read_varint(r: &mut impl Read) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        v |= u64::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("invalid varint")
}

/// Refuse a pipe that is not served by the installed service (another
/// process could have claimed the name while the service was stopped).
fn verify_service_owns(pipe: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let mut pid = 0u32;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }.context("GetNamedPipeServerProcessId")?;
    let service_pid = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(crate::SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .and_then(|s| s.query_status())
        .ok()
        .and_then(|s| s.process_id);
    if service_pid != Some(pid) {
        bail!("控制管道不是由 {} 服务创建的（进程 {pid}），已拒绝连接", crate::SERVICE_NAME);
    }
    Ok(())
}

