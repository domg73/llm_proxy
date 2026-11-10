// llm_proxy: reverse proxy (default port 9931) that manages an LLM backend
// (llama-server or ninfer): lazy spawn on first request, keep warm,
// optional idle unload. Tray icon + always-on stats web server (web.rs).

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};
use tracing::{debug, info, warn};

use crate::backend::{BackendState, SpawnSpec};
use crate::config::Config;
use crate::config::Profile;
use crate::stats::Stats;

pub struct AppState {
    pub config: Config,
    /// Router mode (--router): the model is chosen by the client (the `model`
    /// field in the chat body / POST /models/load), never from the tray or
    /// the dashboard. There is no active-profile default — the proxy stays
    /// idle until the client's first load. The tray and dashboard hide their
    /// switch/load/stop controls so the client is the sole driver (no
    /// load/unload/load ping-pong between the two UIs).
    pub router: bool,
    /// Name of the ACTIVE profile (Windows: switchable at runtime from the
    /// tray; Linux local mode: fixed at startup). Guarded because the proxy
    /// handler (tokio, shared) and the tray thread both read it.
    pub active: std::sync::Mutex<String>,
    /// The managed backend (spawn/stop/state machine). Cross-platform:
    /// a profile that declares an `exe` is managed (spawn/stop/idle-unload);
    /// an external profile (no `exe`) never spawns — it sits in Stopped.
    pub backend: Arc<crate::backend::Backend>,
    pub client: reqwest::Client,
    pub stats: Arc<Stats>,
    /// Serializes backend state transitions (stop/spawn) so tray switches,
    /// lazy request loads, remote-admin commands and /models/load never
    /// double-spawn. The long model-load waits happen OUTSIDE this lock, so
    /// the dispatchers and new switches stay responsive while a model loads.
    pub ops: Arc<tokio::sync::Mutex<()>>,
    /// Epoch ms at which the proxy started (dashboard uptime).
    pub started_ms: u128,
    /// GPU/CPU/RAM snapshot published by the system monitor (sysmon).
    pub sysmon: Arc<std::sync::Mutex<crate::sysmon::SysSnapshot>>,
    /// Bridge mode: cached /api/stats JSON of the remote llm_proxy,
    /// published by the background poller (app.rs) and read by web.rs.
    pub remote_stats: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    /// Samples of the backend's CUMULATIVE token counters
    /// ((time, prompt_total, predicted_total)): web.rs reports the
    /// windowed throughput (delta over the last 5 s) to the dashboard —
    /// backend-independent, since the raw `*_seconds` gauges have
    /// different semantics (llama.cpp zeroes between turns, ninfer
    /// keeps the last batch rate).
    pub metric_hist: Arc<std::sync::Mutex<std::collections::VecDeque<(std::time::Instant, f64, f64)>>>,
    /// Windows: second sender of the action channel — the /api/admin
    /// endpoints hand their commands to the same serialized dispatcher as
    /// the local tray (single shared lifecycle, last command wins).
    #[cfg(windows)]
    pub admin_tx: std::sync::mpsc::Sender<crate::tray::Action>,
}

impl AppState {
    /// Bridge mode is active (config `remote`).
    pub fn is_remote(&self) -> bool {
        self.config.remote.is_some()
    }

    /// Base URL of the remote proxy (bridge mode only).
    pub fn remote_base(&self) -> String {
        format!("http://{}", self.config.remote.as_ref().expect("bridge mode"))
    }

    /// Remote address as configured, for display.
    pub fn remote_addr(&self) -> String {
        self.config
            .remote
            .clone()
            .unwrap_or_default()
    }
    /// Current active profile (cloned; cheap struct).
    pub fn active_profile(&self) -> Profile {
        let name = self.active.lock().unwrap().clone();
        self.config
            .profiles
            .get(&name)
            .cloned()
            .unwrap_or_else(|| self.config.profiles.values().next().cloned().expect("at least one profile"))
    }

    pub fn active_name(&self) -> String {
        self.active.lock().unwrap().clone()
    }
}

fn spawn_spec(profile: &Profile) -> SpawnSpec {
    SpawnSpec {
        exe: profile.exe.clone(),
        workdir: profile.workdir.clone(),
        args: profile.args.clone(),
    }
}

/// Claim the backend (short, under the ops lock): if it is cold, spawn
/// `profile`'s backend. Returns without spawning when one is already
/// running or loading. The long model-load wait happens OUTSIDE the lock,
/// by the caller, so a tray switch can preempt an in-flight load.
pub async fn ensure_spawn(state: &AppState, profile: &Profile) -> Result<()> {
    let _g = state.ops.lock().await;
    match state.backend.state() {
        BackendState::Ready | BackendState::Loading | BackendState::Unloading => Ok(()),
        BackendState::Stopped => {
            info!(model_id = %profile.model_id(), "spawning backend");
            state.backend.spawn(spawn_spec(profile)).await?;
            detach_ready(&state.backend, profile);
            Ok(())
        }
    }
}

