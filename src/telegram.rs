// Telegram bot for the dashboard: a long-poll client of the Bot API that
// exposes the proxy's chat (and status) in Telegram. Starts only when the
// config has a [telegram] section with a bot_token (absent = off).
//
// The bot is just another client of the SAME proxy: chat messages are POSTed
// to the proxy's own /v1/chat/completions on the loopback proxy port, like
// the dashboard's /api/chat — so model routing, the thinking level, web
// search, stats recording and bridge relay all work unchanged. No new
// dependency: reqwest (already in the tree) speaks the Bot API directly.
//
// Security: only allowlisted Telegram user IDs are served. Empty list →
// the FIRST user to write becomes the owner (the bot tells them their ID,
// to be saved in allow_user_ids).
//
// Limits accepted: Telegram has no real streaming — the bot shows a typing
// indicator, then the final answer (split at the 4096-char message limit).
// While an answer is being generated the bot is busy (single-user tool).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::TelegramConfig;
use crate::proxy::AppState;

const API: &str = "https://api.telegram.org";
/// Telegram's message limit is 4096 chars — keep a margin.
const MAX_MSG: usize = 4000;

pub async fn run(state: Arc<AppState>) {
    let cfg = state
        .config
        .telegram
        .clone()
        .expect("run() is only spawned when [telegram] is configured");
    let token = cfg.bot_token.clone();
    let mut offset: i64 = 0;
    let mut owner: Option<i64> = cfg.allow_user_ids.first().copied();
    let mut history: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut thinking = cfg.thinking.clone();
    let mut web_search = cfg.web_search;

    info!(owner = ?owner, "telegram bot: long polling started");

    // Register the chat menu (the ⋮ button next to the input field): the
    // client lists these commands; tapping one fills the input with the
    // command, the bot answers with the current state + options.
    if let Err(e) = tg_post(
        &state,
        &token,
        "setMyCommands",
        json!({"commands": [
            {"command": "menu", "description": "Show the menu"},
            {"command": "status", "description": "Backend, model, requests, uptime"},
            {"command": "profile", "description": "Show / switch the active profile"},
            {"command": "think", "description": "Set the thinking level"},
            {"command": "search", "description": "Toggle web search in chat"},
            {"command": "log", "description": "Last backend log lines"},
            {"command": "clear", "description": "Reset this conversation"},
            {"command": "id", "description": "Show your Telegram user ID"},
            {"command": "help", "description": "Command list"},
        ]}),
    )
    .await
    {
        warn!(err = %e, "telegram: setMyCommands failed");
    }

    loop {
        match fetch_updates(&state.client, &token, offset).await {
            Ok(updates) => {
                for u in updates {
                    if let Some(id) = u.get("update_id").and_then(|v| v.as_i64()) {
                        offset = id + 1;
                    }
                    handle_update(
                        &state,
                        &cfg,
                        &token,
                        &mut owner,
                        &mut history,
                        &mut thinking,
                        &mut web_search,
                        &u,
                    )
                    .await;
                }
            }
            Err(e) => {
                warn!(err = %e, "telegram: getUpdates failed — retrying in 5 s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn fetch_updates(
    client: &reqwest::Client,
    token: &str,
    offset: i64,
) -> Result<Vec<Value>> {
    let text = client
        .get(format!("{API}/bot{token}/getUpdates"))
        .query(&[
            ("offset", offset.to_string().as_str()),
            ("timeout", "30"),
            ("allowed_updates", "[\"message\",\"callback_query\"]"),
        ])
        .timeout(Duration::from_secs(70)) // > the 30 s server-side hold
        .send()
        .await?
        .text()
        .await?;
    let j: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("getUpdates: bad JSON from api.telegram.org ({e})"))?;
    if !j.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(anyhow!(
            "getUpdates: {}",
            j.get("description").and_then(|v| v.as_str()).unwrap_or("error")
        ));
    }
    Ok(j.get("result").and_then(|v| v.as_array()).cloned().unwrap_or_default())
}

async fn tg_post(state: &AppState, token: &str, method: &str, payload: Value) -> Result<Value> {
    let text = state
        .client
        .post(format!("{API}/bot{token}/{method}"))
        .header("content-type", "application/json")
        .body(payload.to_string())
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .text()
        .await?;
    let j: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("{method}: bad JSON from api.telegram.org ({e})"))?;
    if !j.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(anyhow!(
            "{method}: {}",
            j.get("description").and_then(|v| v.as_str()).unwrap_or("error")
        ));
    }
    Ok(j.get("result").cloned().unwrap_or(Value::Null))
}

