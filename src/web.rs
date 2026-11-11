// Always-on web stats server (separate port, see config `stats_port`):
// serves the dashboard and the /api/stats JSON feed.

use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::extract::Request;
use axum::middleware::Next;
use futures::StreamExt;
use axum::Router;
use tracing::info;
#[cfg(windows)]
use tracing::warn;

use crate::backend::BackendState;
use crate::proxy::AppState;

fn dev_mode() -> bool {
    std::env::args().any(|a| a == "--dev")
}

/// `--no-animated-bg`: strip the animated background from the dashboard.
fn bg_off() -> bool {
    std::env::args().any(|a| a == "--no-animated-bg")
}

/// Removes the background canvas + its script block (marker-guarded).
fn strip_bg(html: &str) -> String {
    if !bg_off() {
        return html.to_string();
    }
    let start = match html.find("<!-- bg:start -->") {
        Some(i) => i,
        None => return html.to_string(),
    };
    let end = match html.find("<!-- bg:end -->") {
        Some(i) => i + "<!-- bg:end -->".len(),
        None => return html.to_string(),
    };
    let mut out = String::with_capacity(html.len());
    out.push_str(&html[..start]);
    out.push_str(&html[end..]);
    out
}

/// Serves the dashboard. With `--dev` the page is re-read from disk on
/// every request (CWD-relative `src/web/dashboard.html`, falling back to the
/// repo root found by walking up from the exe — the mock-GPU flow copies the
/// exe into `test\mock_gpu` and runs it from there), so layout iterations
/// need only a browser refresh — no rebuild. Falls back to the embedded
/// copy when the file is missing.
async fn dashboard() -> Response {
    if dev_mode() {
        for p in dev_dashboard_paths() {
            match std::fs::read_to_string(&p) {
                Ok(html) => {
                    return (
                        StatusCode::OK,
                        [("content-type", "text/html; charset=utf-8".to_string())],
                        strip_bg(&html),
                    )
                        .into_response()
                }
                Err(e) => {
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if WARNED
                        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        tracing::warn!(
                            err = %e,
                            ?p,
                            "--dev: dashboard file not found (run from the repo root); using the embedded copy"
                        );
                    }
                }
            }
        }
    }
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8".to_string())],
        strip_bg(include_str!("web/dashboard.html")),
    )
        .into_response()
}

/// On-disk candidates for the dev dashboard: CWD-relative first (repo-root
/// flow), then the repo root found by walking up from the exe directory
/// (the exe may have been copied into a subfolder, e.g. `test\mock_gpu`).
fn dev_dashboard_paths() -> Vec<std::path::PathBuf> {
    let rel = std::path::Path::new("src/web/dashboard.html");
    let mut v = vec![rel.to_path_buf()];
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        for _ in 0..8 {
            match dir {
                Some(d) => {
                    let cand = d.join("src/web/dashboard.html");
                    if cand.is_file() {
                        v.push(cand);
                    }
                    dir = d.parent();
                }
                None => break,
            }
        }
    }
    v
}

async fn api_stats(st: axum::extract::State<Arc<AppState>>) -> Response {
    stats_json(&st.0).await
}

