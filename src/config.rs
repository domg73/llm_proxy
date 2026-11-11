use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub port: u16,
    /// Port of the always-on stats/web server (serves the dashboard).
    /// Must differ from the proxy port and from every backend_port.
    pub stats_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Model ID exposed to clients (required): it forms the composite id
    /// `<profile>/<model_id>` that the proxy advertises in /v1/models and
    /// the harness always addresses the model by it. If the backend
    /// reports a different id in its own /v1/models, the proxy translates
    /// to it before relaying — clients never notice.
    #[serde(default)]
    pub model_id: String,
/// Path to the backend executable (llama-server.exe or ninfer*.exe).
/// Optional when the backend is NOT spawned by the proxy (Linux local
/// mode: the backend runs externally, only its address matters).
    #[serde(default)]
    pub exe: String,
    /// Working directory for the backend process (relative model paths resolve here).
    #[serde(default)]
    pub workdir: Option<String>,
    /// CLI arguments, in order. First positional arg is typically the model
    /// path. The backend address the proxy dials is derived from here: `--host`
    /// (0.0.0.0/::/absent loopback to 127.0.0.1) and `--port` (absent: 8080,
    /// the llama.cpp default) — declared once, here, never duplicated.
    #[serde(default)]
    pub args: Vec<String>,
    /// Explicit backend host. Wins over the `--host` derivation from `args`;
    /// required in practice for profiles whose backend is not spawned by
    /// the proxy (Linux local mode, no args to derive from).
    #[serde(default)]
    pub backend_host: Option<String>,
    /// Explicit backend port. Wins over the `--port` derivation from `args`.
    #[serde(default)]
    pub backend_port: Option<u16>,
    /// Optional explicit health URL; defaults to http://<backend host>:<backend port>/health
    #[serde(default)]
    pub health: Option<String>,
    /// If > 0, unload backend after this many seconds of inactivity.
    #[serde(default)]
    pub idle_unload_secs: u64,
    /// Context length for the synthetic model entry (max_model_len / n_ctx).
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Vision-capable model (synthetic architecture.input_modalities).
    #[serde(default)]
    pub vision: bool,
}

impl Profile {
    pub fn model_id(&self) -> String {
        self.model_id.clone()
    }

    /// Value of `--name`/`--name value` in `args` (first occurrence, both forms).
    fn arg_value(&self, name: &str) -> Option<String> {
        let mut i = 0;
        while i < self.args.len() {
            let a = &self.args[i];
            if let Some(v) = a.strip_prefix(&format!("{}=", name)) {
                return if v.is_empty() { None } else { Some(v.to_string()) };
            }
            if a == name && i + 1 < self.args.len() {
                return Some(self.args[i + 1].clone());
            }
            i += 1;
        }
        None
    }

    /// Host the proxy dials the backend on: from `--host` in `args`.
    /// 0.0.0.0/::/absent loopback to 127.0.0.1 (a bind on a specific IP does
    /// not accept loopback connections, hence the passthrough).
    pub fn backend_host(&self) -> String {
        if let Some(h) = &self.backend_host {
            return h.clone();
        }
        match self.arg_value("--host").as_deref() {
            Some("0.0.0.0") | Some("::") | None => "127.0.0.1".to_string(),
            Some(h) => h.to_string(),
        }
    }

    /// Port the backend listens on: from `--port` in `args`.
    /// Absent: 8080 — the llama.cpp-family default, i.e. what the backend
    /// itself would use without the flag.
    pub fn backend_port(&self) -> u16 {
        if let Some(p) = self.backend_port {
            return p;
        }
        self.arg_value("--port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080)
    }

    pub fn health_url(&self) -> String {
        self.health
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}/health", self.backend_host(), self.backend_port()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub proxy: ProxyConfig,
    /// Bridge mode: address ("host:port") of the remote llm_proxy. This
    /// instance has no local backend: all proxy traffic is forwarded
    /// there (the remote runs its own cold-start logic) and the local
    /// dashboard is driven by the remote's /api/stats feed. Mutually
    /// exclusive with `active_profile`/`profiles`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Name of the profile to use for on-demand spawn (local mode only).
    #[serde(default)]
    pub active_profile: Option<String>,
    /// Shared secret. When set (non-empty), every request must present it:
    /// header `Authorization: Bearer <token>` on the proxy port, header or
    /// query `?t=<token>` on the stats port (`/health` stays open). In bridge
    /// mode it is sent to the remote proxy on every forwarded request — it
    /// must match the remote's token. `token = ""` means "generate one on
    /// first start" (local mode only: the file is rewritten and the value
    /// printed once — copy it into the bridge configs).
    #[serde(default)]
    pub token: Option<String>,
    /// Bind host for proxy and stats servers (default `127.0.0.1`).
    /// `0.0.0.0` exposes both on the LAN — meaningful only together with a
    /// token (the bridge on another machine needs to reach the proxy port).
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(skip)]
    pub path: PathBuf,
}