/// Switch to the profile identified by `key`: kill whatever is running or
/// loading (the previous profile's backend), then spawn `profile`'s
/// backend. Returns false when a newer switch superseded this one (the old
/// backend is stopped and nothing is spawned — the newer switch owns it).
/// The guard compares KEYS (config keys), not display names: the active
/// slot holds the key, so a profile whose `name` differs from its key
/// would otherwise never spawn.
pub async fn switch_spawn(state: &AppState, key: &str, profile: &Profile) -> Result<bool> {
    let _g = state.ops.lock().await;
    if state.backend.state() != BackendState::Stopped {
        state.backend.stop().await;
    }
    if state.active_name() != key {
        info!(key = %key, "switch superseded by a newer one");
        return Ok(false);
    }
    info!(key = %key, model_id = %profile.model_id(), "spawning backend");
    state.backend.spawn(spawn_spec(profile)).await?;
    detach_ready(&state.backend, profile);
    Ok(true)
}

/// Detach the health wait after a spawn: the Loading→Ready flip must not
/// depend on the lifetime of any particular request task. If the client
/// disconnects, that request's own wait_ready() is dropped with the task
/// and the backend would sit in Loading forever, with nothing left to poll
/// its health. The detached task owns the flip (and the 300 s timeout /
/// dead-child handling); it bails cleanly when a newer spawn bumps the
/// epoch, so overlapping flows never fight over the state.
fn detach_ready(backend: &Arc<crate::backend::Backend>, profile: &Profile) {
    let b = backend.clone();
    let url = profile.health_url();
    tokio::spawn(async move {
        if let Err(e) = b.wait_ready(&url).await {
            debug!(err = %e, "detached readiness wait ended");
        }
    });
}
/// Switch the active profile from an external trigger (remote-admin:
/// POST /api/admin/profile on the stats port, web.rs — the bridge's tray
/// relays it for the user). Same path as the tray's LoadProfile action:
/// activate immediately (routing + tray marker), persist the choice, then
/// do the heavy work (stop + spawn + wait_ready) in the background so the
/// caller stays responsive while the model loads.
pub fn start_switch(state: &Arc<AppState>, name: String) {
    let cur = state.active_name();
    if name == cur {
        info!(%name, "switch requested (already active)");
        {
            // Background: never block the caller on a model load.
            let st = state.clone();
            tokio::spawn(async move {
                let profile = st.active_profile();
                // Readiness (Loading→Ready) is owned by the detached task
                // kicked off inside ensure_spawn — no request lifetime is
                // involved, so a client disconnect cannot strand the state.
                let _ = ensure_spawn(&st, &profile).await;
            });
        }
        return;
    }
    info!(from = %cur, to = %name, "switching active profile");
    // Activate immediately (routing + tray marker), then do the heavy work
    // (stop + spawn + wait_ready) in the background so the caller stays
    // responsive (further commands, exit) while the model loads.
    *state.active.lock().unwrap() = name.clone();
    #[cfg(windows)]
    crate::tray::set_active_profile(&name);
    // Persist the choice in the config file: on next start this profile is
    // the active one (surgical rewrite, comments kept). Failure is
    // non-fatal: the switch continues in memory.
    match state.config.persist_active_profile(&name) {
        Ok(()) => info!(name = %name, "active profile persisted to config file"),
        Err(e) => warn!(err = %e, name = %name, "could not persist active profile to config (switch continues in memory)"),
    }
    {
        let st = state.clone();
        // `key` is captured at the moment this switch was requested: if a
        // newer switch moved the active slot in the meantime, this task is
        // superseded (the guard inside switch_spawn compares keys).
        let key = name.clone();
        tokio::spawn(async move {
            let profile = match st.config.profiles.get(&key) {
                Some(p) => p.clone(),
                None => return, // removed from config meanwhile: nothing to do
            };
            match switch_spawn(&st, &key, &profile).await {
                // The detached readiness task (inside switch_spawn) performs
                // the Loading→Ready flip and logs "backend ready"; a client
                // disconnect here cannot strand the backend in Loading.
                Ok(true) => info!(name = %key, "profile switched, backend loading"),
                Ok(false) => {} // superseded by a newer switch
                Err(e) => warn!(err = %e, "spawn after switch failed"),
            }
        });
    }
}

