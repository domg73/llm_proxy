# Changelog

## v1.0.6 — 2026-11-26

- **New: Telegram bot** — an optional `[telegram]` section (bot token from @BotFather) starts a long-polling Bot API client inside the proxy: no public port, no webhook, no new dependencies, just outbound HTTPS to api.telegram.org. Free text chats with the active model through the proxy's own `/v1/chat/completions` (loopback, like the dashboard chat — model routing, thinking level, web search, stats and bridge relay all work unchanged), with the per-chat history kept in the proxy's memory. Long answers are split at Telegram's 4096-char limit.
  - **Button menu** — `/menu` (or `/help`) and the Telegram **chat menu button** (registered via `setMyCommands`) open a keyboard: 📊 Status, 🧠 Think, 🔍 Search, 📂 Profile, 📋 Log, 🧹 Clear chat. Each sub-menu shows the **current state** and the options: Think lists the six thinking levels, Search is a toggle, Profile has one button per configured profile (✅ on the active one).
  - **Profile switching from the bot** — tapping a profile switches the active backend and the bot **waits for the new backend to be ready** (up to ~2 min) before confirming; in bridge mode the list and the switch go through the remote's admin API (`/api/admin/profile`), so a bridge's bot administers the server it points at.
  - **Commands** — `/status` (profile, model, backend state, requests, uptime), `/think [level]`, `/search [on|off]`, `/profile [id]`, `/log [n]` (backend log tail), `/clear`, `/id`, `/menu`, `/help`. The picker menus (`/think`, `/search`, `/profile` with no argument) always reply with the current state + the option buttons; every answer and command result carries the menu on inline buttons. **Attachments** — images sent to the bot go to the model as vision input, and text files are inlined into the prompt (same `--- attached file: name ---` convention as the dashboard chat).
  - **Typing indicator that doesn't die** — Telegram expires `typing` after 5 s; the bot re-sends it every ~4 s for the whole generation, and after 20 s a "⏳ still working…" note arrives, so a web search or a long generation never looks like a hang.
  - **`[telegram]` keys** — `bot_token` (required), `allow_user_ids` (the complete allowlist — a non-empty list admits exactly those ids, yours included; empty = the first user to write becomes the owner and the bot replies with their id, so you can pin it in the list), `system_prompt` (optional; unset or empty = no system message), `history` (messages kept per chat, default 20), `thinking` (default level for bot chats, `default` = no override), `web_search` (default **on**, per-chat toggle via the menu).
- **New: web search in the dashboard chat** — a globe button next to the thinking button (on by default) switches the model into search mode: the proxy injects a `web_search` tool into the upstream chat request and the model decides when (and whether) to search. Each query runs against Exa's free keyless MCP endpoint and the results go back to the model, which may search again (up to 4 rounds) before streaming the final answer. The search progress shows live above each answer — the query, then the clickable sources. One stats entry per chat message no matter how many search rounds ran; `stream: false` requests are handled as well. Works with any function-calling backend (tested with Qwen via llama.cpp). The on/off state is remembered between reloads (localStorage), like the thinking level. The same web search is available in the Telegram bot (default on there too, per-chat toggle).
- **New: `[web_search]` config section** — optional `exa_api_key` (sent as `x-api-key`) to lift the free-tier rate limits of the keyless endpoint; absent or empty means keyless. The feature itself is purely toggle-driven and needs no config.
- **Dashboard: the throughput KPI card is now a 2-page carousel** — the same `‹`/`›` pager as the GPU card switches between **backend** (the pinned windowed prefill/decode throughput) and **requests** (the latest request's prefill/decode tok/s, derived client-side: prompt tokens / TTFT, completion tokens / decode time — same semantics as the request-detail inspector).
- **Dashboard**: the chat attach button is now a paperclip icon, in line with the other icon buttons.

## v1.0.5 — 2026-11-23

- **Fix: the chat's `off` thinking level now works with llama.cpp backends** — llama.cpp's OpenAI-compatible server does not read the top-level `enable_thinking` (ninfer does), so `off` was silently ignored and the model kept thinking. `off` now also sends `reasoning_effort: "none"`, the standard way to disable reasoning on llama.cpp. ninfer and the other backends behave exactly as before — each backend applies only the field it understands.

## v1.0.4 — 2026-11-21

- **New: thinking-level control in the dashboard chat** — a lightbulb button next to the chat input opens a dropdown with the levels `model default`, `off`, `low`, `medium`, `high`, `xhigh`. The bulb is grayed on `off` and lit otherwise; the choice is remembered for the session. `off` sends `enable_thinking: false`, the levels send `reasoning_effort` with the matching value, `model default` sends no override — the request body is otherwise untouched.
- **Fix: the GPU pager buttons no longer appear on single-GPU systems** — the shared `.btn` rule overrode the `[hidden]` state, so the `‹`/`›` buttons showed even with one GPU; they are now hidden unless there are multiple GPUs. Their tooltips are in English.

## v1.0.3 — 2026-11-19

- **New: backend-log card in the dashboard** — a collapsible card in the backend panel streams the backend process's stdout/stderr live (2 s cursor-based polling; in bridge mode the endpoint is forwarded to the remote proxy). It auto-follows the tail until you scroll up, resumes when you return to the bottom, and auto-opens when a backend load fails — exactly when that output is needed. A "…" handle at the card's bottom center drag-resizes it (height remembered per panel); *Clear* empties the view.
- **New: `GET /api/backend-log`** — backend stdout/stderr is always captured into a rolling ring buffer (even in silent mode; console mode still prints to the terminal as before) and served through this endpoint.
- **Tray**: "Open log" renamed to "Open proxy log" to disambiguate from the new backend-log card.
- **Dashboard**: unified button sizing across all controls (the chat input row keeps its own style); the chat follows the tail while streaming and no longer pulls you back to the bottom if you scrolled up to read (a new turn always jumps to it); fixed the dashboard page bouncing/scrolling when log lines append (overflow-anchor).

## v1.0.2 — 2026-11-18

- **Tray**: new *Open log* menu entry — opens the log file (shown when a log file exists, e.g. in silent mode).
- **Dashboard**: the footer GitHub link now shows text + logo (theme-adaptive).

## v1.0.1 — 2026-11-11

- **`model_id` is now required and acts as an override**: it is the model id clients use — the proxy advertises the composite id `<profile>/<model_id>` in `/v1/models`, and the harness always addresses the model by it. Behind the scenes the proxy talks to the backend with the backend's *own* id: if the configured id is not in the backend's `/v1/models` list, the proxy resolves the real id (logged once, cached per profile for the process lifetime) and rewrites every request accordingly. A wrong `model_id` in the toml no longer breaks the proxy — it must just be non-empty.
- **Fix: `/v1/models` now advertises the composite id** `<profile>/<model_id>`. It used to expose the bare model id, which broke clients that use the advertised id verbatim (404 on chat).
- **Fix: dashboard chat always stays routable** — the status feed now reports the configured `model_id` (not the resolved one), so the built-in chat keeps working even after the proxy has learned the backend's real id.
- **Dashboard**: added GitHub link in the footer.
- Tests: the simulated backend is now strict about unknown model ids (like a real backend), the managed-backend suite exercises composite ids end-to-end, and new tests cover the id-override path and the missing-`model_id` config rejection.

## v1.0.0 — 2026-11-10

Initial release.

Local OpenAI-compatible LLM gateway for one or more inference backends: full backend lifecycle (wake on first request, idle unload, profile switching), OpenRouter-style multi-backend routing, live stats and chat dashboard. Single static binary, Windows + Linux.
