# Changelog

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