/// Wait until the backend is Ready, spawning the active profile's backend
/// if it is cold. The long wait happens OUTSIDE the ops lock: a tray
/// switch can preempt an in-flight load (this wait then bails when its
/// child dies — the next request picks up the new profile).
async fn wait_ready_or_spawn(state: &AppState) -> Result<()> {
    loop {
        if state.backend.is_ready() {
            return Ok(());
        }
        match state.backend.state() {
            // A spawn/unload is in flight (possibly for a different profile
            // during a switch): poll until it settles.
            BackendState::Ready | BackendState::Loading | BackendState::Unloading => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            BackendState::Stopped => {}
        }
        let profile = state.active_profile();
        ensure_spawn(state, &profile).await?;
        // The detached readiness task (kicked off by ensure_spawn) owns the
        // Loading→Ready flip; here we only poll the state — so even if this
        // request's client disconnects (and this task is aborted), the
        // backend still reaches Ready instead of sitting in Loading.
        loop {
            match state.backend.state() {
                BackendState::Ready => return Ok(()),
                BackendState::Stopped => return Err(anyhow!("backend not ready")),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }
}

fn json_ok(v: serde_json::Value) -> Response {
    (StatusCode::OK, v.to_string()).into_response()
}

/// Bridge mode: single probe of the remote proxy's /health before relaying.
/// The remote answers /health instantly even while its model loads (its
/// own cold-start logic runs on that side), so one probe is enough — a
/// down remote fails fast with 502 instead of hanging the request.
async fn wait_remote_ready(state: &AppState) -> Result<()> {
    let url = format!("{}/health", state.remote_base());
    match state.client.get(&url).timeout(Duration::from_secs(3)).send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(anyhow!("remote {} unreachable ({}: {})", state.remote_base(), resp.status(), url)),
        Err(e) => Err(anyhow!("remote {} unreachable ({})", state.remote_base(), e)),
    }
}

/// Composite model id exposed by /v1/models: "<profile key>/<model_id>".
/// Clients (pi) select models by this id; the proxy routes the request to
/// the matching profile — switching the active one when needed — and the
/// bare id is what the backend itself reports and expects.
pub fn composite_id(key: &str, p: &Profile) -> String {
    format!("{}/{}", key, p.model_id())
}

/// Resolve a composite model id to (profile key, profile).
fn profile_by_composite(state: &Arc<AppState>, model: &str) -> Option<(String, Profile)> {
    state.config
        .profiles
        .iter()
        .find(|(k, p)| composite_id(k, p) == model)
        .map(|(k, p)| (k.clone(), p.clone()))
}

/// Model listing served by the proxy (cold and warm alike). Router: every
/// configured profile under its composite id. Non-router: only the active
/// profile, under its composite id (the single model this proxy exposes).
/// pi probes this with a 1000 ms budget,
/// then loads the model on demand (POST /models/load) and polls
/// /props?model= until it reports loaded.
fn all_models(state: &Arc<AppState>) -> serde_json::Value {
    let active = state.active_name();
    let bstate = state.backend.state();
    // Router: every profile under its composite id. Non-router: only the
    // active profile, under its composite id — the single model this proxy
    // exposes (the client always sees the proxy's id, never the backend's).
    let entries: Vec<(String, Profile, bool)> = if state.router {
        let mut keys: Vec<&String> = state.config.profiles.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|k| {
                (
                    composite_id(k, &state.config.profiles[k]),
                    state.config.profiles[k].clone(),
                    k == &active,
                )
            })
            .collect()
    } else {
        let p = state.active_profile();
        vec![(composite_id(&active, &p), p, true)]
    };
    let data = entries
        .into_iter()
        .map(|(id, p, is_active)| {
            let value = match (is_active, bstate) {
                (true, BackendState::Ready) => "loaded",
                (true, BackendState::Loading | BackendState::Unloading) => "loading",
                _ => "unloaded",
            };
            let input = if p.args.len() > 1 {
                vec![serde_json::Value::String(p.args[1].clone())]
            } else {
                vec![]
            };
            serde_json::json!({
                "id": id,
                "object": "model",
                "owned_by": "llm_proxy",
                "architecture": {
                    "input_formats": input,
                    "output_formats": [serde_json::Value::String("text".to_string())],
                    "family": "unknown"
                },
                "meta": { "n_ctx": p.context_length },
                "status": { "args": p.args, "value": value }
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "data": data })
}

/// `model=<value>` from a query string (e.g. `/props?model=...`).
fn query_model(query: &str) -> Option<String> {
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("model=").map(str::to_string))
}

/// Switch to a managed, non-active profile (same path as the tray) and wait
/// until any previously running backend is fully stopped. External profiles
/// are no-ops (the proxy never spawns them).
async fn switch_to(state: &Arc<AppState>, key: &str, target: &Profile) {
    if state.active_name() == key {
        return;
    }
    if !target.exe.trim().is_empty() {
        start_switch(state, key.to_string());
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while matches!(
            state.backend.state(),
            BackendState::Ready | BackendState::Unloading
        ) && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Chat completions model routing. Router: a composite id in the body's
/// `model` field selects the profile (switching the active one when needed)
/// and `model` is rewritten to the bare id the backend expects; anything
/// else passes through untouched. Non-router: the only accepted id is the
/// active profile's composite id — the bare id is rejected (404).
async fn route_chat(
    state: &Arc<AppState>,
    m: &str,
    path: &str,
    profile: Profile,
    req: Request<Body>,
) -> Result<(Profile, Request<Body>, Option<String>), (StatusCode, String)> {
    if m != "POST" || (path != "/chat/completions" && path != "/v1/chat/completions") {
        return Ok((profile, req, None));
    }
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let body = match axum::body::to_bytes(req.into_body(), 100 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return Err((StatusCode::BAD_REQUEST, e.to_string())),
    };
    let rebuild = |b: Vec<u8>| {
        let mut r = Request::new(Body::from(b));
        *r.method_mut() = axum::http::Method::POST;
        *r.uri_mut() = uri.clone();
        *r.headers_mut() = headers.clone();
        r
    };
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        // Not JSON — relay the untouched body to the active profile.
        return Ok((profile, rebuild(body.to_vec()), None));
    };
    let Some(model) = v.get("model").and_then(|x| x.as_str()).map(str::to_string) else {
        return Ok((
            profile,
            rebuild(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())),
            None,
        ));
    };
    // Router: a composite id selects the profile; anything else passes
    // through untouched to the active profile (the backend answers with its
    // own "model not found").
    if state.router {
        let Some((key, target)) = profile_by_composite(state, &model) else {
            return Ok((
                profile,
                rebuild(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())),
                None,
            ));
        };
        // Managed targets: switch (same path as the tray) and drain the old
        // backend so this request cannot hit it. External targets: dial
        // them directly, no lifecycle. `model` (the client's composite id)
        // is kept for the dashboard; the body carries the bare id.
        switch_to(state, &key, &target).await;
        v["model"] = serde_json::Value::String(target.model_id());
        return Ok((
            target,
            rebuild(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())),
            Some(model),
        ));
    }
    // Non-router: the proxy exposes exactly one model — the active profile's
    // composite id. The bare id was never advertised: reject it, so clients
    // keep using what /v1/models reports.
    let key = state.active_name();
    let target = state.active_profile();
    let advertised = composite_id(&key, &target);
    if model != advertised {
        return Err((
            StatusCode::NOT_FOUND,
            format!("model not found: '{}' (this proxy exposes '{}')", model, advertised),
        ));
    }
    v["model"] = serde_json::Value::String(target.model_id());
    Ok((
        target,
        rebuild(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())),
        Some(model),
    ))
}

