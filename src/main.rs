// llm_proxy entry point.
//
// `-h`/`--help`: lists the available flags and exits (both platforms).
// `--silent`: no console; all output goes to the log file
//             (%LOCALAPPDATA%\llm_proxy\llm_proxy.log). For auto-start.
// (no flag): opens a native console with the output there (no log file).
// `--dev`: dev mode — the dashboard is served from CWD-relative
//          src/web/dashboard.html (re-read on every request); iterate with
//          a browser refresh instead of rebuilding. Use with a test config.
//             (nothing is written to the log file).
// `--no-animated-bg`: the dashboard is served without the animated
//          background (the canvas and its script are stripped).
// `--remote <host:port>`: bridge mode — no local backend; all traffic is
//          relayed to another llm_proxy (which runs the backend) and the
//          dashboard mirrors the remote's /api/stats feed.
//
// An optional positional argument (a .toml path) selects the config file.
// Log level is always INFO: runtime information, no debug.

mod app;
mod config;
mod proxy;
mod stats;
mod web;

// The backend lifecycle is cross-platform: a profile that declares an
// `exe` is managed by the proxy (spawn/stop/idle-unload); a profile
// without `exe` is external (pass-through). The tray is Windows-only:
// on Linux the proxy runs headless.
mod backend;
mod sysmon;
#[cfg(windows)]
mod tray;

#[tokio::main]
async fn main() {
    match app::run_app().await {
        Ok(()) => {}
        Err(e) => {
            app::fatal(&format!("llm_proxy: {e:?}"));
            std::process::exit(1);
        }
    }
}
