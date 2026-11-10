// Wiring: logging, console mode, config, proxy + stats servers, tray,
// watcher, action dispatcher. Shared by the silent and console entry points.

#[cfg(windows)]
use std::sync::mpsc;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::time::{Duration, UNIX_EPOCH};

use tracing::{error, info};
#[cfg(windows)]
use tracing::warn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::{backend, config, proxy, stats, web};
#[cfg(windows)]
use crate::tray;

/// Console modes: Windows only (the Linux build is headless, always
/// logging to stdout).
#[cfg(windows)]
pub enum Mode {
    /// No visible console, no backend log; llm_proxy logs go to a file.
    Silent,
    /// Visible native console; backend stdout/stderr stream into it.
    Console,
}

#[cfg(windows)]
fn is_console(mode: &Mode) -> bool {
    matches!(mode, Mode::Console)
}

/// %LOCALAPPDATA%\llm_proxy\llm_proxy.log (fallback: next to the exe).
/// Env override LLM_PROXY_LOG: lets a second (test) instance write to its
/// own log file instead of the production one.
fn log_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("LLM_PROXY_LOG") {
        if !p.is_empty() {
            return Some(std::path::PathBuf::from(p));
        }
    }
    let base: std::path::PathBuf = match std::env::var_os("LOCALAPPDATA") {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf()))?,
    };
    let dir = base.join("llm_proxy");
    if std::fs::create_dir_all(&dir).is_ok() {
        Some(dir.join("llm_proxy.log"))
    } else {
        None
    }
}

#[cfg(windows)]
struct FileLog(std::fs::File);

#[cfg(windows)]
impl std::io::Write for FileLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Runtime info only (never debug, no RUST_LOG override):
/// Silent -> log file; Console -> stdout (no file).
#[cfg(windows)]
fn init_logging(mode: &Mode) {
    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"));

    match mode {
        Mode::Silent => {
            match log_path().and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
                    .map(FileLog)
            }) {
                Some(f) => registry
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(f.0.try_clone().expect("clone log file")),
                    )
                    .init(),
                None => registry.init(),
            }
        }
        Mode::Console => registry.with(tracing_subscriber::fmt::layer()).init(),
    }
}

/// Linux build: always stdout.
#[cfg(not(windows))]
fn init_logging() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

#[cfg(windows)]
fn apply_console_mode(mode: &Mode) {
    unsafe {
        match mode {
            Mode::Silent => {
                // The release exe is built with /SUBSYSTEM:WINDOWS, so no
                // console is ever allocated. This is a safety fallback for
                // console-subsystem builds (e.g. debug): hide any console.
                let hw = windows::Win32::System::Console::GetConsoleWindow();
                if !hw.0.is_null() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                        hw,
                        windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
                    );
                }
            }
            Mode::Console => {
                // Reuse our terminal or allocate a console (Explorer launch).
                if windows::Win32::System::Console::GetStdHandle(
                    windows::Win32::System::Console::STD_OUTPUT_HANDLE,
                )
                .is_err()
                {
let _ = windows::Win32::System::Console::AllocConsole();
                }
                // Enable ANSI/VT interpretation: Windows Terminal does it
                // already, a plain conhost needs the handle flag or the
                // escape codes show up as literal text.
                use windows::Win32::System::Console::{
                    CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    GetConsoleMode, SetConsoleMode,
                };
                if let Ok(h) = windows::Win32::System::Console::GetStdHandle(
                    windows::Win32::System::Console::STD_OUTPUT_HANDLE,
                ) {
let mut mode = CONSOLE_MODE(0);
                    if GetConsoleMode(h, &mut mode).is_ok() {
                        let _ = SetConsoleMode(
                            h,
                            mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                        );
                    }
                }
                let title: Vec<u16> = "llm_proxy"
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let _ = windows::Win32::System::Console::SetConsoleTitleW(
                    windows::core::PCWSTR(title.as_ptr()),
                );
            }
        }
    }
}