/// Record an instant (proxy-level) request in the stats feed.
fn trace_cold(state: &Arc<AppState>, m: &str, path: &str, status: u16, ok: bool) {
    state.stats.request_started(m, path);
    state.stats.request_finished(m, path, status, ok, 0, 0, None,
        &crate::stats::ReqDetail::default());
}

pub async fn run_proxy(
    state: Arc<AppState>,
    addr: String,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    async fn forward(
        State(state): State<Arc<AppState>>,
        req: Request<Body>,
    ) -> Result<Response, (StatusCode, String)> {
        let path = req.uri().path().to_string();
        let mut query = req.uri().query().map(|q| format!("?{q}"));
        let m = req.method().as_str().to_string();

        // Token gate (proxy port): /health stays open; /api/stats also
        // accepts `?t=<token>` (it is a dashboard feed); every other path
        // requires the `Authorization: Bearer <token>` header.
        if let Some(t) = &state.config.token {
            if !t.is_empty() && path != "/health" {
                let ok_header = req.headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(|v| v == t)
                    .unwrap_or(false);
                let ok_query = path == "/api/stats"
                    && req.uri().query()
                        .map(|q| q.split('&').any(|kv| kv == &format!("t={t}")))
                        .unwrap_or(false);
                if !ok_header && !ok_query {
                    state.stats.request_started(&m, &path);
                    state.stats.request_finished(
                        &m, &path, 401, false, 0, 0,
                        Some("unauthorized (missing/invalid token)".to_string()), &Default::default(),
                    );
                    return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
                }
            }
        }

        // /health — always 200 in both modes: the proxy itself is the live
        // server. NOT recorded in stats: it is liveness noise (pi probes,
        // the bridge tray dot, fail-fast probe) and, once recorded, would
        // keep `generating` permanently true (the bridge polls the remote
        // once per second).
        if m == "GET" && path == "/health" {
            return Ok(json_ok(serde_json::json!({ "status": "ok" })));
        }

        // /api/stats — the dashboard feed, also on the proxy port: a bridge
        // instance mirrors the remote's copy (it only knows the remote's
        // proxy address, not its stats port). NOT recorded in stats for
        // the same reason: the bridge's 1 s mirror poller would keep
        // `generating` hot and flood recent requests.
        if m == "GET" && path == "/api/stats" {
            return Ok(crate::web::stats_json(&state).await);
        }

        // /api/admin/* — remote-admin commands, served on the proxy port
        // too: a bridge instance only knows the remote PROXY address (its
        // config `remote`), not the stats port where these routes also
        // live. Token-gated by the gate above; a bridge instance rejects
        // them (405 — no local profiles/backend to administer).
        if m == "POST" && (path == "/api/admin/profile" || path == "/api/admin/stop") {
            let body = axum::body::to_bytes(req.into_body(), 64 * 1024)
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            return match path.as_str() {
                "/api/admin/profile" => Ok(
                    crate::web::api_admin_profile(State(state.clone()), body).await,
                ),
                _ => Ok(crate::web::api_admin_stop(State(state.clone())).await),
            };
        }

        // ---------- Bridge mode: everything else is relayed transparently
        // to the remote proxy. The remote runs its own cold-start
        // endpoints (synthetic models, /models/load, /props) — this
        // instance only relays traffic and mirrors the remote /api/stats
        // onto the local dashboard.
        if state.is_remote() {
            state.stats.request_started(&m, &path);
            if let Err(e) = wait_remote_ready(&state).await {
                return Err((StatusCode::BAD_GATEWAY, e.to_string()));
            }
            let mut target = format!("{}{}", state.remote_base(), path);
            if let Some(q) = &query {
                target.push_str(q);
            }
            return relay(&state, &m, &path, target, req, None).await;
        }

        // Local backend mode. Managed profile (declares an `exe`): cold-start
        // endpoints, wake-on-traffic, idle unload — same on both platforms.
        // External profile (no `exe`): transparent pass-through, like bridge
        // mode but without the remote hop. Model routing: router mode routes
        // a composite `model` id to the matching profile (rewriting it to the
        // bare id before the relay); non-router mode accepts only the active
        // profile's composite id (bare id → 404).
        let (profile, req, client_model) =
            route_chat(&state, &m, &path, state.active_profile(), req).await?;
        // Router: a warm /props?model=<composite of the active profile> is
        // rewritten to the bare id (the backend never sees the composite).
        if m == "GET" && path == "/props" {
            if let Some(model) = query.as_deref().and_then(query_model) {
                if let Some((key, target)) = profile_by_composite(&state, &model) {
                    if key == state.active_name() {
                        query = Some(format!("?model={}", target.model_id()));
                    }
                }
            }
        }
        if !profile.exe.trim().is_empty() {
        let mut bstate = state.backend.state();

        // ---------- Proxy-level endpoints: instant, never block on the
        // backend (pi probes these with a 1000 ms startup budget).

        // /v1/models — always proxy-served: router → every profile under
        // its composite id; non-router → only the active profile, under its
        // composite id (the single model this proxy exposes).
        // Same fields pi expects from a backend.
        if m == "GET" && (path == "/v1/models" || path == "/models") {
            trace_cold(&state, &m, &path, 200, true);
            return Ok(json_ok(all_models(&state)));
        }

        // /props?model=<id> — model status. pi maps the error field in the
        // 200 body: code 503 → LOADING, 400 "model is not loaded" → UNLOADED.
        let model_query = query.as_deref().unwrap_or("").contains("model=");
        if m == "GET" && path == "/props" && model_query && bstate != BackendState::Ready {
            // Router: a composite id naming a non-active profile is not
            // loaded here — pi answers by POST /models/load (which switches).
            let not_active = query
                .as_deref()
                .and_then(query_model)
                .and_then(|model| profile_by_composite(&state, &model))
                .map(|(k, _)| k != state.active_name())
                .unwrap_or(false);
            let (code, msg) = if not_active {
                (400, "model is not loaded")
            } else {
                match bstate {
                    BackendState::Loading | BackendState::Unloading => (503, "model is loading"),
                    _ => (400, "model is not loaded"),
                }
            };
            trace_cold(&state, &m, &path, 200, true);
            return Ok(json_ok(serde_json::json!({
                "is_sleeping": false,
                "total_slots": 1,
                "n_ctx": profile.context_length.unwrap_or(0),
                "modalities": { "vision": profile.vision, "video": false, "audio": false },
                "error": { "code": code, "message": msg, "type": "server_error" }
            })));
        }

        // /props?autoload=false — server props, used only for mode detection.
        if m == "GET" && path == "/props" && !model_query && bstate != BackendState::Ready {
            trace_cold(&state, &m, &path, 200, true);
            return Ok(json_ok(serde_json::json!({ "backend": "llm_proxy" })));
        }

        // POST /models/load — on-demand spawn in the background, reply now;
        // pi polls /props?model= until it reports loaded. Router: a
        // composite model param naming a managed profile switches to it
        // first (same path as the tray); the background spawn re-reads the
        // active profile, so it targets the new one. Concurrent loads are
        // serialized by the ops lock (ensure_spawn no-ops when a spawn is
        // already in flight).
        if m == "POST" && path == "/models/load" {
            // pi-llama-cpp posts the model in the JSON body ({"model": "..."}),
            // not in the query string. Read it from the body; fall back to the query.
            let model_param: Option<String> = {
                let mut m = None;
                let body = axum::body::to_bytes(req.into_body(), 1024 * 1024).await
                    .map(|b| b.to_vec());
                if let Ok(b) = body {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                        m = v.get("model").and_then(|x| x.as_str()).map(str::to_string);
                    }
                }
                m.or_else(|| query.as_deref().and_then(query_model))
            };
            // Router only: a composite model param naming a managed profile
            // switches to it (same path as the tray). Non-router never
            // switches — it only loads the active profile.
            if let Some(model) = model_param {
                if state.router {
                    if let Some((key, target)) = profile_by_composite(&state, &model) {
                        switch_to(&state, &key, &target).await;
                    }
                }
            }
            // The switch above may have changed the backend state: re-read it.
            bstate = state.backend.state();
            if bstate != BackendState::Ready {
                let st = state.clone();
                tokio::spawn(async move {
                    let profile = st.active_profile();
                    // Readiness is owned by the detached task (ensure_spawn).
                    if let Err(e) = ensure_spawn(&st, &profile).await {
                        warn!(err = %e, "background model load failed");
                    }
                });
            }
            // Always reply for /models/load: the body is consumed above, so
            // we cannot fall through to the chat relay. pi polls /props until loaded.
            trace_cold(&state, &m, &path, 200, true);
            return Ok(json_ok(all_models(&state)));
        }

        // POST /models/unload — stop the backend in the background, reply now.
        // Router: a composite model param naming a non-active profile is a
        // no-op (nothing of it is loaded here).
        if m == "POST" && path == "/models/unload" {
            let other_profile = query
                .as_deref()
                .and_then(query_model)
                .and_then(|model| profile_by_composite(&state, &model))
                .map(|(k, _)| k != state.active_name())
                .unwrap_or(false);
            if other_profile || bstate != BackendState::Stopped {
                if !other_profile {
                    let be = state.backend.clone();
                    tokio::spawn(async move {
                        be.stop().await;
                    });
                }
                trace_cold(&state, &m, &path, 200, true);
                return Ok(json_ok(all_models(&state)));
            }
        }

        // GET /models/sse — the proxy has no SSE; 404 → pi falls back to polling.
        if m == "GET" && path == "/models/sse" && bstate != BackendState::Ready {
            trace_cold(&state, &m, &path, 404, false);
            return Ok((StatusCode::NOT_FOUND, "no sse support".to_string()).into_response());
        }

        let target = format!(
            "http://{}:{}{}{}",
            profile.backend_host(),
            profile.backend_port(),
            path,
            query.unwrap_or_default()
        );

        state.stats.request_started(&m, &path);
        state.backend.touch();

        // Backend required: cold → spawn, or wait for someone else's spawn.
        if let Err(e) = wait_ready_or_spawn(&state).await {
            state.stats.request_finished(
                m.as_str(),
                &path,
                503,
                false,
                0,
                0,
                Some(e.to_string()),
                &crate::stats::ReqDetail::default(),
            );
            return Err((StatusCode::SERVICE_UNAVAILABLE, e.to_string()));
        }

        relay(&state, &m, &path, target, req, client_model.as_deref()).await
        }
        else {
            let target = format!(
                "http://{}:{}{}{}",
                profile.backend_host(),
                profile.backend_port(),
                path,
                query.unwrap_or_default()
            );
            state.stats.request_started(&m, &path);
            relay(&state, &m, &path, target, req, None).await
        }
    }

    let app = axum::Router::new()
        .route("/{*path}", axum::routing::any(forward))
        .with_state(state);

    info!(addr = %addr, "proxy listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Resets the backend idle timer when a relayed request finishes (see the
/// watch loop in `backend.rs`).
struct IdleReset(std::sync::Arc<crate::backend::Backend>);
impl Drop for IdleReset {
    fn drop(&mut self) {
        self.0.touch();
    }
}

/// Relay `req` to `target` and pipe the response back, recording the
/// request stats (SSE: tokens/TTFT from `usage`; non-streaming: buffer).
/// Shared by local (backend) mode and bridge (remote) mode.
async fn relay(
    state: &Arc<AppState>,
    m: &str,
    path: &str,
    target: String,
    req: Request<Body>,
    client_model: Option<&str>,
) -> Result<Response, (StatusCode, String)> {
        let method = req.method().clone();
        let headers = req.headers().clone();
        let body_bytes = axum::body::to_bytes(req.into_body(), 100 * 1024 * 1024)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

        // LLM request details for the dashboard (best effort — non-JSON
        // bodies such as /health keep the defaults).
        let parsed = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();
        let detail = crate::stats::ReqDetail {
            model: client_model
                .map(str::to_string)
                .or_else(|| {
                    parsed
                        .as_ref()
                        .and_then(|v| v.get("model").and_then(|v| v.as_str()))
                        .map(str::to_string)
                }),
            stream: parsed
                .as_ref()
                .and_then(|v| v.get("stream").and_then(|s| s.as_bool())),
            body_bytes: body_bytes.len() as u64,
            ..Default::default()
        };

        // Drop guard: if the client disconnects before the response is
        // fully delivered, the handler or body stream is dropped and the
        // normal request_finished path never runs — the guard's Drop
        // clears the in-flight marker so `generating` cannot stay latched
        // (recorded as aborted, status 499).
        let guard = Arc::new(Mutex::new(Some(state.stats.sse_guard(m, path))));
        // Idle-unload timer: reset when the request lifecycle ENDS (any exit
        // path, client disconnect included) — a long generation must not
        // count toward the idle window, and idle time starts after the last
        // request finishes, not when it started.
        let _idle_reset = IdleReset(state.backend.clone());
        let complete_guard =
            |g: &Arc<Mutex<Option<crate::stats::ReqGuard>>>| {
                if let Some(mut x) = g.lock().unwrap().take() {
                    x.done = true;
                }
            };

        let mut fwd_headers = axum::http::HeaderMap::new();
        for (k, v) in headers.iter() {
            // host: we dial the target ourselves.
            // content-length / transfer-encoding: the body is re-sent as a
            // full buffer (it may have been re-serialized by the model
            // rewrite) — let the client compute the real framing.
            if matches!(
                k.as_str(),
                "host" | "content-length" | "transfer-encoding"
            ) {
                continue;
            }
            fwd_headers.append(k, v.clone());
        }
        // Bridge mode: present our (== the remote's) token to the remote
        // proxy, overriding whatever the local client sent.
        if state.is_remote() {
            if let Some(t) = &state.config.token {
                if !t.is_empty() {
                    fwd_headers.insert(
                        axum::http::header::AUTHORIZATION,
                        format!("Bearer {t}").parse().unwrap(),
                    );
                }
            }
        }
        let mut b = state.client
            .request(method, &target)
            .headers(fwd_headers);
        if !body_bytes.is_empty() {
            b = b.body(body_bytes);
        }

        let t0 = std::time::Instant::now();
        let resp = match b.send().await {
            Ok(r) => r,
            Err(e) => {
                complete_guard(&guard);
                state.stats.request_finished(
                    m, &path, StatusCode::BAD_GATEWAY.as_u16(), false, 0, 0,
                    Some(format!("backend request failed: {e}")), &detail,
                );
                return Err((StatusCode::BAD_GATEWAY, format!("backend request failed: {e}")));
            }
        };

        let status = resp.status();
        let status_u16 = status.as_u16();
        let resp_headers = resp.headers().clone();
        let is_sse = resp_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        // Stream response bytes through, counting usage tokens from the tail.
        let tokens_cell = Arc::new(AtomicU64::new(0));
        let prompt_cell = Arc::new(AtomicU64::new(u64::MAX));
        let completion_cell = Arc::new(AtomicU64::new(u64::MAX));
        let cached_cell = Arc::new(AtomicU64::new(u64::MAX));
        let ttft_cell = Arc::new(AtomicU64::new(0));
        let t0c = t0;
        let (tok_e, prompt_e, completion_e, cached_e, ttft_e) = (
            tokens_cell.clone(),
            prompt_cell.clone(),
            completion_cell.clone(),
            cached_cell.clone(),
            ttft_cell.clone(),
        );
        let ok = status.is_success();
        // Non-streaming response: small JSON body — buffer it fully and
        // record the request stats directly. A stream-completion hook
        // cannot be relied on here: hyper stops polling a Content-Length
        // body once the declared bytes are written, so it never reaches
        // the end (SSE bodies are chunked and always drain).
        if !is_sse {
            match resp.bytes().await {
                Ok(full) => {
                    let mut detail = detail;
                    if let Ok(text) = std::str::from_utf8(&full) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                            apply_usage(
                                v.get("usage"), &tokens_cell, &prompt_cell,
                                &completion_cell, &cached_cell,
                            );
                        }
                    }
                    let tokens = tokens_cell.load(Ordering::SeqCst);
                    let opt = |c: &AtomicU64| (c.load(Ordering::SeqCst) != u64::MAX).then(|| c.load(Ordering::SeqCst));
                    detail.prompt_tokens = opt(&prompt_cell);
                    detail.completion_tokens = opt(&completion_cell);
                    detail.cached_prompt_tokens = opt(&cached_cell);
                    let elapsed_ms = t0.elapsed().as_millis() as u64;
                    complete_guard(&guard);
                    state.stats.request_finished(
                        m, &path, status_u16, ok, tokens, elapsed_ms,
                        if ok { None } else { Some(format!("status {status_u16}")) },
                        &detail,
                    );
                    let mut builder = Response::builder().status(status);
                    for (k, v) in resp_headers.iter() {
                        builder = builder.header(k, v);
                    }
                    return builder
                        .body(axum::body::Body::from(full))
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                }
                Err(e) => {
                    complete_guard(&guard);
                    state.stats.request_finished(
                        m, &path, StatusCode::BAD_GATEWAY.as_u16(), false, 0,
                        t0.elapsed().as_millis() as u64,
                        Some(format!("reading backend response failed: {e}")), &detail,
                    );
                    return Err((StatusCode::BAD_GATEWAY, format!("reading backend response failed: {e}")));
                }
            }
        }
        let counting = resp
            .bytes_stream()
            .map(move |item: Result<bytes::Bytes, reqwest::Error>| {
                let Ok(chunk) = &item else { return item };
                if let Ok(text) = std::str::from_utf8(chunk) {
                    for line in text.lines() {
                        let line = line.trim();
                        let Some(payload) = line.strip_prefix("data:") else {
                            continue;
                        };
                        let Ok(v) =
                            serde_json::from_str::<serde_json::Value>(payload.trim())
                        else {
                            continue;
                        };
                        apply_usage(
                            v.get("usage"),
                            &tokens_cell,
                            &prompt_cell,
                            &completion_cell,
                            &cached_cell,
                        );
                        if has_content_delta(&v) {
                            let _ = ttft_cell.compare_exchange(
                                0,
                                t0c.elapsed().as_millis() as u64,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            );
                        }
                    }
                }
                item
            });
        let stats2 = state.stats.clone();
        let m_owned = m.to_string();
        let path_owned = path.to_string();
        let sse_guard_end = guard.clone();
        // Hand the client the server-side ttft/duration (the exact window
        // recent-requests uses for prefill/decode tok/s) as a trailing SSE
        // comment: clients that don't know it ignore comments (SSE spec).
        let t0s = t0;
        let ttfts = ttft_e.clone();
        let counting = counting.chain(futures::stream::once(Box::pin(async move {
            let tt = ttfts.load(Ordering::SeqCst);
            let dur = t0s.elapsed().as_millis() as u64;
            Ok(bytes::Bytes::from(format!(
                ": proxy-stats {{\"ttft_ms\":{tt},\"duration_ms\":{dur}}}\n\n"
            )))
        })));
        let counting = OnEnd::new(
            counting,
            move |last: Option<Result<bytes::Bytes, reqwest::Error>>| {
                complete_guard(&sse_guard_end);
                if let Some(Err(e)) = last {
                    tracing::warn!(err = %e, "stream interrupted");
                }
                let elapsed_ms = t0.elapsed().as_millis() as u64;
                let mut detail = detail;
                let tokens = tok_e.load(Ordering::SeqCst);
                let opt =
                    |c: &AtomicU64| (c.load(Ordering::SeqCst) != u64::MAX).then(|| c.load(Ordering::SeqCst));
                detail.prompt_tokens = opt(&prompt_e);
                detail.completion_tokens = opt(&completion_e);
                detail.cached_prompt_tokens = opt(&cached_e);
                let tt = ttft_e.load(Ordering::SeqCst);
                detail.ttft_ms = (tt > 0).then_some(tt);
                stats2.request_finished(
                    &m_owned, &path_owned, status_u16, ok, tokens, elapsed_ms,
                    if ok { None } else { Some(format!("status {status_u16}")) },
                    &detail,
                );
                if !ok {
                    warn!(status = %status, path = %path_owned, "request failed");
                }
            },
        );
        let body = Body::from_stream(counting);

        let mut builder = Response::builder().status(status);
        for (k, v) in resp_headers.iter() {
            builder = builder.header(k, v);
        }
        builder
            .body(body)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Store the `usage` object of an OpenAI-compatible response (SSE final
/// chunk or non-streaming body) into the counters: total tokens plus the
/// prompt/completion breakdown when present.
fn apply_usage(
    usage: Option<&serde_json::Value>,
    tokens: &Arc<AtomicU64>,
    prompt: &Arc<AtomicU64>,
    completion: &Arc<AtomicU64>,
    cached: &Arc<AtomicU64>,
) {
    let Some(u) = usage else { return };
    let p = u.get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(|x| x.as_u64());
    let c = u.get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(|x| x.as_u64());
    let total = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .or_else(|| p.zip(c).map(|(p, c)| p + c));
    if let Some(t) = total {
        tokens.store(t, Ordering::SeqCst);
    }
    if let Some(p) = p {
        prompt.store(p, Ordering::SeqCst);
    }
    if let Some(c) = c {
        completion.store(c, Ordering::SeqCst);
    }
    // Prompt tokens already served from a cached prefix (llama.cpp /
    // OpenAI-compatible `prompt_tokens_details.cached_tokens`): they cost
    // no prefill compute, so the derived prefill rate must exclude them.
    if let Some(k) = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
    {
        cached.store(k, Ordering::SeqCst);
    }
}

/// Does this stream chunk carry a visible content token (first one -> TTFT)?
fn has_content_delta(v: &serde_json::Value) -> bool {
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .map(|d| {
            ["content", "reasoning_content"]
                .iter()
                .any(|k| d.get(k).and_then(|c| c.as_str()).is_some_and(|s| !s.is_empty()))
        })
        .unwrap_or(false)
}

/// Stream wrapper: runs `on_end` exactly once when `inner` completes.
/// (futures `StreamExt` has no `finalize`; this is the minimal equivalent.)
struct OnEnd<S, F> {
    inner: S,
    on_end: Option<F>,
}

impl<S, F> OnEnd<S, F> {
    fn new(inner: S, on_end: F) -> Self {
        Self { inner, on_end: Some(on_end) }
    }
}

impl<S, F> Stream for OnEnd<S, F>
where
    S: Stream + Unpin,
    F: FnOnce(Option<S::Item>) + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(None) => {
                (this.on_end.take().unwrap())(None);
                Poll::Ready(None)
            }
            other => other,
        }
    }
}
