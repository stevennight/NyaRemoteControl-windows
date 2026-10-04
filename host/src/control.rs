//! Control pipe, service side: management tools (GUI, CLI) query and change
//! the running host through it instead of editing its files. See
//! `proto/control.proto` for the protocol and the access rules.

use std::sync::Arc;

use anyhow::Result;
use nya_proto::framing::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use nya_server_core::control::{config_from_pb, config_to_pb, pipe_path, CONTROL_VERSION, LOG_NAMES, MAX_MSG};
use crate::config::ServerConfig;
use crate::control_pb::{self as cpb, error::Code, request::Req, response::Resp, Mode};
use crate::state::State;

/// SYSTEM: full control (it creates the pipe instances). Administrators and
/// interactive users: read / write data only — no FILE_CREATE_PIPE_INSTANCE,
/// so nobody else can serve instances of this pipe.
const SERVICE_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;0x12019b;;;BA)(A;;0x12019b;;;IU)";

fn create(path: &str, mode: Mode, first: bool) -> Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).max_instances(16).in_buffer_size(64 << 10).out_buffer_size(1 << 20);
    Ok(if mode == Mode::Service {
        let mut sa = crate::winutil::PipeSa::new(SERVICE_PIPE_SDDL)?;
        unsafe { opts.create_with_security_attributes_raw(path, &mut sa.sa as *mut _ as *mut std::ffi::c_void)? }
    } else {
        // Standalone: the default DACL (creator, SYSTEM, administrators; others read-only).
        opts.create(path)?
    })
}

/// Serve the control pipe for the life of the host.
pub async fn serve(state: Arc<State>) {
    let path = pipe_path(state.mode);
    serve_at(state, path).await
}

async fn serve_at(state: Arc<State>, path: String) {
    let mut server = match create(&path, state.mode, true) {
        Ok(s) => Some(s),
        Err(e) => {
            // Most likely another instance (or an impostor) owns the name.
            tracing::error!("control pipe {path} unavailable: {e:#}");
            return;
        }
    };
    tracing::info!("control pipe {path}");
    loop {
        let conn = match server.take() {
            Some(s) => s,
            None => match create(&path, state.mode, false) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("control pipe: {e:#}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            },
        };
        if let Err(e) = conn.connect().await {
            tracing::warn!("control pipe connect: {e}");
            continue;
        }
        // Next instance first, so the name never disappears (clients would
        // take that for "not running"); serve this client either way.
        server = create(&path, state.mode, false).map_err(|e| tracing::error!("control pipe: {e:#}")).ok();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(conn, &state).await {
                tracing::debug!("control connection: {e:#}");
            }
        });
    }
}

async fn handle(mut pipe: NamedPipeServer, state: &Arc<State>) -> Result<()> {
    let mut admin = None;
    while let Some(req) = read_msg::<cpb::Request, _>(&mut pipe, MAX_MSG).await? {
        // Impersonation only works once the client has written something.
        let admin = *admin.get_or_insert_with(|| state.mode == Mode::Standalone || crate::winutil::pipe_client_is_admin(&pipe));
        let resp = match req.req {
            // Talks to GitHub; anyone may ask (the answer is in the public status too).
            Some(Req::CheckUpdate(_)) => {
                crate::update::check(state).await;
                Resp::Status(state.status(admin))
            }
            Some(Req::ApplyUpdate(_)) if admin => match crate::update::apply(state) {
                Ok(m) => done(m),
                Err(e) => err(Code::Invalid, format!("{e:#}")),
            },
            other => dispatch(state, admin, other),
        };
        write_msg(&mut pipe, &cpb::Response { id: req.id, resp: Some(resp) }).await?;
    }
    Ok(())
}

/// The full certificate fingerprint (pairing links pin it).
fn full_fp() -> String {
    crate::net::SERVER_FP.get().map(|f| f.to_hex()).unwrap_or_default()
}

fn err(code: Code, message: impl Into<String>) -> Resp {
    Resp::Error(cpb::Error { code: code as i32, message: message.into() })
}

fn done(message: impl Into<String>) -> Resp {
    Resp::Done(cpb::Done { message: message.into() })
}

