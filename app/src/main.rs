//! NyaRemoteControl for Windows: the program users open.
//!
//! Double-click: the main window — remote control of other computers, and
//! this computer as a host ("本机", managing the service nya-server-svc.exe).
//! `NyaRemoteControl connect <host>` connects right away; `diag` / `hosts`
//! print to the terminal they were started from.

#![windows_subsystem = "windows"]

mod app;
mod audio;
mod caps;
mod gamepad;
mod clipboard;
mod config;
mod diag;
mod events;
mod input;
mod mic;
mod net;
mod printing;
mod render;
mod session;
mod stats;
mod transfer;
mod tray;
mod ui;
mod usb;
mod video;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use nya_transport::Identity;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use winit::event_loop::EventLoop;

use crate::config::ClientConfig;
use crate::events::{Ui, UiEvent};

/// Version for display: `0.2.0 (1a2b3c4d)` (commit id from build.rs; `+` = uncommitted changes).
pub fn version() -> String {
    match env!("NYA_GIT_HASH") {
        "" => env!("CARGO_PKG_VERSION").to_owned(),
        h => format!("{} ({h})", env!("CARGO_PKG_VERSION")),
    }
}

#[derive(Parser)]
#[command(name = "NyaRemoteControl", version = concat!(env!("CARGO_PKG_VERSION"), " ", env!("NYA_GIT_HASH")), about = "NyaRemoteControl 远程桌面")]
struct Cli {
    /// Page the window opens on (host = 本机).
    #[arg(long, global = true, hide = true)]
    page: Option<String>,
    /// Start in the tray, without the window (starting with Windows).
    #[arg(long, hide = true)]
    tray: bool,
    /// Started by the running program in its place (reopened as administrator).
    #[arg(long, hide = true)]
    relaunched: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 直接连接被控端（已保存的名称或地址）
    Connect {
        target: String,
        /// 保存时使用的名称
        #[arg(long)]
        name: Option<String>,
        /// office | game
        #[arg(long)]
        mode: Option<String>,
        /// 被控端显示器编号（0 = 主显示器）
        #[arg(long)]
        display: Option<u32>,
        #[arg(long)]
        fullscreen: bool,
        /// auto | nvenc | qsv | amf | software
        #[arg(long)]
        encoder: Option<String>,
        /// auto | h264 | hevc | av1
        #[arg(long)]
        codec: Option<String>,
        /// auto | 420 | 444
        #[arg(long)]
        chroma: Option<String>,
        /// 码率 kbit/s（0 = 自动）
        #[arg(long)]
        bitrate: Option<u32>,
        /// 禁用硬件解码
        #[arg(long)]
        sw_decode: bool,
    },
    /// 列出 / 删除保存的被控端
    Hosts {
        #[arg(long)]
        remove: Option<String>,
    },
    /// 诊断：显卡、硬件解码能力、音频
    Diag,
}

/// Show an error to a user who has no console.
pub fn fatal(msg: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    tracing::error!("{msg}");
    eprintln!("错误：{msg}");
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), &HSTRING::from("NyaRemoteControl"), MB_OK | MB_ICONERROR);
    }
}

/// When started from a terminal, write to it despite the GUI subsystem.
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn init_logging(dir: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("NYA_LOG").unwrap_or_else(|_| "info".into());
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("client")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir.join("logs"))
        .ok();
    let (file, guard) = match appender {
        Some(a) => {
            let (nb, g) = tracing_appender::non_blocking(a);
            (Some(tracing_subscriber::fmt::layer().with_writer(nb).with_ansi(false)), Some(g))
        }
        None => (None, None),
    };
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init();
    guard
}

fn main() {
    attach_console();
    if let Err(e) = real_main() {
        fatal(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    // A clicked pairing link (`nyaremote://…`, registered by the installer).
    let link = std::env::args_os().nth(1).and_then(|a| a.into_string().ok()).filter(|a| nya_transport::invite::Invite::looks_like(a));
    let cli = if link.is_some() { Cli::parse_from(["NyaRemoteControl"]) } else { Cli::parse() };
    let dir = config::data_dir();
    std::fs::create_dir_all(&dir)?;
    let _log = init_logging(&dir);
    nya_media::check_runtime_versions()?;
    nya_media::init_log_level();
    let mut cfg = ClientConfig::load(&dir)?;

    let auto_connect = match cli.cmd {
        Some(Cmd::Diag) => return diag::run(),
        Some(Cmd::Hosts { remove }) => {
            if let Some(r) = remove {
                cfg.hosts.retain(|h| h.name != r && h.address != r);
                cfg.save(&dir)?;
            }
            for h in &cfg.hosts {
                println!("{}  {}  {}", h.name, h.address, if h.fingerprint.is_empty() { "未配对" } else { "已配对" });
            }
            return Ok(());
        }
        Some(Cmd::Connect { target, name, mode, display, fullscreen, encoder, codec, chroma, bitrate, sw_decode }) => {
            // Command-line overrides apply to this connection only.
            let o = config::Overrides { mode, display, fullscreen, encoder, codec, chroma, bitrate_kbps: bitrate, sw_decode };
            Some((target, name, o))
        }
        None => None,
    };
    // One program per user session: started again, it shows the running one.
    let instance = if auto_connect.is_none() {
        match tray::claim(&dir, link.as_deref(), cli.relaunched) {
            Some(i) => Some(i),
            None => return Ok(()),
        }
    } else {
        None
    };

    let identity = Identity::load_or_create(&dir)?;
    let rt = tokio::runtime::Runtime::new()?;
    let event_loop = EventLoop::<UiEvent>::with_user_event().build().map_err(|e| anyhow!("{e}"))?;
    let ui = Ui::new(event_loop.create_proxy());
    if let Some(i) = instance {
        i.listen(ui.clone(), dir.clone());
    }
    // A start-with-Windows entry follows the program (updates, moves).
    tray::autostart::refresh();
    let mut app = app::App::new(rt.handle().clone(), ui, dir, cfg, identity, auto_connect, cli.page, cli.tray && link.is_none(), link);
    event_loop.run_app(&mut app).map_err(|e| anyhow!("{e}"))?;
    // Let the Bye go out.
    rt.shutdown_timeout(std::time::Duration::from_millis(300));
    Ok(())
}
