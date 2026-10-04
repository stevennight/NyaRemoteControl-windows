use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const ENCODERS: [&str; 5] = ["auto", "nvenc", "qsv", "amf", "software"];

/// `server.toml`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// UDP port.
    pub port: u16,
    /// Address to bind; `::` listens on all IPv4 and IPv6 addresses. Set it to the
    /// overlay network address (e.g. Tailscale 100.x.y.z) to only accept connections there.
    pub bind: String,
    /// Name shown to clients; empty = computer name.
    pub name: String,
    /// "auto" | "nvenc" | "qsv" | "amf" | "software"
    pub encoder: String,
    /// Default bitrate in kbit/s when the client doesn't ask for one; 0 = automatic.
    pub office_bitrate_kbps: u32,
    pub game_bitrate_kbps: u32,
    pub max_fps: u32,
    pub audio: bool,
    /// tracing filter, e.g. "info" or "nya_server=debug"
    pub log_level: String,
    /// Look for new versions on GitHub Releases (installing is always manual).
    pub check_updates: bool,
    /// Addresses clients reach this computer at from elsewhere (port
    /// forwarding, frp: `host:port`, comma separated), put in pairing links
    /// before the local IPs. Only the link uses it.
    pub public_address: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: nya_proto::DEFAULT_PORT,
            bind: "::".into(),
            name: String::new(),
            encoder: "auto".into(),
            office_bitrate_kbps: 0,
            game_bitrate_kbps: 0,
            max_fps: 144,
            audio: true,
            log_level: "info".into(),
            check_updates: true,
            public_address: String::new(),
        }
    }
}

impl ServerConfig {
    /// Load `server.toml`, writing the defaults if it doesn't exist.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join("server.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            return toml::from_str(&text).with_context(|| format!("parse {}", path.display()));
        }
        let cfg = Self::default();
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        Ok(cfg)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("server.toml"), toml::to_string_pretty(self)?).context("write server.toml")
    }

    /// Reject values the host cannot use (messages are shown to the user).
    pub fn validate(&self) -> Result<()> {
        if self.port < 1024 {
            bail!("端口必须在 1024–65535 之间");
        }
        if self.bind.parse::<std::net::IpAddr>().is_err() {
            bail!("监听地址 {:?} 不是有效的 IP 地址（:: 表示所有网卡）", self.bind);
        }
        if !ENCODERS.contains(&self.encoder.as_str()) {
            bail!("未知编码器 {:?}（可选：{}）", self.encoder, ENCODERS.join(" / "));
        }
        if !(1..=1000).contains(&self.max_fps) {
            bail!("最高帧率必须在 1–1000 之间");
        }
        if tracing_subscriber::EnvFilter::try_new(&self.log_level).is_err() {
            bail!("日志级别 {:?} 无效", self.log_level);
        }
        Ok(())
    }

    /// Do the settings the host (helper) reads at startup differ?
    pub fn host_part_differs(&self, o: &Self) -> bool {
        (&self.name, &self.encoder, self.office_bitrate_kbps, self.game_bitrate_kbps, self.max_fps, self.audio)
            != (&o.name, &o.encoder, o.office_bitrate_kbps, o.game_bitrate_kbps, o.max_fps, o.audio)
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            crate::win::computer_name()
        } else {
            self.name.clone()
        }
    }
}