/// Remote-admin: switch the active profile of THIS proxy. The bridge's
/// tray sends it (it relays the user's command to the server it is
/// attached to). Token-gated like every other stats route. Bridge
/// instances have no local profiles: 405.
pub async fn api_admin_profile(
    st: axum::extract::State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    if st.0.is_remote() {
        return (StatusCode::NOT_ACCEPTABLE, "bridge mode: no local profiles").into_response();
    }
    let name = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("profile").and_then(|v| v.as_str()).map(str::to_string));
    let Some(name) = name else {
        return (StatusCode::BAD_REQUEST, r#"expected {"profile": "<name>"}"#).into_response();
    };
    if !st.0.config.profiles.contains_key(&name) {
        return (StatusCode::NOT_FOUND, format!("unknown profile: {name}")).into_response();
    }
    info!(%name, "remote-admin: switch profile requested");
    // Windows: serialized on the SAME action channel as the local tray —
    // single shared lifecycle, last command wins (local tray + remote
    // admin, several bridges on the same server).
    #[cfg(windows)]
    match st.0
        .admin_tx
        .send(crate::tray::Action::LoadProfile(name))
    {
        Ok(()) => {}
        Err(e) => {
            warn!(%e, "admin action channel closed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    #[cfg(not(windows))]
    {
        // Linux: no tray/dispatcher — run the switch directly (routing +
        // persistence; for a managed profile start_switch also stops the
        // old backend and spawns the new one, serialized by the ops lock).
        crate::proxy::start_switch(&st.0, name);
    }
    (StatusCode::ACCEPTED, "switch started").into_response()
}

/// Remote-admin: stop the backend of THIS proxy. 406 when the active
/// profile is external (no `exe` — not managed by the proxy) or in
/// bridge mode.
pub async fn api_admin_stop(st: axum::extract::State<Arc<AppState>>) -> Response {
    if st.0.is_remote() {
        return (StatusCode::NOT_ACCEPTABLE, "bridge mode: no local backend").into_response();
    }
    let profile = st.0.active_profile();
    if profile.exe.trim().is_empty() {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "active profile is external: backend not managed by the proxy",
        )
            .into_response();
    }
    info!("remote-admin: stop backend requested");
    #[cfg(windows)]
    {
        // Same serialized channel as the local tray's Stop.
        match st.0.admin_tx.send(crate::tray::Action::Stop) {
            Ok(()) => {}
            Err(e) => {
                warn!(%e, "admin action channel closed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }
    #[cfg(not(windows))]
    {
        // Linux: no tray dispatcher — run the stop directly (the state
        // machine serializes it with switches/wake-on-traffic).
        st.0.backend.stop().await;
    }
    (StatusCode::ACCEPTED, "stop started").into_response()
}

/// Chat bridge for the dashboard: same-origin POST from the browser is
/// forwarded to the proxy's own port (in-process loopback), so the request
/// goes through the normal proxy path — same stats recording, same active
/// profile — and the SSE stream is piped back to the browser untouched.
async fn api_chat(
    st: axum::extract::State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    let port = st.0.config.proxy.port;
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    // The internal hop crosses the proxy-port token gate (which requires a
    // Bearer header there): attach the configured token when present.
    let auth = format!("Bearer {}", st.0.config.token.as_deref().unwrap_or_default());
    let upstream = match st
        .0
        .client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", auth)
        .body(body)
        .send()
        .await
    {
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("proxy: {e}")).into_response(),
        Ok(r) => r,
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ct = upstream
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();
    let stream = upstream
        .bytes_stream()
        .map(|c| c.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string())));
    (
        status,
        [("content-type", ct)],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// The app icon as favicon: the multi-size `.ico` is embedded and served
/// at `/favicon.ico` (browsers request it automatically — no HTML change).
async fn favicon() -> Response {
    (
        [("content-type", "image/x-icon".to_string())],
        include_bytes!("../assets/llm_proxy.ico"),
    )
        .into_response()
}

/// Token gate for the stats port: when a non-empty `token` is configured,
/// every request must present it via the `Authorization: Bearer <token>`
/// header or the `?t=<token>` query. `/favicon.ico` stays open (no data).
/// `/health` is not served on this port (it lives on the proxy port).
async fn token_gate(token: Option<String>, req: Request, next: Next) -> Response {
    if req.uri().path() != "/favicon.ico" {
        if let Some(t) = &token {
            if !t.is_empty() {
                let ok_header = req.headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(|v| v == t)
                    .unwrap_or(false);
                let ok_query = req.uri().query()
                    .map(|q| q.split('&').any(|kv| kv == &format!("t={t}")))
                    .unwrap_or(false);
                if !ok_header && !ok_query {
                    return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
                }
            }
        }
    }
    next.run(req).await
}

pub async fn run_stats(
    state: Arc<AppState>,
    addr: String,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let token = state.config.token.clone();
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/stats", get(api_stats))
        .route("/api/chat", post(api_chat))
        .route("/api/admin/profile", post(api_admin_profile))
        .route("/api/admin/stop", post(api_admin_stop))
        .route("/favicon.ico", get(favicon))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn(move |req, next| {
            token_gate(token.clone(), req, next)
        }));

    info!(addr = %addr, "stats server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

pub async fn stats_json(state: &Arc<AppState>) -> Response {
    // Bridge mode: the dashboard is driven by the remote /api/stats feed
    // (polled in the background, app.rs). The remote instance reports its
    // own backend state, profile and system metrics. If the feed is not
    // available yet (remote down), serve a static skeleton so the page
    // still renders.
    if state.is_remote() {
        let cached = state.remote_stats.lock().unwrap().clone();
        let mut v = cached.unwrap_or_else(|| serde_json::json!({
            "started_ms": 0,
            "proxy": {
                "port": state.config.proxy.port,
                "stats_port": state.config.proxy.stats_port,
            },
            "profile": {
                "name": state.remote_addr(),
                "model_id": "—",
                "backend_host": state.remote_addr(),
                "backend_port": 0,
                "backend_exe": "remote",
                "context_length": null,
                "vision": null,
                "idle_unload_secs": null,
            },
            "backend": { "state": "offline", "pid": null, "metrics": [] },
            "system": null,
            "stats": { "requests": [], "errors": 0, "avg_response_ms": 0 },
        }));
        // The bridge's dashboard does not expose the remote's profile list:
        // admin actions on a bridge instance are 405 — switching happens from
        // the remote's own dashboard (local admin API) or the bridge's tray
        // (which polls the remote directly, app.rs).
        if let Some(o) = v.as_object_mut() {
            o.remove("profiles");
        }
        return (StatusCode::OK, [("content-type".to_string(), "application/json".to_string())], v.to_string()).into_response();
    }

    // Local mode: full feed from this proxy's own profile and system
    // monitor. Managed profiles expose the backend state machine
    // (stopped/loading/ready/unloading); external ones show "external".
    let profile = state.active_profile();

    // Optional backend metrics (only when the backend answers; never
    // blocks the dashboard). The backend may require the SAME token as the
    // proxy gate (--api-key <token>): present it when configured, like the
    // dashboard chat's internal hop does.
    let mut metrics: Vec<serde_json::Value> = Vec::new();
    let mut throughput: Option<(f64, f64)> = None;
    let url = format!("http://{}:{}/metrics", profile.backend_host(), profile.backend_port());
    let mut req = state
        .client
        .get(&url)
        .timeout(std::time::Duration::from_millis(1000));
    if let Some(t) = &state.config.token {
        if !t.is_empty() {
            req = req.header("authorization", format!("Bearer {}", t));
        }
    }
    if let Ok(resp) = req.send().await {
        if let Ok(text) = resp.text().await {
            let pairs = parse_metrics(&text);
            for (name, value) in &pairs {
                metrics.push(serde_json::json!({ "name": name, "value": value }));
            }
            throughput = throughput_window(state, &pairs);
        }
    }

    // Managed profile (declares an `exe`): the proxy owns the backend
    // lifecycle (state machine, pid, advertised capabilities from the
    // toml). External profile: no state machine — the backend answers
    // /props and /metrics itself when up; the toml values are the
    // fallback while it is down.
    let managed = !profile.exe.trim().is_empty();

    let state_name = if managed {
        match state.backend.state() {
            BackendState::Stopped => "stopped",
            BackendState::Loading => "loading",
            BackendState::Ready => "ready",
            BackendState::Unloading => "unloading",
        }
    } else {
        "external"
    };
    let backend_pid = if managed { state.backend.pid() } else { None };

    // Advertised capabilities (see `managed` above).
    let (ctx_advertised, vision_advertised) = if managed {
        (profile.context_length, profile.vision)
    } else {
        {
        let mut ctx: Option<u64> = None;
        let mut vision: Option<bool> = None;
        let url = format!(
            "http://{}:{}/props?autoload=false",
            profile.backend_host(),
            profile.backend_port()
        );
        if let Ok(resp) = state
            .client
            .get(&url)
            .timeout(std::time::Duration::from_millis(1000))
            .send()
            .await
        {
            if let Ok(text) = resp.text().await {
                if let Ok(j) = serde_json::from_str::<serde_json::Value>(&text) {
                    ctx = j.get("n_ctx").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)));
                    vision = j.pointer("/modalities/vision").and_then(|v| v.as_bool());
                }
            }
        }
        (ctx.or(profile.context_length), vision.unwrap_or(profile.vision))
        }
    };

    let backend_exe = if managed {
        exe_display(&profile.exe)
    } else {
        "—".to_string()
    };

    let sys = {
        let g = state.sysmon.lock().unwrap();
        crate::sysmon::SysSnapshot {
            cpu_pct: g.cpu_pct,
            cpu_name: g.cpu_name.clone(),
            ram: g.ram,
            gpus: g.gpus.clone(),
        }
    };

    // Profiles list (name = config key) + which one is active: consumed by
    // the remote-admin UI (the tray of a bridge pointing at this proxy).
    let active_name = state.active_name();
    let mut keys: Vec<&String> = state.config.profiles.keys().collect();
    keys.sort();
    let profiles = keys
        .into_iter()
        .map(|k| serde_json::json!({ "id": k, "name": k, "active": k == &active_name }))
        .collect::<Vec<_>>();
    // The model_id clients use: the configured one — the proxy translates
    // it to the backend's own id on the relay when they differ, so the
    // composite <profile>/<model_id> built from this stays routable.
    let model_id = profile.model_id();
    let v = serde_json::json!({
        "started_ms": state.started_ms,
        "router_mode": state.router,
        "proxy": {
            "port": state.config.proxy.port,
            "stats_port": state.config.proxy.stats_port,
        },
        "profile": {
            "name": active_name,
            "model_id": model_id,
            "backend_host": profile.backend_host(),
            "backend_port": profile.backend_port(),
            "backend_exe": backend_exe,
            "context_length": ctx_advertised,
            "vision": vision_advertised,
            "idle_unload_secs": profile.idle_unload_secs,
        },
        "backend": {
            "state": state_name,
            "pid": backend_pid,
            // Windowed tok/s from the cumulative counters (null until the
            // backend answers and exposes them — the dashboard falls back
            // to the raw gauges).
            "throughput": match throughput {
                Some((prefill, decode)) => serde_json::json!({
                    "prefill_tps": prefill,
                    "decode_tps": decode,
                    "window_secs": 5,
                }),
                None => serde_json::Value::Null,
            },
            "metrics": metrics,
        },
        "system": {
            "cpu_pct": sys.cpu_pct,
            "cpu_name": sys.cpu_name,
            "ram": sys.ram,
            "gpus": sys.gpus,
        },
        "stats": state.stats.snapshot(),
        "profiles": profiles,
    });
    (StatusCode::OK, [("content-type".to_string(), "application/json".to_string())], v.to_string()).into_response()
}

/// Display name of the backend executable from the config (base name,
/// `.exe` stripped): "C:\\backends\\llama-server.exe" -> "llama-server".
fn exe_display(exe: &str) -> String {
    let base = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    let lower = base.to_lowercase();
    match lower.strip_suffix(".exe") {
        Some(stem) => base[..stem.len()].to_string(),
        None => base.to_string(),
    }
}

/// Windowed throughput (tok/s, prefill/decode) from the backend's
/// CUMULATIVE token counters over the last 5 s. The raw `*_seconds`
/// gauges are not portable: llama.cpp's are the rate since the previous
/// poll and go to zero between turns, ninfer's keep the last batch rate —
/// the counters, in the identical format on both backends, make the
/// dashboard number backend-independent.
fn throughput_window(state: &Arc<AppState>, pairs: &[(String, f64)]) -> Option<(f64, f64)> {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
    let get = |name: &str| pairs.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
    let (Some(prompt), Some(predicted)) =
        (get("llamacpp:prompt_tokens_total"), get("llamacpp:tokens_predicted_total"))
    else {
        return None;
    };
    let now = std::time::Instant::now();
    let mut hist = state.metric_hist.lock().unwrap();
    // Drop samples older than the window (e.g. the dashboard was closed).
    while matches!(hist.front(), Some((t, _, _)) if now.duration_since(*t) > WINDOW) {
        hist.pop_front();
    }
    if let Some(&(t0, p0, d0)) = hist.front() {
        if prompt < p0 || predicted < d0 {
            // Counters restarted (backend restart): rebaseline.
            hist.clear();
        } else {
            let dt = now.duration_since(t0).as_secs_f64();
            if dt >= 0.5 {
                return Some(((prompt - p0) / dt, (predicted - d0) / dt));
            }
        }
    }
    hist.push_back((now, prompt, predicted));
    None
}

/// Minimal Prometheus text parser: `name{labels} value` pairs.
fn parse_metrics(text: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((nv, val)) = line.rsplit_once(' ') {
            if let Ok(v) = val.parse::<f64>() {
                let name = nv.split('{').next().unwrap_or(nv).to_string();
                out.push((name, v));
            }
        }
    }
    out
}