#[cfg(windows)]
fn open_url(url: &str) {
    unsafe {
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        let _ = windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::PCWSTR(verb.as_ptr()),
            windows::core::PCWSTR(wide.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_MAXIMIZE,
        );
    }
}

/// Fatal error sink for a GUI-subsystem exe (no stdio: println!/eprintln!
/// would panic). Always goes to the log file; also to the console when one
/// exists (console mode; the console then waits for Enter).
pub fn fatal(msg: &str) {
    error!(%msg, "fatal");
    if let Some(p) = log_path() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!("unix_ts={}  FATAL {msg}\n", ts);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }
    #[cfg(windows)]
    unsafe {
        if windows::Win32::System::Console::GetStdHandle(
            windows::Win32::System::Console::STD_OUTPUT_HANDLE,
        )
        .is_ok()
        {
            // A console exists (console mode): print the error and wait for
            // Enter so the window doesn't close before it can be read.
            eprintln!("{msg}");
            eprintln!("[llm_proxy] fatal error — press Enter to exit");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
    }
    #[cfg(not(windows))]
    eprintln!("{msg}");
}

/// Usage help (printed by `-h`/`--help`, both platforms).
const HELP: &str = "\
llm_proxy — OpenAI-compatible LLM proxy with dashboard, tray (Windows) and bridge.

Usage:
  llm_proxy [FLAGS] [config.toml]

Flags:
  -h, --help            Show this help and exit
  --silent              (Windows only) no console window; the proxy log goes
                        to %LOCALAPPDATA%\\llm_proxy\\llm_proxy.log (for autostart)
  --dev                 development: the dashboard is served from
                        src/web/dashboard.html relative to the CWD and re-read
                        on every request (no log file)
  --no-animated-bg      serve the dashboard without the animated background
  --router              router mode: the model is chosen by the client (the
                        model field in the chat body / POST /models/load); the
                        tray and dashboard hide their switch/load/stop controls
                        (the client is the sole driver, no active-profile
                        default)
  --remote <host:port>  bridge mode: no local backend — all traffic is relayed
                        to another llm_proxy; the dashboard mirrors the
                        remote's /api/stats feed

Positional argument:
  <config.toml>         config file (default: llm_proxy.toml next to the
                        executable, falling back to the CWD)

Platforms:
  Windows — tray icon, console/silent modes; local or bridge.
  Linux   — headless (no tray, no console): bridge mode (remote) or local
            monitoring (profiles with backend_host/backend_port).";