fn dispatch(state: &State, admin: bool, req: Option<Req>) -> Resp {
    let Some(req) = req else { return err(Code::Unsupported, "被控端不认识这个请求（版本较旧？）") };
    match req {
        Req::Hello(h) => {
            tracing::debug!("control client: {} (control v{})", h.tool, h.control_version);
            Resp::Hello(cpb::HelloReply {
                control_version: CONTROL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").into(),
                mode: state.mode as i32,
                admin,
            })
        }
        Req::GetStatus(_) => Resp::Status(state.status(admin)),
        _ if !admin => err(Code::Denied, "需要管理员权限"),
        Req::GetPairing(_) => {
            Resp::Pairing(cpb::Pairing { code: state.auth.key().to_code(), fingerprint: state.fingerprint.clone(), fingerprint_hex: full_fp() })
        }
        Req::ResetPairingCode(_) => match state.auth.reset_key() {
            Ok(k) => {
                tracing::info!("pairing code reset from the control pipe");
                state.event(cpb::event::Kind::Service, "配对码已重新生成");
                Resp::Pairing(cpb::Pairing { code: k.to_code(), fingerprint: state.fingerprint.clone(), fingerprint_hex: full_fp() })
            }
            Err(e) => err(Code::Internal, format!("{e:#}")),
        },
        Req::GetConfig(_) => Resp::Config(config_to_pb(&state.config())),
        Req::SetConfig(s) => match set_config(state, config_from_pb(s.config.unwrap_or_default())) {
            Ok(r) => Resp::SetConfig(r),
            Err(e) => err(Code::Invalid, format!("{e:#}")),
        },
        Req::ListClients(_) => Resp::Clients(cpb::Clients {
            clients: state
                .auth
                .clients()
                .into_iter()
                .map(|c| cpb::PairedClient { fingerprint: c.fingerprint, name: c.name, paired_at: c.paired_at })
                .collect(),
        }),
        Req::RemoveClient(r) => remove_client(state, &r.fingerprint),
        Req::Disconnect(d) => {
            let reason = if d.reason.is_empty() { "被控端断开了连接".to_owned() } else { d.reason };
            if state.hub.kick(&reason) {
                done("已断开当前连接")
            } else {
                err(Code::NotFound, "当前没有连接")
            }
        }
        // Answered in `handle` (async / needs the shared state).
        Req::CheckUpdate(_) => Resp::Status(state.status(admin)),
        Req::ApplyUpdate(_) => err(Code::Internal, "ApplyUpdate is handled by the connection"),
        Req::TailLog(t) => {
            if !LOG_NAMES.contains(&t.name.as_str()) {
                return err(Code::Invalid, format!("没有名为 {:?} 的日志", t.name));
            }
            let lines = if t.lines == 0 { 200 } else { t.lines.min(5000) } as usize;
            Resp::Log(cpb::LogText { text: crate::logging::tail(&state.dir, &t.name, lines) })
        }
    }
}

fn remove_client(state: &State, prefix: &str) -> Resp {
    let c = match crate::auth::find_by_prefix(&state.auth.clients(), prefix) {
        Ok(Some(c)) => c,
        Ok(None) => return err(Code::NotFound, "没有这个客户端"),
        Err(e) => return err(Code::Invalid, e),
    };
    match state.auth.remove(&c.fingerprint) {
        Ok(_) => {
            state.event(cpb::event::Kind::Service, format!("已移除客户端 {}", c.name));
            for token in state.sessions_of(&c.fingerprint) {
                state.hub.kick_one(token, "此客户端已被被控端移除，需要重新配对");
            }
            done(format!("已移除 {}", c.name))
        }
        Err(e) => err(Code::Internal, format!("{e:#}")),
    }
}

