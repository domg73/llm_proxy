# Changelog

## v1.0.1 — 2026-11-11

- **`model_id` is now required and acts as an override**: it is the model id clients use — the proxy advertises the composite id `<profile>/<model_id>` in `/v1/models`, and the harness always addresses the model by it. Behind the scenes the proxy talks to the backend with the backend's *own* id: if the configured id is not in the backend's `/v1/models` list, the proxy resolves the real id (logged once, cached per profile for the process lifetime) and rewrites every request accordingly. A wrong `model_id` in the toml no longer breaks the proxy — it must just be non-empty.
- **Fix: `/v1/models` now advertises the composite id** `<profile>/<model_id>`. It used to expose the bare model id, which broke clients that use the advertised id verbatim (404 on chat).
- **Fix: dashboard chat always stays routable** — the status feed now reports the configured `model_id` (not the resolved one), so the built-in chat keeps working even after the proxy has learned the backend's real id.
- **Dashboard**: added GitHub link in the footer.
- Tests: the simulated backend is now strict about unknown model ids (like a real backend), the managed-backend suite exercises composite ids end-to-end, and new tests cover the id-override path and the missing-`model_id` config rejection.

## v1.0.0 — 2026-11-10

Initial release.

Local OpenAI-compatible LLM gateway for one or more inference backends: full backend lifecycle (wake on first request, idle unload, profile switching), OpenRouter-style multi-backend routing, live stats and chat dashboard. Single static binary, Windows + Linux.