/// Download a bot file (getFile → /file/<path>) into bytes.
async fn tg_download(state: &AppState, token: &str, file_id: &str) -> Result<Vec<u8>> {
    let file = tg_post(state, token, "getFile", json!({"file_id": file_id})).await?;
    let path = file
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("getFile: no file_path in the reply"))?;
    let resp = state
        .client
        .get(format!("{API}/file/bot{token}/{path}"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| anyhow!("file download failed: {e}"))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("file download failed: {e}"))?;
    use tracing::info;
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64)]).into_owned();
    info!(
        path,
        status = %status,
        size = bytes.len(),
        head,
        "telegram file download"
    );
    if !status.is_success() {
        return Err(anyhow!(
            "file download failed: HTTP {status} ({} bytes)",
            String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
        ));
    }
    Ok(bytes.to_vec())
}

/// data: URL for a model vision part (same format the dashboard chat sends).
fn data_url(mime: &str, bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Text-file name — documents are inlined as text for the model.
fn is_text_name(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str(),
        "txt" | "md" | "mdown" | "csv" | "tsv" | "json" | "log" | "rs" | "py" | "js"
            | "ts" | "toml" | "ini" | "cfg" | "conf" | "yml" | "yaml" | "xml" | "html"
            | "css" | "h" | "hpp" | "c" | "cpp" | "sh"
    )
}

async fn reply_plain(state: &AppState, token: &str, chat_id: i64, text: &str) {
    let text = trunc(text, MAX_MSG);
    let payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if let Err(e) = tg_post(state, token, "sendMessage", payload).await {
        warn!(err = %e, "telegram: sendMessage failed");
    }
}

/// Send with parse_mode HTML; on rejection (malformed HTML) fall back to
/// plain text with the tags stripped.
async fn reply_html(state: &AppState, token: &str, chat_id: i64, html: &str) {
    let payload = json!({
        "chat_id": chat_id,
        "text": trunc(html, MAX_MSG),
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });
    if let Err(e) = tg_post(state, token, "sendMessage", payload).await {
        warn!(err = %e, "telegram: HTML message rejected — retrying as plain text");
        reply_plain(state, token, chat_id, &strip_tags(html)).await;
    }
}

/// Send with parse_mode HTML and an inline keyboard (buttons). On
/// rejection (malformed HTML/markup) fall back to plain text, no buttons.
async fn reply_menu(state: &AppState, token: &str, chat_id: i64, html: &str, markup: &Value) {
    let payload = json!({
        "chat_id": chat_id,
        "text": trunc(html, MAX_MSG),
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
        "reply_markup": markup,
    });
    if let Err(e) = tg_post(state, token, "sendMessage", payload).await {
        warn!(err = %e, "telegram: menu message rejected — retrying as plain text");
        reply_plain(state, token, chat_id, &strip_tags(html)).await;
    }
}

/// Main inline menu — attached to command answers so nothing needs to be
/// typed: thinking level, web search, profile picker, status, log, clear.
fn main_menu(thinking: &str, web_search: &bool, profile: &str) -> Value {
    json!({"inline_keyboard": [
        [{"text": format!("🧠 Think: {thinking}"), "callback_data": "think:menu"},
         {"text": if *web_search {"🔍 Search: ON"} else {"🔍 Search: OFF"}, "callback_data": "search:toggle"}],
        [{"text": format!("📂 Profile: {}", trunc(profile, 28)), "callback_data": "profile:menu"}],
        [{"text": "📊 Status", "callback_data": "status:show"},
         {"text": "📋 Log", "callback_data": "log:show"}],
        [{"text": "🧹 Clear chat", "callback_data": "clear:do"}]
    ]})
}

fn think_menu() -> Value {
    let lvl = |l: &str| json!({"text": l, "callback_data": format!("think:{l}")});
    json!({"inline_keyboard": [
        [lvl("default"), lvl("off")],
        [lvl("low"), lvl("medium")],
        [lvl("high"), lvl("xhigh")],
        [{"text": "← back", "callback_data": "menu:show"}]
    ]})
}

/// Single toggle for web search (the button label says what it will do).
fn search_menu(on: &bool) -> Value {
    let label = if *on { "Turn web search OFF" } else { "Turn web search ON" };
    json!({"inline_keyboard": [
        [{"text": label, "callback_data": "search:toggle"}],
        [{"text": "← back", "callback_data": "menu:show"}]
    ]})
}

fn profile_menu(names: &[String], active: &str) -> Value {
    let rows: Vec<Value> = names
        .iter()
        .map(|n| {
            let mark = if n == active { "✅ " } else { "▶ " };
            json!([json!({"text": format!("{mark}{}", trunc(n, 30)), "callback_data": format!("profile:{n}")})])
        })
        .collect();
    json!({"inline_keyboard": rows})
}

/// Active profile name from the in-process stats snapshot ("—" if unknown).
async fn current_profile(state: &Arc<AppState>) -> String {
    let v = crate::web::stats_value(state).await;
    v.pointer("/profile/name")
        .and_then(|v| v.as_str())
        .unwrap_or("—")
        .to_string()
}