fn set_config(state: &State, new: ServerConfig) -> Result<cpb::SetConfigReply> {
    new.validate()?;
    let old = state.config();
    new.save(&state.dir)?;
    state.set_config(new.clone());

    let mut notes = vec!["已保存".to_owned()];
    let host_restarted = new.host_part_differs(&old);
    if host_restarted {
        state.restart_host.notify_one();
        notes.push("采集进程已按新设置重启".into());
    }
    let listener_restarted = (new.port, &new.bind) != (old.port, &old.bind);
    if listener_restarted {
        if state.mode == Mode::Service && new.port != old.port {
            if let Err(e) = crate::install::firewall_allow(new.port) {
                tracing::warn!("firewall rule: {e:#}");
                notes.push(format!("更新防火墙规则失败：{e:#}"));
            }
        }
        state.rebind.notify_one();
        notes.push(format!("已改为监听 [{}]:{}，当前连接会断开", new.bind, new.port));
    }
    if new.check_updates && !old.check_updates {
        state.updates.wake.notify_one();
    }
    let needs_restart = new.log_level != old.log_level;
    if needs_restart {
        notes.push("日志级别在服务重启后生效".into());
    }
    tracing::info!("settings changed from the control pipe: {new:?}");
    state.event(cpb::event::Kind::Service, "设置已更新");
    Ok(cpb::SetConfigReply {
        config: Some(config_to_pb(&new)),
        host_restarted,
        listener_restarted,
        needs_restart,
        message: notes.join("；"),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nya_server_core::control::{is_not_running, ControlClient, ControlError};

    use super::*;
    use crate::auth::AuthStore;
    use crate::hub::Hub;
    fn remote_code(e: &anyhow::Error) -> Option<Code> {
        match e.downcast_ref::<ControlError>() {
            Some(ControlError::Remote { code, .. }) => Some(*code),
            _ => None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn control_pipe_round_trip() {
        let dir = std::env::temp_dir().join(format!("nya-control-test-{}", nya_proto::now_us()));
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let (hub, _cmds) = Hub::new();
        let state = State::new(Mode::Standalone, dir.clone(), ServerConfig::default(), "fp".into(), auth.clone(), hub);
        let path = format!(r"\\.\pipe\nya-control-test-{}", nya_proto::now_us());
        tokio::spawn(serve_at(state.clone(), path.clone()));

        let st = state.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = loop {
                match ControlClient::connect_path(&path, false, "test") {
                    Ok(c) => break c,
                    Err(e) if is_not_running(&e) => std::thread::sleep(Duration::from_millis(20)),
                    Err(e) => panic!("{e:#}"),
                }
            };
            assert!(c.is_admin());
            assert_eq!(c.hello.mode, Mode::Standalone as i32);

            let s = c.status().unwrap();
            assert_eq!(s.fingerprint, "fp");
            assert!(s.session.is_none());

            let before = c.pairing().unwrap().code;
            assert_eq!(before, st.auth.key().to_code());
            let after = c.reset_pairing_code().unwrap().code;
            assert_ne!(before, after);
            assert_eq!(after, st.auth.key().to_code(), "reset takes effect in the running host");

            // Invalid settings are refused and nothing is saved.
            let mut cfg = c.config().unwrap();
            cfg.port = 80;
            assert_eq!(remote_code(&c.set_config(&cfg).unwrap_err()), Some(Code::Invalid));
            assert_eq!(c.config().unwrap().port, ServerConfig::default().port);

            cfg.port = ServerConfig::default().port;
            cfg.encoder = "software".into();
            let r = c.set_config(&cfg).unwrap();
            assert!(r.host_restarted && !r.listener_restarted);
            assert_eq!(ServerConfig::load_or_create(&st.dir).unwrap().encoder, "software");

            assert!(c.clients().unwrap().is_empty());
            assert_eq!(remote_code(&c.remove_client("0123456789abcdef").unwrap_err()), Some(Code::NotFound));
            assert_eq!(remote_code(&c.disconnect("").unwrap_err()), Some(Code::NotFound));
            assert_eq!(remote_code(&c.tail_log("../secret", 10).unwrap_err()), Some(Code::Invalid));
        })
        .await
        .unwrap();
        // The encoder change asked the host to restart.
        tokio::time::timeout(Duration::from_secs(1), state.restart_host.notified()).await.expect("host restart requested");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The service's pipe ACL and the impersonation check, as far as a test
    /// can go without being SYSTEM: an interactive user can connect, and is
    /// an administrator exactly when this process is elevated.
    #[tokio::test(flavor = "multi_thread")]
    async fn service_pipe_acl_and_admin_check() {
        let dir = std::env::temp_dir().join(format!("nya-control-test-{}", nya_proto::now_us()));
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let (hub, _cmds) = Hub::new();
        let state = State::new(Mode::Service, dir.clone(), ServerConfig::default(), "fp".into(), auth, hub);
        let path = format!(r"\\.\pipe\nya-control-test-svc-{}", nya_proto::now_us());
        tokio::spawn(serve_at(state, path.clone()));
        let admin = tokio::task::spawn_blocking(move || {
            let mut c = loop {
                match ControlClient::connect_path(&path, false, "test") {
                    Ok(c) => break c,
                    Err(e) if is_not_running(&e) => std::thread::sleep(Duration::from_millis(20)),
                    Err(e) => panic!("{e:#}"),
                }
            };
            let admin = c.is_admin();
            assert!(c.status().is_ok());
            assert_eq!(c.pairing().is_ok(), admin);
            admin
        })
        .await
        .unwrap();
        assert_eq!(admin, nya_server_core::win::is_elevated());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_admin_gets_status_only() {
        let dir = std::env::temp_dir().join(format!("nya-control-test-{}", nya_proto::now_us()));
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let (hub, _cmds) = Hub::new();
        let state = State::new(Mode::Service, dir.clone(), ServerConfig::default(), "fp".into(), auth, hub);
        state.event(cpb::event::Kind::Service, "x");
        let Resp::Status(s) = dispatch(&state, false, Some(Req::GetStatus(Default::default()))) else { panic!() };
        assert!(s.recent.is_empty() && s.data_dir.is_empty());
        for req in [
            Req::GetPairing(Default::default()),
            Req::GetConfig(Default::default()),
            Req::ListClients(Default::default()),
            Req::TailLog(cpb::TailLog { name: "service".into(), lines: 1 }),
        ] {
            let r = dispatch(&state, false, Some(req));
            assert!(matches!(r, Resp::Error(ref e) if e.code == Code::Denied as i32), "{r:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
