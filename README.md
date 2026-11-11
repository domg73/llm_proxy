# llm_proxy

Website: https://ai.domg73.cc/llm_proxy/

A local, **OpenAI-compatible LLM gateway** in front of one or more inference backends (ninfer, llama.cpp, or any OpenAI-compatible server). It stays invisible to any OpenAI client — point the client at it and it behaves like a normal endpoint — but behind the scenes it is the backends' supervisor: it owns each backend's whole lifecycle (wake on the first request, idle unload, runtime profile switching) and serves a live stats + chat dashboard.

By default the proxy fronts the **active profile**: the client sees exactly one backend (its composite id `<profile>/<model_id>`), and the profile is switched from the tray or the dashboard. With `--router` it becomes a multi-backend gateway: clients pick their backend and model with the `model` field (OpenRouter-style composite ids `<profile>/<model_id>`) and are the sole model driver — their `model` field and their load call are the only way the backend and model are selected.

Companion app: [llm_proxy configurator](https://github.com/domg73/llm_proxy_configurator) — a small Tauri desktop app to edit `llm_proxy.toml` (local and bridge modes, general settings, token generator, profiles, live TOML preview).

Two native targets:

- **Windows** — a single self-contained `.exe` with a tray icon (embedded app icon), backend management and runtime profile switching.
- **Linux** — a native headless binary (**experimental**: no tray, no local tray UI). Full backend management for **managed** profiles (spawn/stop/switch/wake/idle-unload exactly like Windows); profile management is driven from its **stats dashboard** (the profile dropdown in the backend card) or from a Windows bridge (tray) — or via the admin API. A profile with `exe`/`args` is managed by the proxy; a profile with only `backend_host`/`backend_port` is external (you start it) and the proxy only fronts it. See the [Linux](#linux) section for status details.

Both serve the identical web stats dashboard (request statistics, a chat card, CPU/RAM/GPU system cards, animated inference background).

## Features

- **Wake-on-traffic** — the first `/v1` request starts the backend; the answer is streamed as soon as the backend's health check passes. The client never sees a "starting" error.
- **Idle unload** — optionally stop the backend after N seconds without traffic (`idle_unload_secs`), freeing VRAM/RAM.
- **Backend profiles** — declare several backend configurations (any OpenAI-compatible backend; ninfer and llama.cpp are both tested) and switch between them **at runtime** — from the Windows tray, or remotely from a bridge (see [Remote administration](#remote-administration)).
- **Multi-profile routing** (`--router`) — `/v1/models` lists **every** configured profile as an OpenRouter-style composite id `<profile>/<model_id>`; any OpenAI client routes to a profile simply by setting the standard `model` field (e.g. pi's `/model`). A bare (non-composite) `model_id` passes through untouched to the active profile. Requests to a non-active managed profile trigger the usual switch (stop + spawn + wait), and the backend itself never sees the composite — the proxy rewrites the `model` field before forwarding.
- **Router mode** (`--router`) — the client is the sole model driver. The proxy starts idle and loads whichever model the client's `model` field / `POST /v1/models/load` selects; the tray and dashboard hide their switch/load/stop controls, so the client is the only thing that selects the model. Without `--router` the proxy behaves as a single backend: `/v1/models` lists only the active profile (its composite id `<profile>/<model_id>` — the single id the proxy exposes), chat traffic is relayed to it (any other id → `404`), and `/models/load` loads only the active profile — no composite routing, no profile switching from the client.
- **Stats dashboard** — request KPIs with per-request details, a collapsible **chat** card (same-origin `POST /api/chat` through the proxy, word-by-word SSE streaming, Stop/New controls, file attachments with client-side image scaling, Markdown rendering, copyable code blocks, duration + tok/s meta line), a **system** section (CPU card — model name, semicircle CPU%/RAM gauges — side by side with the GPU card: name, clocks, power, semicircle gauges for GPU/VRAM/temperature on a green→yellow→red ramp; multi-GPU cards cycle with `‹`/`›` and a VRAM pool row; GPU data from `nvidia-smi`), server info, a pinned **throughput** box (prefill/decode tok/s computed by the proxy from the backend's cumulative token counters — identical semantics on every backend, keeps the last non-zero value between turns), and the raw Prometheus-style backend metrics when exposed: llama.cpp serves `/metrics` only with the `--metrics` flag, ninfer always does. The page has an animated background — a neural lattice that lights up while an inference is running — disable with `--no-animated-bg`.
- **Tray icon** (Windows) — green = backend ready, red = not active. Double-click opens the dashboard; right-click shows the menu: load a profile, stop the backend, open the dashboard, restart the proxy, exit.
- **Bridge mode** — one llm_proxy relays every request to another llm_proxy (the one that runs the backend) and mirrors the remote's `/api/stats` feed on its local dashboard. Fail-fast: remote down → immediate `502`.
- **Remote administration** — switch the active profile or stop the backend of a server from a bridge (Windows tray menu) or via its API. See [Remote administration](#remote-administration).
- **Linux (headless)** — same backend lifecycle as Windows. A profile is **managed** (`exe`/`args`: the proxy spawns/stops it, the default in the shipped config) or **external** (`backend_host`/`backend_port`: you run it, e.g. under systemd); any machine on the network can use it as an OpenAI endpoint, and other llm_proxy instances can connect to it as a bridge.

## Requirements

- Windows 10/11, or Linux (glibc, x86_64) for the headless target.
- A local OpenAI-compatible inference backend and its model files (tested: `ninfer-kvmem-server`, llama.cpp). On the Linux target the backend is managed by the proxy (managed profile) or runs externally (external profile — you start it, e.g. under systemd).
- Optional: `nvidia-smi` on the `PATH` — enables the GPU card (without it the dashboard shows `n/a` for the GPU).
- To build from source: Rust toolchain + Visual Studio 2022 BuildTools with the Windows SDK (for `rc.exe`).

## Quick start (Windows)

1. Put `llm_proxy.exe` and `llm_proxy.toml` in the same folder.
2. Run the exe. A console opens with the runtime log and a tray icon appears. For auto-start / no console: `llm_proxy.exe --silent` — the log then goes to `%LOCALAPPDATA%\llm_proxy\llm_proxy.log`.
3. Point your OpenAI client at the proxy:

   ```
   base_url  http://127.0.0.1:9931/v1
   ```

4. Open the dashboard: `http://127.0.0.1:9935`.

The backend is **not** started until the first request arrives; the first request blocks until the backend's health check passes, then is served normally.

## Command-line options

| Option | Meaning |
|---|---|
| `-h`, `--help` | List the available options and exit (both platforms). |
| `llm_proxy.toml` (positional) | Path to the config file. Without it the proxy searches next to the exe, then in the CWD. |
| `--silent` | No console; all output goes to the log file. For auto-start. |
| `--no-animated-bg` | Serve the dashboard without the animated background. |
| `--remote host:port` | Bridge mode: no local backend; relay to another llm_proxy. Same as the `remote` config key. |
| `--dev` | Dev only: dashboard served from `src/web/dashboard.html` on disk (re-read per request) — iterate with a browser refresh. |
| `--router` | Router mode: the client is the sole model driver. No profile is loaded at startup (no active-profile default); the model is chosen only by the client's `model` field / `POST /v1/models/load`. The tray and dashboard hide their switch/load/stop controls. Without it, the proxy fronts only the active profile (single backend). |

Log level is always INFO.

## Configuration (`llm_proxy.toml`)

No defaults: without the config file the proxy exits. Search order:

1. Path given as first command-line argument.
2. `llm_proxy.toml` next to the executable.
3. `llm_proxy.toml` in the current working directory.

### Global

| Key | Meaning |
|---|---|
| `active_profile` | Profile started at boot (if any). Tray switching rewrites this key, so the last choice survives a restart. |
| `token` | Optional shared secret. When set (non-empty), both servers enforce it: the dashboard accepts `Authorization: Bearer <t>` **or** `?t=<t>`, every other route accepts only the header. Missing/wrong → `401`; `GET /health` stays open. In local mode `token = ''` auto-generates a 256-bit secret on first start, rewrites it into the toml and prints it once. In bridge mode `token = ''` is rejected: the bridge must carry the **remote's** token (it is sent on every forwarded request). |
| `bind` | Listen address for proxy + stats servers (default `127.0.0.1`). `0.0.0.0` exposes both on the LAN — only safe together with a token. With `bind = '0.0.0.0'` the self-referencing URLs (tray “Open stats”, console print, tooltip) use `127.0.0.1`, since `0.0.0.0` is not dialable. |
| `remote` | Optional. `'host:port'` of another llm_proxy → bridge mode (same as `--remote host:port`). Mutually exclusive with `active_profile`/`profiles`. |

### `[proxy]`

| Key | Meaning |
|---|---|
| `port` | Proxy (OpenAI API) port. |
| `stats_port` | Dashboard/stats port. |

### Profiles

Each `[profiles.<id>]` describes one backend:

| Key | Meaning |
|---|---|
| `exe` | Backend executable. Declared in a **managed** profile (the proxy spawns it; both targets). Omit it for an external backend (e.g. Linux profile with only `backend_host`/`backend_port`). |
| `workdir` | Backend working directory (relative model paths resolve here). |
| `args` | CLI arguments in order (typically: model path, `--port`, …). The backend address is derived from `--host`/`--port` when `backend_host`/`backend_port` are not set. |
| `model_id` | **Required.** The id clients see: it forms the composite id `<profile>/<model_id>` that `/v1/models` advertises (in `--router` mode for every profile, otherwise for the active one only), and the harness always addresses the model by it. If the backend reports a different id in its own `/v1/models`, the proxy translates to the backend's id before relaying — clients never notice. |
| `context_length` | Advertised context window (served in `/v1/models` while cold). Keep in sync with the backend's `--max-context`. |
| `vision` | Model is vision-capable (advertised input modalities). Keep in sync with the backend's `--vision` flag. |
| `health` | Health-check URL (default: `http://<backend_host>:<backend_port>/health`). |
| `idle_unload_secs` | Stop the backend after N seconds without traffic. |
| `backend_host` / `backend_port` | Explicit backend address. Required for an **external** profile (no `exe`/`args` — the proxy only fronts a backend you run); optional in a managed profile (override of the `--host`/`--port` derivation). |

Example — two profiles (ninfer and llama.cpp), one active:

```toml
# llm_proxy.toml — two profiles, one active
active_profile = 'ninfer'
# token = ''          # auto-generated on first start (local mode)
# bind = '0.0.0.0'

[proxy]
port = 9931
stats_port = 9935

# context_length and vision are served in /v1/models while the backend is
# cold — keep them in sync with the backend's --max-context / --vision flags.
[profiles.ninfer]
exe = 'C:\ninfer\ninfer-server.exe'
workdir = 'C:\ninfer'
model_id = 'qwen3.8-27b'
context_length = 32768
vision = true
args = [
  'C:\ninfer\models\Qwen3.8-27B-IQ3_S.ninfer',
  '--max-context', '32768',
  '--host', '127.0.0.1',
  '--port', '8080',
  '--vision',
]
idle_unload_secs = 1800

[profiles.llamacpp]
exe = 'C:\llama\llama-server.exe'
workdir = 'C:\llama'
model_id = 'gemma-4-26b'
context_length = 32768
vision = true
args = [
  '--model', 'C:\llama\models\gemma-4-26b-a4b-it-QAT-Q4_0.gguf',
  '--mmproj', 'C:\llama\models\mmproj-bf16.gguf',
  '--ctx-size', '32768',
  '--host', '127.0.0.1',
  '--port', '8080',
  '--metrics',            # required for the dashboard's backend metrics
]
```

## Windows

- **Console mode** (no flags): a native console shows the runtime log — useful while setting up.
- **`--silent`**: no console; log file at `%LOCALAPPDATA%\llm_proxy\llm_proxy.log`. The tray icon still appears (green/red backend state).
- **Tray menu**: *Load profile …* switches the active profile at runtime (the switch persists in the toml), *Stop backend* unloads it, *Open stats* opens the dashboard, *Restart proxy* relaunches the process so a config file modified while it was running is re-read (the backend restarts: the first request after is cold), *Exit* stops the proxy.

## Linux

**Status: experimental.** The backend lifecycle is at parity with Windows, but the target is headless: **no tray, no console mode, no
local tray UI**. Profile management (switch / stop) is available from the **stats dashboard** — the profile dropdown + *stop* button in the
backend card — or from a **Windows bridge** (its tray mirrors the server's profiles: *Remote profile …*, *Stop backend*), or by calling
the server's `POST /api/admin/profile` / `POST /api/admin/stop` endpoints directly. Everything else (OpenAI proxy, wake-on-traffic,
idle unload, managed/external profiles, dashboard, bridge mode) works the same as on Windows.

Headless: no tray, but the same backend lifecycle as Windows. A profile **with `exe`/`args` is managed** (spawn, stop, switch, wake-on-traffic, idle unload — the proxy kills the whole process group); a profile **with only `backend_host`/`backend_port` is external** (you start it, e.g. via systemd, and the proxy only fronts it):

```toml
# the same two-profile config as above, with Linux paths and executables
active_profile = 'ninfer'
# token = ''          # auto-generated on first start (local mode)
# bind = '0.0.0.0'

[proxy]
port = 9931
stats_port = 9935

[profiles.ninfer]
exe = '/opt/ninfer/ninfer-server'
workdir = '/opt/ninfer'
model_id = 'qwen3.8-27b'
context_length = 32768
vision = true
args = [
  '/opt/ninfer/models/Qwen3.8-27B-IQ3_S.ninfer',
  '--max-context', '32768',
  '--host', '127.0.0.1',
  '--port', '8080',
  '--vision',
]
idle_unload_secs = 1800

[profiles.llamacpp]
exe = '/opt/llama/llama-server'
workdir = '/opt/llama'
model_id = 'gemma-4-26b'
context_length = 32768
vision = true
args = [
  '--model', '/opt/llama/models/gemma-4-26b-a4b-it-QAT-Q4_0.gguf',
  '--mmproj', '/opt/llama/models/mmproj-bf16.gguf',
  '--ctx-size', '32768',
  '--host', '127.0.0.1',
  '--port', '8080',
  '--metrics',            # required for the dashboard's backend metrics
]

# external profile: a backend you run yourself (e.g. systemd) — the proxy
# only fronts it (no exe/args, no spawn/stop)
# [profiles.external]
# model_id = 'external-model'
# context_length = 32768
# vision = true
# backend_host = '127.0.0.1'
# backend_port = 8080
```

Managed profiles need no systemd: the proxy is the backend's supervisor (backend `idle_unload_secs` applies too). External profiles still work as before:

```ini
# /etc/systemd/system/ninfer.service
[Unit]
Description=ninfer LLM backend
After=network.target

[Service]
WorkingDirectory=/opt/ninfer
ExecStart=/opt/ninfer/ninfer-kvmem-server /opt/ninfer/models/Qwen3.8-27B-IQ3_S-vision-mtp-dflash2-q3.ninfer --max-context 262144 --host 127.0.0.1 --port 8080 --vision
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

then run the proxy (e.g. `nohup ./llm_proxy llm_proxy.toml &` or its own unit). The dashboard reports the backend state live (from the backend's `/metrics`/`/props`), and any machine on the network can use the proxy as an OpenAI endpoint — including other llm_proxy instances in bridge mode.

## Bridge mode

A bridge instance has no local backend: `remote = 'host:port'` (or `--remote host:port`) points it at another llm_proxy. Every request is transparently forwarded (with the remote's `token` on every request), and the local dashboard mirrors the remote's `/api/stats` feed — GPU/CPU/RAM, recent requests, generating state, active profile.

LAN recipe:

1. On the **server** (where the backend runs): `bind = '0.0.0.0'` and a non-empty `token`.
2. On the **bridge**: this config, with the same `token`:

   ```toml
   remote = '192.168.1.50:9931'   # host:port of the server
   token = '<the server token>'

   [proxy]
   port = 9940
   stats_port = 9941
   ```

3. Point the client at the bridge: `http://<bridge>:9940/v1`.

The server may be a Windows instance or a headless Linux one (managed or external profile); the bridge is typically Windows (its tray shows the remote's state). Fail-fast: server down → immediate `502`, no hanging requests.

**When the client can reach the server directly, the bridge is optional.** Its remaining value: (1) a **stable local endpoint** for the client — the server address lives in the bridge config, not the client's; (2) **network segmentation** — bridge on loopback only, server in a less exposed segment, the bridge (with its token) is the only gateway; (3) clients that **cannot reach the server** directly; (4) the tray as a quick UI; (5) **at-a-glance monitoring** — the tray dot is the remote server's liveness (green = up, red = down) without opening the dashboard.

## Remote administration

A server exposes two admin endpoints (same token gate as every other route; `401` without the token). They are reachable on **both** the stats port and the proxy port — the proxy port is what a bridge uses, because its config `remote` only carries that address:

| Endpoint | Request | Effect |
|---|---|---|
| `POST /api/admin/profile` | `{"profile": "<id>"}` | Switch the active profile at runtime (cold-starts the new backend; persists `active_profile` in the server's toml). `404` for unknown ids. |
| `POST /api/admin/stop` | — | Stop the backend. `406` only for **external** profiles (backend not managed by the proxy); managed profiles (with `exe`) get `202` on both targets. |

From a **Windows bridge** there is no need to call them by hand: the tray menu lists the **remote's** profiles (mirrored from the server's `/api/stats`), with `●` on the active one — clicking one switches the remote server exactly like the local tray does, and *Stop backend* stops the remote's backend. While the remote is switching, the bridge's tray dot goes red until the new backend is ready. The same two endpoints are also wired to the **stats dashboard** (profile dropdown + *stop* button in the backend card, token from the page URL) — that is the local UI on both targets.

Server side:

- **Windows server** — admin commands are serialized on the same channel as the local tray actions: one switch at a time, last command wins (even with several bridges pointed at the same server).
- **Linux server** — managed profiles: same full lifecycle as Windows (stop A, spawn B, wait ready, persist). External profiles: the switch only re-points the proxy.

## Security

- Never expose a server on the LAN without a `token`.
- With a token, every route except `GET /health` requires `Authorization: Bearer <token>` (the dashboard feed additionally accepts `?t=<token>` so the browser can open it directly).
- **Client side (harness):** point the client at the proxy and set its **API key to the proxy's token** — OpenAI-compatible clients send `Authorization: Bearer <api_key>` on every request, which is exactly what the gate expects. No other client-side config is needed.
- **Optional backend key:** ninfer and llama.cpp can both require `--api-key <t>` on the backend itself. If you use it, the value **must equal the proxy's `token`** — the proxy forwards the client's `Authorization` header to the backend and its dashboard `/metrics` polling presents the proxy token; a mismatch means silent `401`s from the backend (chat and backend metrics die). The backend's `GET /health` stays unauthenticated, so proxy readiness checks keep working.
- The bridge forwards the remote's token on every request; keep the bridge itself on loopback or behind the same token.

## Building from source

- **Windows**: `build_windows.ps1` (Rust toolchain + VS 2022 BuildTools).
- **Linux**: `build_linux.sh` (or `cargo build --release`; native, glibc x86_64).

The result is a single static binary: no runtime dependencies.

---

## Copyright

© 2026 Domenico Giuffrida (domg73). MIT license.

## Support

llm_proxy is free (MIT). If it has been useful to you, a coffee is
welcome — never required:

[![Donate via PayPal](assets/support_button.png)](https://paypal.me/domg73)