async fn typing(state: &AppState, token: &str, chat_id: i64) {
    let _ = tg_post(
        state,
        token,
        "sendChatAction",
        json!({"chat_id": chat_id, "action": "typing"}),
    )
    .await;
}

/// Keeps the "typing…" indicator alive for the whole answer: Telegram
/// expires the action after 5 s, so the spawned task re-sends it every
/// ~4 s until the guard is dropped (function return, any early exit).
/// After ~20 s it also posts one "still working" note so long web-search
/// / generation rounds don't look like a hang.
struct TypingGuard {
    stop: Arc<AtomicBool>,
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn typing_guard(state: &Arc<AppState>, token: &str, chat_id: i64) -> TypingGuard {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let st = state.clone();
        let tok = token.to_string();
        let s = stop.clone();
        tokio::spawn(async move {
            let mut cycles = 0;
            let mut note_sent = false;
            loop {
                typing(&st, &tok, chat_id).await;
                // ~4 s sleep, waking early once stopped
                for _ in 0..20 {
                    if s.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if s.load(Ordering::Relaxed) {
                    return;
                }
                cycles += 1;
                if cycles == 5 && !note_sent {
                    reply_plain(&st, &tok, chat_id, "⏳ still working… (web search / long generation can take a while)").await;
                    note_sent = true;
                }
            }
        });
    }
    TypingGuard { stop }
}

async fn handle_update(
    state: &Arc<AppState>,
    cfg: &TelegramConfig,
    token: &str,
    owner: &mut Option<i64>,
    history: &mut HashMap<i64, Vec<Value>>,
    thinking: &mut String,
    web_search: &mut bool,
    u: &Value,
) {
    if let Some(cb) = u.get("callback_query") {
        handle_callback(state, cfg, token, owner, history, thinking, web_search, cb).await;
        return;
    }
    let msg = match u.get("message") {
        Some(m) => m,
        None => return,
    };
    let chat_id = match msg.pointer("/chat/id").and_then(|v| v.as_i64()) {
        Some(i) => i,
        None => return,
    };
    let from_id = msg.pointer("/from/id").and_then(|v| v.as_i64()).unwrap_or(0);
    let text = msg
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");

    // /id works even for strangers: the not-authorized message below tells
    // them to send it, and it reveals nothing but their own ID.
    if text.eq_ignore_ascii_case("/id") {
        reply_plain(state, token, chat_id, &format!("Your Telegram user ID: {from_id}")).await;
        return;
    }

    // Authorization: allowlist; empty list → first writer becomes owner.
    let allowed = if !cfg.allow_user_ids.is_empty() {
        cfg.allow_user_ids.contains(&from_id)
    } else {
        match *owner {
            Some(o) => o == from_id,
            None => {
                *owner = Some(from_id);
                info!(
                    user_id = from_id,
                    "telegram: first user registered as owner — save the ID in allow_user_ids to make it explicit"
                );
                true
            }
        }
    };
    if !allowed {
        reply_plain(
            state,
            token,
            chat_id,
            "⛔ Not authorized. This bot is private (send /id and give your ID to the admin).",
        )
        .await;
        return;
    }

    // Attachments: the largest photo size, or a document — same semantics
    // as the dashboard chat (images as vision parts, text docs inlined).
    let caption = msg
        .get("caption")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let mut image: Option<String> = None; // data URL
    let mut doc: Option<(String, String)> = None; // (name, text)
    if let Some(arr) = msg.get("photo").and_then(|v| v.as_array()) {
        if let Some(p) = arr.last() {
            let file_id = p.get("file_id").and_then(|v| v.as_str()).unwrap_or("");
            match tg_download(state, token, file_id).await {
                Ok(bytes) if bytes.len() <= 15 * 1024 * 1024 => {
                    let mime = p
                        .get("mime_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/jpeg");
                    image = Some(data_url(mime, &bytes));
                }
                Ok(_) => {
                    reply_plain(state, token, chat_id, "Image too large (max 15 MB).").await;
                    return;
                }
                Err(e) => {
                    reply_plain(
                        state,
                        token,
                        chat_id,
                        &format!("Couldn't download the image: {e}"),
                    )
                    .await;
                    return;
                }
            }
        }
    } else if let Some(d) = msg.get("document") {
        let name = d.get("file_name").and_then(|v| v.as_str()).unwrap_or("file");
        let size = d.get("file_size").and_then(|v| v.as_u64()).unwrap_or(0);
        let file_id = d.get("file_id").and_then(|v| v.as_str()).unwrap_or("");
        match tg_download(state, token, file_id).await {
            Ok(bytes) => {
                let mime = d.get("mime_type").and_then(|v| v.as_str()).unwrap_or("");
                if mime.starts_with("image/") && size <= 15 * 1024 * 1024 {
                    image = Some(data_url(mime, &bytes));
                } else if (mime.starts_with("text/") || is_text_name(name))
                    && !bytes.contains(&0)
                    && size <= 300 * 1024
                {
                    doc = Some((
                        name.to_string(),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ));
                } else {
                    reply_plain(
                        state,
                        token,
                        chat_id,
                        "I can only read images and text documents (up to 300 KB) here.",
                    )
                    .await;
                    return;
                }
            }
            Err(e) => {
                reply_plain(
                    state,
                    token,
                    chat_id,
                    &format!("Couldn't download the file: {e}"),
                )
                .await;
                return;
            }
        }
    } else if msg.get("voice").is_some() || msg.get("audio").is_some() {
        reply_plain(
            state,
            token,
            chat_id,
            "Voice and audio aren't supported yet — send text, an image, or a document.",
        )
        .await;
        return;
    }

    if text.is_empty() && caption.is_empty() && image.is_none() && doc.is_none() {
        return;
    }

    if let Some(cmd) = text.strip_prefix('/') {
        handle_command(state, token, chat_id, from_id, cmd, history, thinking, web_search).await;
        return;
    }

    let mut msg_text = if !text.is_empty() { text.to_string() } else { caption.to_string() };
    if let Some((name, doc)) = &doc {
        let inline = format!("--- attached file: {name} ---\n{doc}");
        if msg_text.is_empty() {
            msg_text = inline;
        } else {
            msg_text.push_str("\n\n");
            msg_text.push_str(&inline);
        }
    }
    if msg_text.is_empty() {
        msg_text = "(see attachment)".to_string();
    }
    let content: Value = match &image {
        Some(url) => json!([
            {"type": "text", "text": msg_text},
            {"type": "image_url", "image_url": {"url": url}},
        ]),
        None => json!(msg_text),
    };
    do_chat(state, cfg, token, chat_id, &content, history, thinking, web_search).await;
}

const HELP: &str = "llm_proxy bot — most actions have buttons (under the messages and after /menu); typing still works:
/profile — pick the active profile
/menu — show the menu
/status — backend, model, requests
/log [n] — last n backend log lines
/search on|off — web search in chat
/think default|off|low|medium|high|xhigh
/clear — reset this conversation
/id — show your user ID
Any other message is sent to the model. Images and text documents (≤ 300 KB) are understood.";

async fn handle_command(
    state: &Arc<AppState>,
    token: &str,
    chat_id: i64,
    from_id: i64,
    cmd: &str,
    history: &mut HashMap<i64, Vec<Value>>,
    thinking: &mut String,
    web_search: &mut bool,
) {
    let (cmd, arg) = match cmd.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (cmd, ""),
    };
    let prof = current_profile(state).await;
    let menu = main_menu(thinking, web_search, &prof);
    match cmd {
        "help" | "start" => reply_plain(state, token, chat_id, HELP).await,
        "id" => reply_plain(state, token, chat_id, &format!("Your Telegram user ID: {from_id}")).await,
        "status" => status_cmd(state, token, chat_id, None).await,
        "log" => log_cmd(state, token, chat_id, arg, None).await,
        "clear" => {
            history.remove(&chat_id);
            reply_plain(state, token, chat_id, "Conversation cleared.").await;
        }
        "menu" => reply_menu(state, token, chat_id, "Menu:", &menu).await,
        "search" => {
            if arg.is_empty() {
                reply_menu(
                    state,
                    token,
                    chat_id,
                    &format!(
                        "Web search — current: <b>{}</b>. Tap to toggle:",
                        if *web_search { "ON" } else { "OFF" }
                    ),
                    &search_menu(web_search),
                )
                .await;
                return;
            }
            match arg {
                "on" => *web_search = true,
                "off" => *web_search = false,
                _ => {
                    reply_plain(state, token, chat_id, "Usage: /search on|off").await;
                    return;
                }
            }
        }
        "think" => {
            if arg.is_empty() {
                reply_menu(
                    state,
                    token,
                    chat_id,
                    &format!("Thinking level — current: <b>{thinking}</b>. Pick one:"),
                    &think_menu(),
                )
                .await;
                return;
            }
            if !matches!(arg, "default" | "off" | "low" | "medium" | "high" | "xhigh") {
                reply_plain(state, token, chat_id, "Usage: /think default|off|low|medium|high|xhigh").await;
                return;
            }
            *thinking = arg.to_string();
        }
        "profile" => {
            let names = profile_names(state).await;
            if names.is_empty() {
                reply_plain(state, token, chat_id, "No profiles available (config or remote unreachable?).").await;
                return;
            }
            reply_menu(
                state,
                token,
                chat_id,
                &format!("Profile — current: <b>{prof}</b>. Pick one:"),
                &profile_menu(&names, &prof),
            )
            .await;
            return;
        }
        _ => {
            reply_plain(state, token, chat_id, &format!("Unknown command /{cmd} — /help lists the commands.")).await;
            return;
        }
    }
    match cmd {
        "search" => {
            let lbl = if *web_search { "ON" } else { "OFF" };
            reply_plain(state, token, chat_id, &format!("Web search: {lbl}.")).await;
        }
        "think" => reply_plain(state, token, chat_id, &format!("Thinking level: {arg}.")).await,
        _ => {}
    }
}

