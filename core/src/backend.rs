//! What management tools operate on: the running host through the control
//! pipe, or — while nothing is running — the host's files directly (the host
//! reads them when it starts).

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use nya_transport::Identity;

use crate::auth::{self, load_or_create_key, AuthStore, PairedClient};
use crate::config::ServerConfig;
use crate::control::{is_not_running, ControlClient};
use crate::control_pb::{self as cpb, Mode};
use crate::{logging, paths};

pub enum Backend {
    Live(ControlClient),
    Offline(PathBuf),
}

impl Backend {
    /// The service if it is running, else its data directory.
    pub fn service(tool: &str) -> Result<Self> {
        match ControlClient::connect(Mode::Service, tool) {
            Ok(c) => Ok(Backend::Live(c)),
            Err(e) if is_not_running(&e) => Ok(Backend::Offline(paths::service_dir())),
            Err(e) => Err(e),
        }
    }

    /// For the CLI: an explicit data directory, else the running service,
    /// else a running standalone host, else the files of whichever exists.
    pub fn any(tool: &str, data_dir: Option<PathBuf>) -> Result<Self> {
        if let Some(d) = data_dir {
            return Ok(Backend::Offline(d));
        }
        for mode in [Mode::Service, Mode::Standalone] {
            match ControlClient::connect(mode, tool) {
                Ok(c) => return Ok(Backend::Live(c)),
                Err(e) if is_not_running(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let svc = paths::service_dir();
        if svc.exists() {
            if !crate::win::is_elevated() {
                bail!("服务的数据目录只有管理员能读取：请以管理员身份运行，或用 --data-dir 指定开发模式目录");
            }
            return Ok(Backend::Offline(svc));
        }
        Ok(Backend::Offline(paths::standalone_dir()))
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Backend::Live(_))
    }

    /// One line for the CLI / GUI: what the commands act on.
    pub fn describe(&self) -> String {
        match self {
            Backend::Live(c) if c.hello.mode == Mode::Standalone as i32 => "运行中的开发模式被控端".into(),
            Backend::Live(_) => "运行中的服务".into(),
            Backend::Offline(d) => format!("被控端未运行，直接读写 {}", d.display()),
        }
    }

    pub fn status(&mut self) -> Result<Option<cpb::Status>> {
        match self {
            Backend::Live(c) => c.status().map(Some),
            Backend::Offline(_) => Ok(None),
        }
    }

    pub fn pairing(&mut self) -> Result<cpb::Pairing> {
        match self {
            Backend::Live(c) => c.pairing(),
            Backend::Offline(d) => {
                let fp = Identity::load_or_create(d)?.fingerprint();
                Ok(cpb::Pairing { code: load_or_create_key(d, false)?.to_code(), fingerprint: fp.to_string(), fingerprint_hex: fp.to_hex() })
            }
        }
    }

    pub fn reset_pairing_code(&mut self) -> Result<cpb::Pairing> {
        match self {
            Backend::Live(c) => c.reset_pairing_code(),
            Backend::Offline(d) => {
                load_or_create_key(d, true)?;
                self.pairing()
            }
        }
    }

    pub fn config(&mut self) -> Result<ServerConfig> {
        match self {
            Backend::Live(c) => c.config(),
            Backend::Offline(d) => ServerConfig::load_or_create(d),
        }
    }

    /// Save (and, when live, apply) the settings. Returns a message for the user.
    pub fn set_config(&mut self, cfg: &ServerConfig) -> Result<String> {
        match self {
            Backend::Live(c) => c.set_config(cfg).map(|r| r.message),
            Backend::Offline(d) => {
                cfg.validate()?;
                let old = ServerConfig::load_or_create(d)?;
                cfg.save(d)?;
                let mut msg = "已保存，被控端下次启动时生效".to_owned();
                // The running service updates the rule itself; a stopped one cannot.
                if old.port != cfg.port && *d == paths::service_dir() && crate::install::service_points_here().is_some() {
                    if let Err(e) = crate::install::firewall_allow_service(cfg.port) {
                        msg.push_str(&format!("；但更新防火墙规则失败：{e:#}"));
                    }
                }
                Ok(msg)
            }
        }
    }

    pub fn clients(&mut self) -> Result<Vec<PairedClient>> {
        match self {
            Backend::Live(c) => Ok(c
                .clients()?
                .into_iter()
                .map(|c| PairedClient { fingerprint: c.fingerprint, name: c.name, paired_at: c.paired_at })
                .collect()),
            Backend::Offline(d) => Ok(AuthStore::list(d)),
        }
    }

    /// `fingerprint`: full or a unique prefix. Returns a message for the user.
    pub fn remove_client(&mut self, fingerprint: &str) -> Result<String> {
        match self {
            Backend::Live(c) => c.remove_client(fingerprint),
            Backend::Offline(d) => {
                let mut list = AuthStore::list(d);
                let c = auth::find_by_prefix(&list, fingerprint).map_err(|e| anyhow!(e))?.ok_or_else(|| anyhow!("没有这个客户端"))?;
                list.retain(|x| x.fingerprint != c.fingerprint);
                AuthStore::save_list(d, list)?;
                Ok(format!("已移除 {}", c.name))
            }
        }
    }

    pub fn disconnect(&mut self, reason: &str) -> Result<String> {
        match self {
            Backend::Live(c) => c.disconnect(reason),
            Backend::Offline(_) => bail!("被控端没有运行"),
        }
    }

    pub fn tail_log(&mut self, name: &str, lines: u32) -> Result<String> {
        match self {
            Backend::Live(c) => c.tail_log(name, lines),
            Backend::Offline(d) => Ok(logging::tail(d, name, lines as usize)),
        }
    }

    /// Does the running service update itself (else this program installs updates)?
    pub fn service_updates(&self) -> bool {
        matches!(self, Backend::Live(c) if self_updates(c))
    }

    /// Look for a newer release: through the service, or from here.
    pub fn check_update(&mut self) -> Result<cpb::UpdateStatus> {
        match self {
            Backend::Live(c) if self_updates(c) => c.check_update(),
            _ => crate::updater::check_here().map(|(s, _)| s),
        }
    }

    /// Install the newer release: the service hands over to its updater
    /// (stop, install, check, roll back if needed). Without a service that
    /// can, this program downloads the installer and starts it with its
    /// window; returns `true` then (the caller should exit so the installer
    /// can replace it).
    pub fn apply_update(&mut self) -> Result<(String, bool)> {
        match self {
            Backend::Live(c) if self_updates(c) => c.apply_update().map(|m| (m, false)),
            _ => crate::updater::install_here().map(|m| (m, true)),
        }
    }
}

fn self_updates(c: &ControlClient) -> bool {
    c.has_updates() && c.hello.mode == Mode::Service as i32
}