impl Config {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(raw)?;
        if cfg.proxy.stats_port == cfg.proxy.port {
            bail!("stats_port must differ from proxy.port (both {})", cfg.proxy.port);
        }
        if let Some(b) = &cfg.bind {
            if b.trim().is_empty() {
                bail!("bind: empty value (use \"127.0.0.1\" or \"0.0.0.0\")");
            }
        }
        if let Some(remote) = &cfg.remote {
            // Bridge mode: no local profiles.
            if cfg.active_profile.is_some() || !cfg.profiles.is_empty() {
                bail!(
                    "remote mode: remove `active_profile` and the `profiles` sections — this instance only forwards to {remote}"
                );
            }
            // The bridge token must be the remote's: auto-generation here
            // would create a secret the remote does not know.
            if cfg.token.as_deref() == Some("") {
                bail!(
                    "remote mode: `token = \"\"` is not allowed — copy the remote machine's token into this config"
                );
            }
            let (host, port) = remote.rsplit_once(':').ok_or_else(|| {
                anyhow!("remote must be \"host:port\" (got {remote})")
            })?;
            if host.is_empty() {
                bail!("remote: empty host ({remote})");
            }
            let port: u16 = port.parse()
                .map_err(|_| anyhow!("remote: invalid port ({remote})"))?;
            if port == 0 {
                bail!("remote: invalid port (0)");
            }
            cfg.remote = Some(format!("{host}:{port}"));
            return Ok(cfg);
        }
        let active = cfg.active_profile.as_deref().ok_or_else(|| {
            anyhow!("missing `active_profile` (bridge mode uses `remote` instead)")
        })?;
        if !cfg.profiles.contains_key(active) {
            bail!(
                "active_profile '{}' not found in profiles ({})",
                active,
                cfg.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        for (name, p) in &cfg.profiles {
            // Clients address the model by this id, so it must be set:
            // the proxy advertises it and translates it to the backend's
            // own id (if different) before relaying.
            if p.model_id.trim().is_empty() {
                bail!("profile '{}': model_id is empty (required — clients address the model by it)", name);
            }
            if p.backend_port() == cfg.proxy.stats_port {
                bail!("stats_port {} collides with backend port of profile '{}'", cfg.proxy.stats_port, name);
            }
            // Windows: the proxy spawns the backend, so the executable is
            // required. Linux local mode monitors an external backend —
            // only its address matters (backend_host/backend_port).
            #[cfg(windows)]
            if p.exe.trim().is_empty() {
                bail!("profile '{}': exe is empty (required to spawn the backend)", name);
            }
        }
        Ok(cfg)
    }

    pub fn active_profile(&self) -> &Profile {
        let name = self
            .active_profile
            .as_deref()
            .expect("active_profile is set in local mode (validated in parse)");
        self.profiles
            .get(name)
            .expect("active_profile exists (validated in parse)")
    }

    /// Rewrite `token = "<value>"` in the source file (surgical, comments
    /// kept, atomic write) — used after auto-generation on first start.
    pub fn persist_token(&self, token: &str) -> Result<()> {
        let raw = std::fs::read_to_string(&self.path)?;
        let eol = if raw.contains("\r\n") { "\r\n" } else { "\n" };
        let mut lines: Vec<String> = raw.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
        let new = format!("token = \"{token}\"");
        let mut found = false;
        for l in lines.iter_mut() {
            if l.trim_start().starts_with("token") && l.contains('=') {
                *l = new.clone();
                found = true;
                break;
            }
        }
        if !found {
            // Insert before the first [section], else at the top.
            let idx = lines.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(lines.len());
            lines.insert(idx, new);
        }
        let out = lines.join(eol) + eol;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 64-hex-char random secret (256 bits).
    pub fn gen_token() -> String {
        let bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Rewrite `active_profile = "<name>"` in the source file so the choice
    /// survives a restart. Surgical line replacement keeps every comment
    /// intact; if the key is missing it is inserted before the first
    /// `[section]` (or appended). Write is atomic (temp file + rename).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn persist_active_profile(&self, name: &str) -> Result<()> {
        let raw = std::fs::read_to_string(&self.path)?;
        let eol = if raw.contains("\r\n") { "\r\n" } else { "\n" };
        let mut lines: Vec<String> = raw.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
        let new = format!("active_profile = \"{}\"", name);
        let is_key = |l: &String| {
            let t = l.trim_start();
            (t == "active_profile")
                || t.starts_with("active_profile ")
                || t.starts_with("active_profile=")
        };
        match lines.iter().position(is_key) {
            Some(i) => lines[i] = new,
            None => {
                let at = lines
                    .iter()
                    .position(|l| l.trim_start().starts_with('['))
                    .unwrap_or(lines.len());
                lines.insert(at, new);
            }
        }
        let mut out = lines.join(eol);
        if raw.ends_with('\n') {
            out.push_str(eol);
        }
        let tmp = self.path.with_file_name(format!(".{}~tmp",
            self.path.file_name().and_then(|s| s.to_str()).unwrap_or("llm_proxy.toml")));
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Resolve relative `exe`/`workdir` values against `exe_dir` (the proxy's
/// own directory), so a config travels with the binary (CWD-independent):
/// copy exe + toml + backend together and use relative paths.
/// Absolute paths and bare exe names (no separator) are left untouched —
/// CreateProcess finds them next to the proxy exe via its search order.
fn resolve_profile_paths(cfg: &mut Config, exe_dir: &Path) {
    for p in cfg.profiles.values_mut() {
        let exe = Path::new(&p.exe);
        if !exe.is_absolute() && exe.components().count() > 1 {
            p.exe = normalize(exe_dir.join(exe)).display().to_string();
        }
        if let Some(w) = p.workdir.as_mut() {
            if !Path::new(w).is_absolute() {
                *w = normalize(exe_dir.join(w.as_str())).display().to_string();
            }
        }
    }
}

/// Drop `.` components (Path::join keeps them, e.g. `C:\base\.`).
fn normalize(p: PathBuf) -> PathBuf {
    p.components().filter(|c| !matches!(c, Component::CurDir)).collect()
}

pub fn default_config_path() -> PathBuf {
    match std::env::current_exe() {
        Ok(exe) => {
            let dir = exe.parent().unwrap_or_else(|| Path::new("."));
            dir.join("llm_proxy.toml")
        }
        Err(_) => PathBuf::from("llm_proxy.toml"),
    }
}

/// Config lookup: command-line arg ending in `.toml`, else
/// `exe_dir/llm_proxy.toml` (fallback `CWD/llm_proxy.toml`).
/// No defaults: without the file the proxy exits with an error.
pub fn load_config() -> Result<Config> {
    let path = std::env::args()
        .skip(1)
        .find(|a| a.ends_with(".toml"))
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let mut cfg = Config::parse(&raw)?;
        cfg.path = path.clone();
        // token = "" (local mode only, rejected in parse for bridges):
        // generate a 256-bit secret, persist it, print it once.
        if cfg.remote.is_none() && cfg.token.as_deref() == Some("") {
            let tok = Config::gen_token();
            cfg.token = Some(tok.clone());
            match cfg.persist_token(&tok) {
                Ok(()) => tracing::info!(
                    "token generated: {tok} — persisted to {} — copy it into the bridge configs",
                    path.display()
                ),
                Err(e) => tracing::warn!(err = %e, "token generated but could not be persisted to config (it survives this run only)"),
            }
            if std::io::stdout().is_terminal() {
                println!("token generated: {tok}");
                println!("  persisted to {} — copy it into the bridge config (token = \"...\")", path.display());
            }
        }
        let def = default_config_path();
        let exe_dir = def
            .parent()
            .filter(|d| d.is_dir())
            .unwrap_or_else(|| Path::new("."));
        resolve_profile_paths(&mut cfg, exe_dir);
        tracing::info!(path = %path.display(), "loaded config");
        return Ok(cfg);
    }
    tracing::warn!(path = %path.display(), "config file not found — exiting (no defaults)");
    bail!("config file not found at {} — create it and retry", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("llm_proxy_persist_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const RAW: &str = "# top comment\nactive_profile = \"a\"\n\n[proxy]\nport = 1000\nstats_port = 1001\n\n[profiles.a]\nexe = \"a.exe\"\nargs = [\"--port\", \"1\"]\n\n[profiles.b]\nexe = \"b.exe\"\nargs = [\"--port\", \"2\"]\n";

    #[test]
    fn persist_rewrites_active_profile_and_keeps_comments() {
        let path = temp_dir("rewrite").join("cfg.toml");
        std::fs::write(&path, RAW).unwrap();
        let mut cfg: Config = toml::from_str(RAW).unwrap();
        cfg.path = path.clone();
        cfg.persist_active_profile("b").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("active_profile = \"b\""), "rewritten line");
        assert!(raw.contains("# top comment"), "comments kept");
        assert_eq!(raw.matches("active_profile").count(), 1, "single occurrence");
        let cfg2: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg2.active_profile.as_deref(), Some("b"));
        // no temp file left behind
        assert!(!path.with_file_name(".cfg.toml~tmp").exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn persist_inserts_missing_key_before_first_section() {
        let path = temp_dir("insert").join("cfg.toml");
        let raw = "[proxy]\nport = 1000\nstats_port = 1001\n\n[profiles.a]\nexe = \"a.exe\"\nargs = [\"--port\", \"1\"]\n";
        std::fs::write(&path, raw).unwrap();
        // The file alone can't parse (active_profile is required), so build
        // the Config by hand — the method under test only needs self.path.
        let mut cfg = Config {
            proxy: ProxyConfig { port: 1, stats_port: 2 },
            remote: None,
            active_profile: Some("a".into()),
            token: None,
            bind: None,
            profiles: HashMap::new(),
            path: path.clone(),
        };
        cfg.profiles.insert(
            "a".into(),
            Profile {
                exe: "a.exe".into(),
                workdir: None,
                args: vec!["--port".into(), "1".into()],
                health: None,
                idle_unload_secs: 0,
                model_id: "m".into(),
                backend_host: None,
                backend_port: None,
                context_length: None,
                vision: false,
            },
        );
        cfg.persist_active_profile("a").unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let cfg2: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg2.active_profile.as_deref(), Some("a"));
        let pos_key = out.find("active_profile").unwrap();
        let pos_first_section = out.find("[").unwrap();
        assert!(pos_key < pos_first_section, "inserted before first section");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn token_generation_and_persist() {
        let tok = Config::gen_token();
        assert_eq!(tok.len(), 64, "256 bits in hex");
        assert!(tok.chars().all(|c| c.is_ascii_hexdigit()));

        let path = temp_dir("token").join("cfg.toml");
        let raw = "# comment\n[proxy]\nport = 1000\nstats_port = 1001\n";
        std::fs::write(&path, raw).unwrap();
        let cfg = Config {
            proxy: ProxyConfig { port: 1, stats_port: 2 },
            remote: None,
            active_profile: None,
            token: Some(tok.clone()),
            bind: None,
            profiles: HashMap::new(),
            path: path.clone(),
        };
        cfg.persist_token(&tok).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains(&format!("token = \"{tok}\"")), "token persisted");
        assert!(out.contains("# comment"), "comments kept");
        assert_eq!(out.matches("token").count(), 1, "single occurrence");
        let cfg2: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg2.token.as_deref(), Some(tok.as_str()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn token_validation() {
        // bridge mode with token = "": rejected (it must be the remote's)
        let e = Config::parse(
            r#"
remote = "192.168.1.50:9931"
token = ""

[proxy]
port = 9932
stats_port = 9933
"#
        )
        .unwrap_err();
        assert!(e.to_string().contains("remote mode"));

        // empty bind: rejected
        let e = Config::parse(
            r#"
active_profile = "a"
bind = ""

[proxy]
port = 9932
stats_port = 9933

[profiles.a]
exe = "a.exe"
args = []
"#
        )
        .unwrap_err();
        assert!(e.to_string().contains("bind"));
    }

    #[test]
    fn remote_mode_validation() {
        // valid bridge config: no profiles needed
        let cfg = Config::parse(
            r#"
remote = "192.168.1.50:9931"

[proxy]
port = 9932
stats_port = 9933
"#
        )
        .unwrap();
        assert_eq!(cfg.remote.as_deref(), Some("192.168.1.50:9931"));
        assert!(cfg.profiles.is_empty());

        // remote + profiles: rejected
        let e = Config::parse(
            r#"
remote = "192.168.1.50:9931"
active_profile = "a"

[proxy]
port = 9932
stats_port = 9933

[profiles.a]
exe = "a.exe"
args = []
"#
        )
        .unwrap_err();
        assert!(e.to_string().contains("remote mode"));

        // malformed remote: rejected
        let e = Config::parse(
            r#"
remote = "192.168.1.50"

[proxy]
port = 9932
stats_port = 9933
"#
        )
        .unwrap_err();
        assert!(e.to_string().contains("host:port"));
    }

    #[test]
    fn backend_host_and_port_explicit_fields_win() {
        let p = Profile {

            exe: "x.exe".into(),
            workdir: None,
            args: vec!["--host".into(), "10.0.0.9".into(), "--port".into(), "1".into()],
            health: None,
            idle_unload_secs: 0,
            model_id: "m".into(),
            backend_host: Some("192.168.7.7".into()),
            backend_port: Some(9999),
            context_length: None,
            vision: false,
        };
        assert_eq!(p.backend_host(), "192.168.7.7");
        assert_eq!(p.backend_port(), 9999);
        // Without the explicit fields the args derivation still applies.
        let p2 = Profile {
            backend_host: None,
            backend_port: None,
            ..p
        };
        assert_eq!(p2.backend_host(), "10.0.0.9");
        assert_eq!(p2.backend_port(), 1);
    }

    #[test]
    fn backend_host_and_port_derived_from_args() {
        let profile = |args: &[&str]| Profile {

            exe: "x.exe".into(),
            workdir: None,
            args: args.iter().map(|s| s.to_string()).collect(),
            health: None,
            idle_unload_secs: 0,
            model_id: "m".into(),
            backend_host: None,
            backend_port: None,
            context_length: None,
            vision: false,
        };
        assert_eq!(profile(&["--host", "127.0.0.1", "--port", "8080"]).backend_host(), "127.0.0.1");
        assert_eq!(profile(&["--host", "127.0.0.1", "--port", "8080"]).backend_port(), 8080);
        // bind on all interfaces loops back to 127.0.0.1
        assert_eq!(profile(&["--host", "0.0.0.0", "--port", "9931"]).backend_host(), "127.0.0.1");
        assert_eq!(profile(&["--host", "::", "--port", "9931"]).backend_host(), "127.0.0.1");
        // specific bind: pass through (loopback would not reach it)
        assert_eq!(profile(&["--host", "192.168.1.5", "--port", "9932"]).backend_host(), "192.168.1.5");
        assert_eq!(profile(&["--host", "192.168.1.5", "--port", "9932"]).backend_port(), 9932);
        // equals form
        assert_eq!(profile(&["--host=10.0.0.2", "--port=9933"]).backend_host(), "10.0.0.2");
        assert_eq!(profile(&["--host=10.0.0.2", "--port=9933"]).backend_port(), 9933);
        // absent: llama.cpp defaults
        assert_eq!(profile(&["--max-context", "4096"]).backend_host(), "127.0.0.1");
        assert_eq!(profile(&["--max-context", "4096"]).backend_port(), 8080);
    }
}
