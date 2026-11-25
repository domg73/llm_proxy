// Web search for the dashboard chat (alpha): Exa's free keyless MCP
// endpoint (the same one pi uses) — a single JSON-RPC `tools/call` to
// `web_search_exa`, answer parsed from the SSE `data:` lines. No key
// required; an optional `x-api-key` lifts the free-tier rate limits.

use anyhow::{anyhow, Result};
use serde_json::Value;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp?tools=web_search_exa";
const SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// The `web_search` tool advertised to the model (OpenAI function-calling
/// schema). The model decides WHEN to search; the proxy executes it.
pub const WEB_SEARCH_TOOL: &str = r#"{"type":"function","function":{"name":"web_search","description":"Search the web for current information (news, recent events, documentation, prices, anything newer than your training data). Returns the top results with titles, URLs and relevant excerpts. Call it for independent queries in parallel. Do not search for things you already know.","parameters":{"type":"object","properties":{"query":{"type":"string","description":"The search query — a few words, in any language."},"numResults":{"type":"number","description":"Number of results to return (default 5, max 10)."}},"required":["query"]}}}"#;

/// Run one Exa web search. `args` is the model's tool_call arguments
/// ({"query": ..., "numResults": ...}) — validated and clamped here.
/// Returns the formatted result text (what pi shows the user) ready to be
/// sent back as the tool result.
pub async fn exa_search(
    client: &reqwest::Client,
    api_key: &Option<String>,
    args: &Value,
) -> Result<String> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| anyhow!("web_search: missing 'query' argument"))?;
    let num_results = args
        .get("numResults")
        .and_then(|n| n.as_u64())
        .unwrap_or(5)
        .clamp(1, 10);

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search_exa",
            "arguments": { "query": query, "numResults": num_results }
        }
    });
    let body = serde_json::to_string(&payload).unwrap();
    let mut req = client
        .post(EXA_MCP_URL)
        .timeout(SEARCH_TIMEOUT)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("x-exa-source", "llm-proxy")
        .body(body);
    if let Some(k) = api_key {
        if !k.trim().is_empty() {
            req = req.header("x-api-key", k.trim());
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("web_search: request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow!("web_search: reading response failed: {e}"))?;
    if !status.is_success() {
        return Err(anyhow!("web_search: Exa returned {status}: {}", truncate(&body, 200)));
    }

    // The endpoint answers either plain JSON or SSE (`data: {...}` lines) —
    // accept both: collect every JSON payload and take the first that
    // carries a result or an error.
    let mut text = None;
    for candidate in extract_json_payloads(&body) {
        if let Some(t) = payload_text(&candidate) {
            text = Some(t);
            break;
        }
        if let Some(err) = payload_error(&candidate) {
            return Err(anyhow!("web_search: {err}"));
        }
    }
    text.ok_or_else(|| anyhow!("web_search: no result in Exa response: {}", truncate(&body, 200)))
}

/// Every JSON object in `body`: the whole body when it is plain JSON,
/// otherwise each SSE `data:` line that parses.
fn extract_json_payloads(body: &str) -> Vec<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return vec![v];
    }
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if let Some(payload) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(payload.trim()) {
                out.push(v);
            }
        }
    }
    out
}

/// `result.content[0].text` of a JSON-RPC tools/call result.
fn payload_text(v: &Value) -> Option<String> {
    v.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// The JSON-RPC `error.message`, if any.
fn payload_error(v: &Value) -> Option<String> {
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// Parse the (title, url) pairs out of Exa's formatted result text — the
/// dashboard shows them as the sources line under the answer. The format
/// is stable: each result block has `Title: ...` and `URL: ...` lines.
pub fn sources_from_text(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut title: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(t) = line.strip_prefix("Title:") {
            title = Some(t.trim().to_string());
        } else if let Some(u) = line.strip_prefix("URL:") {
            let url = u.trim().to_string();
            if !url.is_empty() {
                out.push((title.take().unwrap_or_default(), url));
            }
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}
