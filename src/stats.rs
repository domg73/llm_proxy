// Shared counters (requests, errors, tokens, load timing) for the proxy.
// The web stats server (see web.rs) reads snapshot() once per second.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One recorded (finished) request, newest first.
#[derive(Clone)]
pub struct ReqRecord {
    pub id: u64,
    /// Epoch milliseconds (client formats wall time).
    pub ts_ms: u128,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ok: bool,
    pub tokens: u64,
    /// Total wall-clock milliseconds for the request.
    pub ms: u64,
    pub error: Option<String>,
    /// `model` from the LLM request body (chat completions).
    pub model: Option<String>,
    /// `stream` from the LLM request body (None: non-JSON endpoint).
    pub stream: Option<bool>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// `usage.prompt_tokens_details.cached_tokens` (prefix cache hits).
    pub cached_prompt_tokens: Option<u64>,
    /// Milliseconds from request start to the first content token.
    pub ttft_ms: Option<u64>,
    /// Request body size, bytes.
    pub body_bytes: u64,
}

/// Captured LLM request details (all optional — non-JSON endpoints such
/// as /health keep the defaults).
#[derive(Default, Clone)]
pub struct ReqDetail {
    /// `model` from the request body (chat completions).
    pub model: Option<String>,
    /// `stream` from the request body.
    pub stream: Option<bool>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// `usage.prompt_tokens_details.cached_tokens` (prefix cache hits).
    pub cached_prompt_tokens: Option<u64>,
    /// Milliseconds from request start to the first content token.
    pub ttft_ms: Option<u64>,
    /// Request body size, bytes.
    pub body_bytes: u64,
}

const MAX_RECENT: usize = 100;

#[derive(Default)]
struct Inner {
    requests: u64,
    errors: u64,
    total_response_ms: u64,
    total_tokens: u64,
    total_decode_ms: u64,
    load_count: u64,
    load_total_ms: u64,
    last_load_ms: u64,
    req_start: Option<(Instant, u64, String, String)>,
    next_id: u64,
    /// Epoch ms of the last finished request (0 = never): the `generating`
    /// flag stays true for a short grace period after the stream ends, so
    /// the dashboard background does not snap off mid-fadeout.
    last_finish_ms: u128,
    recent: VecDeque<ReqRecord>,
}

pub struct Stats {
    inner: Mutex<Inner>,
}

/// RAII guard for an in-flight SSE request. If the response stream is
/// dropped before it drains (client disconnected mid-generation), the
/// guard's `Drop` clears the in-flight marker — otherwise `generating`
/// would stay latched true forever (the OnEnd hook never fires).
pub struct ReqGuard {
    stats: Arc<Stats>,
    method: String,
    path: String,
    pub done: bool,
}