pub async fn run_app() -> anyhow::Result<()> {
    let cli_args: Vec<String> = std::env::args().collect();
    // -h/--help: print the usage and exit — before any console/logging setup.
    // On Windows a launch without console (Explorer/shortcut; the release
    // exe is /SUBSYSTEM:WINDOWS) gets a fresh AllocConsole that would close
    // on exit: wait for Enter so the text can be read (same UX as the
    // fatal-error path).
    if cli_args.iter().any(|a| a == "-h" || a == "--help") {
        #[cfg(windows)]
        {
            use windows::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
            let fresh_console = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.is_err();
            apply_console_mode(&Mode::Console);
            println!("{HELP}");
            if fresh_console {
                eprintln!("[llm_proxy] press Enter to exit");
                let _ = std::io::stdin().read_line(&mut String::new());
            }
        }
        #[cfg(not(windows))]
        println!("{HELP}");
        return Ok(());
    }
    #[cfg(windows)]
    let mode = if std::env::args().any(|a| a == "--silent") {
        Mode::Silent
    } else {
        Mode::Console
    };
    #[cfg(windows)]
    {
        apply_console_mode(&mode);
        init_logging(&mode);
    }
    #[cfg(not(windows))]
    init_logging();

    // Router mode: the client picks the model (field in the chat body /
    // POST /models/load); the tray and dashboard hide their switch/load/
    // stop controls (the client is the sole driver).
    let router = cli_args.iter().any(|a| a == "--router");
    let stats = stats::Stats::new();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3600))
        .build()?;
    // Where the backend's stdout/stderr lines go:
    //  - Windows console mode: the terminal; silent mode: discarded
    //    (the log file must hold only proxy logs, otherwise it grows
    //    without bound);
    //  - Linux: the log stream at debug level (RUST_LOG=debug) — there is
    //    no console window in silent/service use.
    let sink: Option<backend::LineSink> = {
        #[cfg(windows)]
        {
            match mode {
                Mode::Console => Some(std::sync::Arc::new(|l: String| println!("[backend] {l}"))),
                Mode::Silent => None,
            }
        }
        #[cfg(not(windows))]
        {
            Some(std::sync::Arc::new(|l: String| tracing::debug!("[backend] {l}")))
        }
    };
    let backend = backend::Backend::new(stats.clone(), client.clone(), sink);

    let mut cfg = config::load_config()?;
    // `--remote <host:port>`: bridge mode — this instance relays to another
    // llm_proxy (which runs the backend). Takes precedence over profiles.
    if let Some(i) = cli_args.iter().position(|a| a == "--remote") {
        let addr = cli_args
            .get(i + 1)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| anyhow::anyhow!("--remote requires a <host:port> argument"))?;
        cfg.remote = Some(addr.clone());
    }
    // Linux build: either bridge mode (a remote target, in the toml or via
    // --remote) or local monitoring mode (at least one profile declaring
    // the external backend's address).
    #[cfg(not(windows))]
    if cfg.remote.is_none() && cfg.profiles.is_empty() {
        return Err(anyhow::anyhow!(
            "no mode configured: set `remote = \"host:port\"` for bridge mode, or define a profile with backend_host/backend_port for local monitoring mode"
        ));
    }
    // Local mode: log the backend arrangement at startup — managed
    // (the proxy spawns the `exe`) or external (pass-through).
    #[cfg(not(windows))]
    if cfg.remote.is_none() {
        let p = cfg.active_profile();
        let profile_name = cfg.active_profile.clone().unwrap_or_default();
        if !p.exe.trim().is_empty() {
            info!(
                profile = %profile_name,
                exe = %p.exe,
                backend = %format!("{}:{}", p.backend_host(), p.backend_port()),
                health = %p.health_url(),
                "local mode: managed backend (spawned and stopped by the proxy)"
            );
        } else {
            info!(
                profile = %profile_name,
                backend = %format!("{}:{}", p.backend_host(), p.backend_port()),
                health = %p.health_url(),
                "local monitoring mode: external backend (not spawned by the proxy)"
            );
        }
    }
    let started_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d|d.as_millis())
        .unwrap_or(0);
    let sysmon_snap = crate::sysmon::new_snapshot();
    #[cfg(windows)]
    let (action_tx, action_rx) = mpsc::channel::<tray::Action>();
    let state = std::sync::Arc::new(proxy::AppState {
        config: cfg.clone(),
        active: std::sync::Mutex::new(cfg.active_profile.clone().unwrap_or_default()),
        router,
        backend: backend.clone(),
        client,
        stats: stats.clone(),
        ops: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        started_ms,
        sysmon: sysmon_snap.clone(),
        remote_stats: std::sync::Arc::new(std::sync::Mutex::new(None)),
        metric_hist: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        #[cfg(windows)]
        admin_tx: action_tx.clone(),
    });

    // GPU/CPU/RAM monitor (feeds the dashboard, 2 s cadence).
    tokio::spawn(crate::sysmon::run(sysmon_snap));

    if state.is_remote() {
        // Bridge mode: no local backend — the idle-unload watcher does not
        // apply. Poll the remote /api/stats feed for the dashboard instead.
        let st = state.clone();
        let cli = state.client.clone();
        let tok = state.config.token.clone();
        // Serializes the first post with tray readiness (startup race, see
        // the dot task): last_remote tracks what was actually POSTED, not
        // just what was observed.
        #[cfg(windows)]
        let mut last_remote: (Vec<(String, String)>, String) = (Vec::new(), String::new());
        tokio::spawn(async move {
            loop {
                let url = format!("{}/api/stats", st.remote_base());
                let mut req = cli.get(&url).timeout(Duration::from_secs(2));
                if let Some(t) = &tok {
                    if !t.is_empty() {
                        req = req.header("Authorization", format!("Bearer {t}"));
                    }
                }
                match req
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(text) = resp.text().await {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                #[cfg(windows)]
                                {
                                    // Mirror the remote profile list into the tray
                                    // menu (call only when it changed).
                                    let (profiles, active) = v
                                        .get("profiles")
                                        .and_then(|p| p.as_array())
                                        .map(|arr| {
                                            let entries: Vec<(String, String, bool)> = arr
                                                .iter()
                                                .map(|p| {
                                                    (
                                                        p.get("id")
                                                            .and_then(|v| v.as_str())
                                                            .unwrap_or_default()
                                                            .to_string(),
                                                        p.get("name")
                                                            .and_then(|v| v.as_str())
                                                            .unwrap_or_default()
                                                            .to_string(),
                                                        p.get("active")
                                                            .and_then(|a| a.as_bool())
                                                            .unwrap_or(false),
                                                    )
                                                })
                                                .filter(|(id, _, _)| !id.is_empty())
                                                .collect();
                                            let active = entries
                                                .iter()
                                                .find(|(_, _, a)| *a)
                                                .or_else(|| entries.first())
                                                .map(|(id, _, _)| id.clone())
                                                .unwrap_or_default();
                                            (
                                                entries
                                                    .into_iter()
                                                    .map(|(id, name, _)| (id, name))
                                                    .collect::<Vec<_>>(),
                                                active,
                                            )
                                        })
                                        .unwrap_or_default();
                                    if (profiles.clone(), active.clone()) != last_remote
                                        && crate::tray::is_ready()
                                    {
                                        last_remote = (profiles.clone(), active.clone());
                                        tray::set_remote_profiles(profiles, active);
                                    }
                                }
                                *st.remote_stats.lock().unwrap() = Some(v);
                            }
                        }
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    } else {
        // Idle-unload / process-death watcher (idle timeout read from the
        // active profile each pass — profiles are switchable at runtime).
        // Safe for external profiles: no child process, idle check only in
        // Ready state.
        let watch = backend.clone();
        let watch_state = state.clone();
        tokio::spawn(async move {
            watch.watch(move || watch_state.active_profile().idle_unload_secs).await
        });
    }

    // Bind both servers up front so a port conflict fails loudly in main.
    // `bind` (default 127.0.0.1): 0.0.0.0 exposes proxy + stats on the LAN
    // (bridge mode — safe only together with a token).
    let host = cfg.bind.clone().unwrap_or_else(|| "127.0.0.1".into());
    let proxy_addr = format!("{host}:{}", state.config.proxy.port);
    let stats_addr = format!("{host}:{}", state.config.proxy.stats_port);
    // `0.0.0.0`/`::` bind every interface but are not dialable: self-
    // referring URLs (tray open, console print, tooltip) must use loopback.
    #[cfg(windows)]
    let stats_display_addr = match host.as_str() {
        "0.0.0.0" | "::" => format!("127.0.0.1:{}", state.config.proxy.stats_port),
        _ => stats_addr.clone(),
    };
    #[cfg(windows)]
    let stats_url = match state.config.token.as_deref() {
        Some(t) if !t.is_empty() => format!("http://{stats_display_addr}?t={t}"),
        _ => format!("http://{stats_display_addr}"),
    };

    // Tray on a dedicated thread (needs its own Win32 message pump).
    // Bridge mode: no profiles to switch — the tray shows the remote
    // address and hides profile/stop entries.
    #[cfg(windows)]
    let (profile_names, active) = if state.is_remote() {
        (Vec::new(), format!("remote {}", state.remote_addr()))
    } else {
        // Deterministic menu order (HashMap keys are unordered: the profile
        // positions in the tray menu would otherwise change on every launch).
        let mut names: Vec<String> = cfg.profiles.keys().cloned().collect();
        names.sort();
        (names, cfg.active_profile.clone().unwrap_or_default())
    };
    #[cfg(windows)]
    let tray_proxy_addr = proxy_addr.clone();
    #[cfg(windows)]
    let tray_stats_addr = stats_display_addr.clone();
    #[cfg(windows)]
    std::thread::spawn(move || {
        let t = tray::Tray::new(
            profile_names,
            &active,
            true,
            &tray_proxy_addr,
            &tray_stats_addr,
            router,
        );
        if let Err(e) = t.run(action_tx) {
            error!(err = %e, "tray thread failed");
        }
    });

    // Tray status dot: GREEN while the backend is ready — local mode:
    // this proxy's backend; bridge mode: the remote's backend.state from
    // the mirrored /api/stats feed (the remote proxy being reachable is NOT
    // enough: its backend may still be stopped). RED otherwise. Polls once
    // per second and posts changes to the tray thread (it owns the Win32
    // icon handles and the message pump).
    #[cfg(windows)]
    let dot = backend.clone();
    #[cfg(windows)]
    let dot_state = state.clone();
    #[cfg(windows)]
    tokio::spawn(async move {
        let mut dot_last = false;
        loop {
            let r = if dot_state.is_remote() {
                dot_state
                    .remote_stats
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|v| v.get("backend").and_then(|b| b.get("state")).and_then(|s| s.as_str()))
                    == Some("ready")
            } else {
                dot.is_ready()
            };
            // Gate on tray readiness: in bridge mode the first probe wins
            // the startup race (loopback health is ~1 ms, the tray window
            // is created ~50 ms later) and the transition would be lost.
            if r != dot_last && crate::tray::is_ready() {
                dot_last = r;
                tray::set_backend_state(r);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let proxy_listener = tokio::net::TcpListener::bind(&proxy_addr).await?;
    let stats_listener = tokio::net::TcpListener::bind(&stats_addr).await?;
    let proxy_handle =
        tokio::spawn(proxy::run_proxy(state.clone(), proxy_addr.clone(), proxy_listener));
    tokio::spawn(web::run_stats(state.clone(), stats_addr.clone(), stats_listener));

    #[cfg(windows)]
    if is_console(&mode) {
        if state.is_remote() {
            println!(
                "llm_proxy started (bridge): proxy {}   stats {}   remote {}",
                proxy_addr, stats_addr, state.remote_addr()
            );
            println!("stats dashboard: {}   (tray: open/exit)", stats_url);
        } else {
            let p = state.config.active_profile();
            println!(
                "llm_proxy started: proxy {}   stats {}   profile {}   backend {}:{}",
                proxy_addr,
                stats_addr,
                state.config.active_profile.clone().unwrap_or_default(),
                p.backend_host(),
                p.backend_port()
            );
            println!("stats dashboard: {}   (tray: stop/exit)", stats_url);
        }
    } else {
        info!(
            proxy = %proxy_addr,
            stats = %stats_addr,
            "llm_proxy started (silent)"
        );
        if let Some(p) = log_path() {
            info!(path = %p.display(), "log file");
        }
    }

    // Action dispatcher.
    #[cfg(windows)]
    loop {
        match action_rx.recv() {
            Ok(tray::Action::LoadProfile(name)) => {
                // Shared path with the remote-admin command (web.rs).
                proxy::start_switch(&state, name);
            }
            Ok(tray::Action::RemoteLoadProfile(id)) => {
                let remote = state.remote_addr();
                let client = state.client.clone();
                let tok = state.config.token.clone();
                let body = format!("{{\"profile\": {id:?}}}");
                tokio::spawn(async move {
                    remote_admin(&client, &remote, tok.as_deref(), "profile", Some(&body)).await;
                });
            }
            Ok(tray::Action::RemoteStop) => {
                let remote = state.remote_addr();
                let client = state.client.clone();
                let tok = state.config.token.clone();
                tokio::spawn(async move {
                    remote_admin(&client, &remote, tok.as_deref(), "stop", None).await;
                });
            }
            Ok(tray::Action::Stop) => {
                state.backend.stop().await;
            }
            Ok(tray::Action::OpenStats) => {
                // Include the token in the URL when one is configured: the
                // browser needs `?t=` for the stats routes.
                info!(url = %stats_display_addr, "open stats page requested");
                open_url(&match state.config.token.as_deref() {
                    Some(t) if !t.is_empty() => format!("http://{stats_display_addr}?t={t}"),
                    _ => format!("http://{stats_display_addr}"),
                });
            }
            Ok(tray::Action::Restart) => {
                info!("restart requested");
                relaunch_self();
            }
            Ok(tray::Action::Quit) => {
                info!("quit requested");
                break;
            }
            Err(_) => break,
        }
    }

    // Linux build: no tray — run until Ctrl+C.
    #[cfg(not(windows))]
    {
        if state.is_remote() {
            info!(
                remote = %state.remote_addr(),
                proxy = %proxy_addr,
                stats = %stats_addr,
                "llm_proxy running (bridge) — Ctrl+C to stop"
            );
        } else {
            let p = state.config.active_profile();
            info!(
                profile = %state.config.active_profile.clone().unwrap_or_default(),
                backend = %format!("{}:{}", p.backend_host(), p.backend_port()),
                proxy = %proxy_addr,
                stats = %stats_addr,
                "llm_proxy running (local monitoring) — Ctrl+C to stop"
            );
        }
        let _ = tokio::signal::ctrl_c().await;
    }

    proxy_handle.abort();
    // Stop the managed backend (no-op when it never started / external
    // profile). On Windows the job object would kill the tree anyway;
    // on Linux this is what releases the process group.
    state.backend.stop().await;
    std::process::exit(0);
}

/// Tray "Restart proxy": spawn a new instance of ourselves with the same
/// command line and exit. The new instance re-reads the config from
/// scratch (profiles, ports, token, active profile). The backend does not
/// survive the switch: when this process exits, its job object kills the
/// backend tree, and the new instance re-spawns the backend on the next
/// request (first request cold). Spawn + immediate exit: no delay, so the
/// old process releases the ports (bind-upfront) before the new one binds
/// them. If the spawn fails, this instance keeps running and logs the error.
#[cfg(windows)]
fn relaunch_self() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "restart failed — keeping the current instance");
            return;
        }
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match std::process::Command::new(&exe)
        .args(&args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: no console flash
        .spawn()
    {
        Ok(child) => {
            info!(pid = child.id(), exe = %exe.display(), ?args, "restart: new instance spawned, exiting");
            std::process::exit(0);
        }
        Err(e) => {
            tracing::error!(error = %e, "restart failed — keeping the current instance");
        }
    }
}

/// Bridge mode: send a remote-admin command to the remote llm_proxy
/// (`/api/admin/<path>`) carrying the remote's token. Never blocks the
/// dispatcher — the request runs in the background and logs the outcome.
#[cfg(windows)]
async fn remote_admin(
    client: &reqwest::Client,
    remote: &str,
    token: Option<&str>,
    path: &str,
    body: Option<&str>,
) {
    let url = format!("http://{remote}/api/admin/{path}");
    let mut req = client.post(&url).timeout(Duration::from_secs(5));
    if let Some(t) = token {
        if !t.is_empty() {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
    }
    if let Some(b) = body {
        req = req.body(b.to_string());
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                info!(remote = %remote, path, status = %status.as_u16(), %text, "remote-admin ok");
            } else {
                warn!(remote = %remote, path, status = %status.as_u16(), %text, "remote-admin failed");
            }
        }
        Err(e) => warn!(remote = %remote, path, err = %e, "remote-admin request failed"),
    }
}