/// Inline-button press (callback_query). Stops the ⋯ spinner, then acts on
/// the "area:arg" payload. Same authorization as plain messages.
async fn handle_callback(
    state: &Arc<AppState>,
    cfg: &TelegramConfig,
    token: &str,
    owner: &Option<i64>,
    history: &mut HashMap<i64, Vec<Value>>,
    thinking: &mut String,
    web_search: &mut bool,
    cb: &Value,
) {
    // Stop the spinner (fire and forget — cosmetic only).
    if let Some(id) = cb.get("callback_query_id").cloned() {
        let (s, t) = (state.clone(), token.to_string());
        tokio::spawn(async move {
            let _ =
                tg_post(&s, &t, "answerCallbackQuery", json!({"callback_query_id": id})).await;
        });
    }
    let Some(chat_id) = cb.pointer("/message/chat/id").and_then(|v| v.as_i64()) else {
        return;
    };
    let from_id = cb.pointer("/from/id").and_then(|v| v.as_i64()).unwrap_or(0);
    let allowed = if !cfg.allow_user_ids.is_empty() {
        cfg.allow_user_ids.contains(&from_id)
    } else {
        owner.is_some_and(|o| o == from_id)
    };
    if !allowed {
        return;
    }
    let (area, arg) = match cb
        .get("data")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .split_once(':')
    {
        Some(p) => p,
        None => return,
    };
    let prof = current_profile(state).await;
    let menu = main_menu(thinking, web_search, &prof);
    match area {
        "think" if arg == "menu" => {
            reply_menu(
                state,
                token,
                chat_id,
                &format!("Thinking level — current: <b>{thinking}</b>. Pick one:"),
                &think_menu(),
            )
            .await;
        }
        "think" if matches!(arg, "default" | "off" | "low" | "medium" | "high" | "xhigh") => {
            *thinking = arg.to_string();
            reply_plain(state, token, chat_id, &format!("Thinking level: {arg}.")).await;
        }
        "search" => {
            *web_search = !*web_search;
            let lbl = if *web_search { "ON" } else { "OFF" };
            reply_plain(state, token, chat_id, &format!("Web search: {lbl}.")).await;
        }
        "profile" if arg == "menu" => {
            let names = profile_names(state).await;
            if names.is_empty() {
                reply_plain(state, token, chat_id, "No profiles available (config or remote unreachable?).").await;
                return;
            }
            reply_menu(
                state,
                token,
                chat_id,
                &format!("Profile — current: <b>{prof}</b>. Pick one:"),
                &profile_menu(&names, &prof),
            )
            .await;
        }
        "profile" => {
            do_profile_switch(state, token, chat_id, arg).await;
        }
        "status" => status_cmd(state, token, chat_id, None).await,
        "log" => log_cmd(state, token, chat_id, "20", None).await,
        "clear" => {
            history.remove(&chat_id);
            reply_plain(state, token, chat_id, "Conversation cleared.").await;
        }
        "menu" => reply_menu(state, token, chat_id, "Menu:", &menu).await,
        _ => {}
    }
}