impl Drop for ReqGuard {
    fn drop(&mut self) {
        if !self.done {
            self.stats.request_aborted(&self.method, &self.path);
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn request_started(&self, method: &str, path: &str) {
        let mut s = self.inner.lock().unwrap();
        s.requests += 1;
        s.next_id += 1;
        s.req_start = Some((
            Instant::now(),
            s.next_id,
            method.to_string(),
            path.to_string(),
        ));
    }

    /// True while a proxied request is in flight (set by `request_started`,
    /// cleared by `request_finished`/`request_aborted`).
    pub fn in_flight(&self) -> bool {
        self.inner.lock().unwrap().req_start.is_some()
    }

    /// Guard for an in-flight SSE request (see `ReqGuard`).
    pub fn sse_guard(self: &Arc<Self>, method: &str, path: &str) -> ReqGuard {
        ReqGuard {
            stats: self.clone(),
            method: method.to_string(),
            path: path.to_string(),
            done: false,
        }
    }

    /// Record an aborted stream (client closed the connection mid-
    /// generation): clears the in-flight marker and appends to the recent
    /// ring, but does NOT count a proxy error — the disconnect is
    /// client-side. Status 499 = client closed request (nginx convention).
    pub fn request_aborted(&self, method: &str, path: &str) {
        let mut s = self.inner.lock().unwrap();
        let (ms, id, m, p) = s
            .req_start
            .take()
            .map(|(t, id, m, p)| (t.elapsed().as_millis() as u64, id, m, p))
            .unwrap_or((0, s.next_id, method.to_string(), path.to_string()));
        s.total_response_ms += ms;
        s.last_finish_ms = now_ms();
        s.recent.push_front(ReqRecord {
            id,
            ts_ms: now_ms(),
            method: m,
            path: p,
            status: 499,
            ok: false,
            tokens: 0,
            ms,
            error: Some("aborted (client closed the connection)".to_string()),
            model: None,
            stream: None,
            prompt_tokens: None,
            completion_tokens: None,
            cached_prompt_tokens: None,
            ttft_ms: None,
            body_bytes: 0,
        });
        while s.recent.len() > MAX_RECENT {
            s.recent.pop_back();
        }
    }

    pub fn request_finished(
        &self,
        method: &str,
        path: &str,
        status: u16,
        ok: bool,
        tokens: u64,
        decode_ms: u64,
        error: Option<String>,
        detail: &ReqDetail,
    ) {
        let mut s = self.inner.lock().unwrap();
        let (ms, id, m, p) = s
            .req_start
            .take()
            .map(|(t, id, m, p)| (t.elapsed().as_millis() as u64, id, m, p))
            .unwrap_or((0, s.next_id, method.to_string(), path.to_string()));
        s.total_response_ms += ms;
        s.total_tokens += tokens;
        s.total_decode_ms += decode_ms;
        s.last_finish_ms = now_ms();
        if !ok {
            s.errors += 1;
        }
        s.recent.push_front(ReqRecord {
            id,
            ts_ms: now_ms(),
            method: m,
            path: p,
            status,
            ok,
            tokens,
            ms,
            error,
            model: detail.model.clone(),
            stream: detail.stream,
            prompt_tokens: detail.prompt_tokens,
            completion_tokens: detail.completion_tokens,
            cached_prompt_tokens: detail.cached_prompt_tokens,
            ttft_ms: detail.ttft_ms,
            body_bytes: detail.body_bytes,
        });
        while s.recent.len() > MAX_RECENT {
            s.recent.pop_back();
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record_error(&self) {
        self.inner.lock().unwrap().errors += 1;
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record_load_start(&self) {
        self.inner.lock().unwrap().load_count += 1;
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record_load_done(&self, secs: f64) {
        let mut s = self.inner.lock().unwrap();
        s.load_total_ms += (secs * 1000.0) as u64;
        s.last_load_ms = (secs * 1000.0) as u64;
    }

    /// JSON view of all counters + the recent request ring buffer.
    pub fn snapshot(&self) -> serde_json::Value {
        let s = self.inner.lock().unwrap();
        serde_json::json!({
            "requests": s.requests,
            "errors": s.errors,
            "avg_response_ms": if s.requests > 0 {
                s.total_response_ms / s.requests
            } else {
                0
            },
            "total_response_ms": s.total_response_ms,
            "total_tokens": s.total_tokens,
            "total_decode_ms": s.total_decode_ms,
            "load_count": s.load_count,
            "load_avg_ms": if s.load_count > 0 {
                s.load_total_ms / s.load_count
            } else {
                0
            },
            "last_load_ms": s.last_load_ms,
            // Ground truth for "inference in flight" (or just finished):
            // a request is open, or one closed less than 2 s ago.
            "generating": s.req_start.is_some()
                || (now_ms() - s.last_finish_ms) < 2000,
            "recent": s
                .recent
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "ts_ms": r.ts_ms,
                        "method": r.method,
                        "path": r.path,
                        "status": r.status,
                        "ok": r.ok,
                        "tokens": r.tokens,
                        "ms": r.ms,
                        "error": r.error,
                        "stream": r.stream,
                        "model": r.model,
                        "prompt_tokens": r.prompt_tokens,
                        "completion_tokens": r.completion_tokens,
                        "cached_prompt_tokens": r.cached_prompt_tokens,
                        "ttft_ms": r.ttft_ms,
                        "body_bytes": r.body_bytes,
                    })
                })
                .collect::<Vec<_>>(),
        })
    }
}