/// The switchable profiles. Local mode: this config. Bridge mode: the
/// remote's own stats feed (the bridge's config has no profiles by
/// design — its stats_value strips them), fetched directly like the tray.
async fn profile_names(state: &Arc<AppState>) -> Vec<String> {
    let v = if state.is_remote() {
        let url = format!("{}/api/stats", state.remote_base());
        let mut req = state.client.get(&url).timeout(Duration::from_secs(5));
        if let Some(t) = &state.config.token {
            if !t.is_empty() {
                req = req.header("authorization", format!("Bearer {t}"));
            }
        }
        match req.send().await { Ok(r) => match r.text().await { Ok(t) => serde_json::from_str::<Value>(&t).unwrap_or(Value::Null), Err(_) => Value::Null }, Err(_) => Value::Null }
    } else {
        crate::web::stats_value(state).await
    };
    let mut names: Vec<String> = v
        .pointer("/profiles")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| {
            p.get("name")
                .or_else(|| p.get("id"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// Switch the active profile: admin endpoint (local port, or the REMOTE
/// address in bridge mode — the bridge itself cannot switch, like the
/// tray), then poll the stats feed until the backend is ready (≈2 min cap)
/// and report the outcome.
async fn do_profile_switch(state: &Arc<AppState>, token: &str, chat_id: i64, name: &str) {
    let prof = current_profile(state).await;
    if name == prof {
        reply_plain(state, token, chat_id, &format!("Profile {name} is already active.")).await;
        return;
    }
    let base = if state.is_remote() {
        format!("http://{}", state.remote_addr())
    } else {
        format!("http://127.0.0.1:{}", state.config.proxy.port)
    };
    let mut req = state
        .client
        .post(format!("{base}/api/admin/profile"))
        .header("content-type", "application/json")
        .body(json!({ "profile": name }).to_string())
        .timeout(Duration::from_secs(10));
    if let Some(t) = &state.config.token {
        if !t.is_empty() {
            req = req.header("authorization", format!("Bearer {t}"));
        }
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            reply_plain(state, token, chat_id, &format!("⚠️ Switch failed: {e}")).await;
            return;
        }
    };
    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        reply_plain(state, token, chat_id, &format!("⚠️ Switch rejected: {}", trunc(&txt, 300)))
            .await;
        return;
    }
    reply_plain(state, token, chat_id, &format!("⏳ Switching to {name} — loading the model, this can take a while...")).await;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let v = crate::web::stats_value(state).await;
        let st = v
            .pointer("/backend/state")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let cur = v.pointer("/profile/name").and_then(|x| x.as_str()).unwrap_or("");
        if cur == name && st == "ready" {
            let model = v
                .pointer("/profile/model_id")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            reply_plain(state, token, chat_id, &format!("Profile {name} is active (model {model})."))
                .await;
            return;
        }
        if st == "error" || st == "stopped" {
            reply_plain(
                state,
                token,
                chat_id,
                &format!("Switch to {name} did not reach ready (backend: {st}). Check /log."),
            )
            .await;
            return;
        }
    }
    reply_plain(state, token, chat_id, &format!("{name} is still loading — check /status in a moment.")).await;
}

/// GET a proxy endpoint on the loopback proxy port (works in both local
/// and bridge mode — the bridge serves /api/* from the remote feed).
/// `path` must start with '/'.
async fn proxy_get(state: &AppState, path: &str) -> Option<Value> {
    let url = format!("http://127.0.0.1:{}{path}", state.config.proxy.port);
    let mut req = state.client.get(&url).timeout(Duration::from_secs(3));
    if let Some(t) = &state.config.token {
        if !t.is_empty() {
            req = req.header("authorization", format!("Bearer {t}"));
        }
    }
    let resp = req.send().await.ok()?;
    let text = resp.text().await.ok()?;
    serde_json::from_str(&text).ok()
}

async fn status_cmd(
    state: &Arc<AppState>,
    token: &str,
    chat_id: i64,
    menu: Option<&Value>,
) {
    // In-process: the same JSON the dashboard shows (bridge: cached remote
    // feed, local: full feed) — no HTTP round-trip, works in both modes.
    let v = crate::web::stats_value(state).await;
    let s = |p: &str| {
        v.pointer(p)
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let profile = s("/profile/name");
    let model = s("/profile/model_id");
    let bstate = s("/backend/state");
    let reqs = v.pointer("/stats/requests").and_then(|x| x.as_u64()).unwrap_or(0);
    let errs = v.pointer("/stats/errors").and_then(|x| x.as_u64()).unwrap_or(0);
    let started = v.get("started_ms").and_then(|x| x.as_u64()).unwrap_or(0);
    let uptime_ms = now_ms().saturating_sub(started);
    let h = uptime_ms / 3_600_000;
    let m = uptime_ms / 60_000 % 60;
    let mode = if state.is_remote() {
        format!("bridge → {}", state.remote_addr())
    } else {
        "local".to_string()
    };
    let out = format!(
        "📊 llm_proxy ({mode})\nprofile: {profile}\nmodel: {model}\nbackend: {bstate}\nrequests: {reqs} ({errs} errors)\nuptime: {h}h {m:02}m"
    );
    match menu {
        Some(m) => reply_menu(state, token, chat_id, &escape_html(&out), m).await,
        None => reply_plain(state, token, chat_id, &out).await,
    }
}

async fn log_cmd(
    state: &Arc<AppState>,
    token: &str,
    chat_id: i64,
    arg: &str,
    menu: Option<&Value>,
) {
    let n = arg.parse::<usize>().ok().filter(|n| (1..=100).contains(n)).unwrap_or(20);
    let Some(v) = proxy_get(state, "/api/backend-log?since=0").await else {
        reply_plain(state, token, chat_id, "⚠️ Backend log unavailable.").await;
        return;
    };
    let lines: Vec<String> = v
        .get("lines")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())
        .collect();
    if lines.is_empty() {
        reply_plain(state, token, chat_id, "Backend log: (empty).").await;
        return;
    }
    // Last n lines, keeping the tail within the message limit.
    let start = lines.len().saturating_sub(n);
    let mut kept: Vec<&String> = Vec::new();
    let mut len = 0;
    for l in lines.iter().rev().take(lines.len() - start) {
        len += l.len() + 1;
        if len > MAX_MSG {
            break;
        }
        kept.push(l);
    }
    kept.reverse();
    let mut joined = String::new();
    for (i, l) in kept.iter().enumerate() {
        if i > 0 {
            joined.push('\n');
        }
    joined.push_str(l);
    }
    match menu {
        Some(m) => reply_menu(state, token, chat_id, &escape_html(&joined), m).await,
        None => reply_plain(state, token, chat_id, &joined).await,
    }
}

/// Free text → chat completion through the proxy (loopback), with the
/// per-chat history kept here (the dashboard keeps its history in JS).
async fn do_chat(
    state: &Arc<AppState>,
    cfg: &TelegramConfig,
    token: &str,
    chat_id: i64,
    content: &Value,
    history: &mut HashMap<i64, Vec<Value>>,
    thinking: &mut String,
    web_search: &mut bool,
) {
    let _typing = typing_guard(state, token, chat_id);

    // Model id: the composite the dashboard uses (profile/model_id),
    // in-process (bridge: cached remote feed; local: full feed).
    let stats = crate::web::stats_value(state).await;
    let model_id = stats
        .pointer("/profile/model_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if model_id.is_empty() || model_id == "—" {
        reply_plain(
            state,
            token,
            chat_id,
            "⚠️ No active model yet (backend info not available). Try again in a moment.",
        )
        .await;
        return;
    }
    let profile = stats
        .pointer("/profile/name")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let model = format!("{profile}/{model_id}");

    let mut messages: Vec<Value> = Vec::new();
    if let Some(sp) = &cfg.system_prompt {
        if !sp.trim().is_empty() {
            messages.push(json!({"role": "system", "content": sp}));
        }
    }
    if let Some(h) = history.get(&chat_id) {
        let keep = cfg.history as usize;
        if h.len() > keep {
            messages.extend(h[h.len() - keep..].iter().cloned());
        } else {
            messages.extend(h.iter().cloned());
        }
    }
    messages.push(json!({"role": "user", "content": content.clone()}));

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if *web_search {
        body["web_search"] = json!(true);
    }
    match thinking.as_str() {
        "default" => {}
        "off" => {
            body["enable_thinking"] = json!(false);
            body["reasoning_effort"] = json!("none");
        }
        lvl => {
            body["reasoning_effort"] = json!(lvl);
        }
    }

    let url = format!("http://127.0.0.1:{}/v1/chat/completions", state.config.proxy.port);
    let mut req = state
        .client
        .post(&url)
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(t) = &state.config.token {
        if !t.is_empty() {
            req = req.header("authorization", format!("Bearer {t}"));
        }
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            reply_plain(state, token, chat_id, &format!("⚠️ Proxy error: {e}")).await;
            return;
        }
    };
    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        reply_plain(state, token, chat_id, &format!("⚠️ Error: {}", trunc(&txt, 500))).await;
        return;
    }

    // Consume the SSE stream, accumulating the answer + search events.
    let mut content = String::new();
    let mut searches: Vec<(String, Vec<String>)> = Vec::new(); // (query, "title — url")
    let mut usage_tokens: u64 = 0;
    let mut dur_ms: u64 = 0;
    let mut sse_event: Option<String> = None;
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(i) = buf.find('\n') {
            let line = buf[..i].trim().to_string();
            buf.drain(..=i);
            if line.is_empty() {
                sse_event = None;
                continue;
            }
            if let Some(e) = line.strip_prefix("event:") {
                sse_event = Some(e.trim().to_string());
                continue;
            }
            if let Some(d) = line.strip_prefix("data:") {
                let d = d.trim();
                if d == "[DONE]" {
                    sse_event = None;
                    continue;
                }
                let Ok(j) = serde_json::from_str::<Value>(d) else {
                    sse_event = None;
                    continue;
                };
                if sse_event.as_deref() == Some("websearch") {
                    sse_event = None;
                    match j.get("phase").and_then(|v| v.as_str()) {
                        Some("query") => searches.push((
                            j.get("query").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                            Vec::new(),
                        )),
                        Some("sources") => {
                            let q = j.get("query").and_then(|v| v.as_str());
                            let arr = j.get("sources").and_then(|v| v.as_array());
                            match (q, arr) {
                                (Some(q), Some(arr)) => {
                                    let srcs: Vec<String> = arr
                                        .iter()
                                        .map(|s| {
                                            let t = s.get("title").and_then(|t| t.as_str()).unwrap_or("");
                                            let u = s.get("url").and_then(|t| t.as_str()).unwrap_or("");
                                            format!("{t} — {u}")
                                        })
                                        .collect();
                                    if searches.last().map(|(lq, _)| lq == q).unwrap_or(false) {
                                        searches.last_mut().unwrap().1 = srcs;
                                    } else {
                                        searches.push((q.to_string(), srcs));
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                if let Some(c) = j.pointer("/usage/completion_tokens").and_then(|v| v.as_u64()) {
                    usage_tokens = c;
                }
                if let Some(delta) = j.pointer("/choices/0/delta/content").and_then(|v| v.as_str()) {
                    content.push_str(delta);
                }
                continue;
            }
            if let Some(ps) = line.strip_prefix(": proxy-stats ") {
                if let Ok(j) = serde_json::from_str::<Value>(ps.trim()) {
                    dur_ms = j.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
        }
    }

    // Record the turn in the per-chat history (trimmed to cfg.history).
    {
        let h = history.entry(chat_id).or_default();
        h.push(json!({"role": "user", "content": content.clone()}));
        h.push(json!({"role": "assistant", "content": content}));
        let keep = cfg.history as usize;
        if h.len() > keep {
            h.drain(..h.len() - keep);
        }
    }

    // Deliver: search note, answer (HTML when it fits, plain when chunked).
    if !searches.is_empty() {
        let mut note = String::from("🔎 Web search:");
        for (q, srcs) in &searches {
            note.push_str(&format!("\n• {q}"));
            for s in srcs.iter().take(5) {
                note.push_str(&format!("\n  {s}"));
            }
        }
        reply_plain(state, token, chat_id, &note).await;
    }
    if content.trim().is_empty() {
        reply_plain(state, token, chat_id, "(empty answer)").await;
    } else if content.len() <= MAX_MSG {
        reply_html(state, token, chat_id, &md_to_html(&content)).await;
    } else {
        // Longer than one Telegram message: plain text, chunked
        // (char-boundary safe).
        let mut start = 0;
        while start < content.len() {
            let mut end = (start + MAX_MSG).min(content.len());
            while end > start && !content.is_char_boundary(end) {
                end -= 1;
            }
            reply_plain(state, token, chat_id, &content[start..end]).await;
            start = end;
        }
    }
    let mut footer = String::new();
    if dur_ms > 0 {
        footer.push_str(&format!("⏱ {:.1} s", dur_ms as f64 / 1000.0));
        if usage_tokens > 0 {
            footer.push_str(&format!(" · {:.0} tok/s", usage_tokens as f64 / (dur_ms as f64 / 1000.0)));
        }
    }
    if !footer.is_empty() {
        reply_plain(state, token, chat_id, &footer).await;
    }
}

// --- Markdown → Telegram HTML (light) --------------------------------------

/// Escape + apply the few constructs models actually emit: ``` blocks →
/// <pre>, `inline` → <code>, **bold** → <b>. Deliberately minimal: Telegram
/// HTML must be well-formed or the whole message is rejected, so anything
/// exotic stays literal (and the plain-text fallback covers the rest).
fn md_to_html(s: &str) -> String {
    let parts: Vec<&str> = s.split("```").collect();
    let mut out = String::with_capacity(s.len() * 2);
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 {
            // Code block: drop a bare language-tag first line.
            let mut code = *part;
            if let Some((first, rest)) = code.split_once('\n') {
                if !first.trim().is_empty() && first.chars().count() <= 15 && !first.contains(' ') {
                    code = rest;
                }
            }
            out.push_str(&format!("<pre>{}</pre>", escape_html(code.trim_end_matches('\n'))));
        } else {
            out.push_str(&inline_md(part));
        }
    }
    out
}

fn inline_md(s: &str) -> String {
    let e = escape_html(s);
    // `code` spans first (their content is protected from the bold pass).
    let mut out = String::with_capacity(e.len());
    let mut rest = e.as_str();
    while let Some(i) = rest.find('`') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('`') {
            Some(j) => {
                out.push_str(&format!("<code>{}</code>", &after[..j]));
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    // **bold** (only well-paired; an unpaired ** stays literal).
    let mut final_out = String::with_capacity(out.len());
    let mut rest2 = out.as_str();
    while let Some(i) = rest2.find("**") {
        let after = &rest2[i + 2..];
        match after.find("**") {
            Some(j) => {
                final_out.push_str(&rest2[..i]);
                final_out.push_str(&format!("<b>{}</b>", &after[..j]));
                rest2 = &after[j + 2..];
            }
            None => break,
        }
    }
    final_out.push_str(rest2);
    final_out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Char-boundary-safe truncation.
fn trunc(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = n;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
