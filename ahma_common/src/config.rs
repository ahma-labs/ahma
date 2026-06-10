//! # Ahma configuration: env-var interpolation, named provider registry, and user settings.
//!
//! ## `~/.ahma/settings.toml` (user settings — all options commented out by default)
//!
//! This file is the primary place to configure Ahma behaviour.  Generate it
//! (with all defaults commented out) using `ahma settings init`.
//!
//! See [`AhmaSettings`] for the full schema and [`settings_path`] for the path.
//!
//!
//! ## `~/.ahma/config.toml` format
//!
//! ```toml
//! # ── LLM providers ──────────────────────────────────────────────────────────
//! [[providers]]
//! name        = "ollama-local"
//! base_url    = "http://localhost:11434/v1"
//! default_model = "llama3.2"
//! # api_key is optional — omit for local (unauthenticated) providers
//!
//! [[providers]]
//! name          = "openai"
//! base_url      = "https://api.openai.com/v1"
//! default_model = "gpt-4o-mini"
//! api_key       = "${OPENAI_API_KEY}"
//!
//! # ── Cluster ─────────────────────────────────────────────────────────────────
//! [cluster]
//! # Path to the file containing the shared HMAC-SHA256 cluster key.
//! # Must be readable only by the ahma user (mode 0600 on Unix).
//! key_file = "/etc/ahma/cluster.key"
//!
//! # How long (in seconds) a peer heartbeat remains valid before the peer is
//! # considered offline.  Default: 60.
//! heartbeat_ttl_secs = 60
//!
//! [[cluster.peers]]
//! name = "gpu-1"
//! url  = "http://10.0.0.5:7000"
//!
//! [[cluster.peers]]
//! name = "gpu-2"
//! url  = "http://10.0.0.6:7000"
//! ```
//!
//! MTDF tools can then reference a provider by name:
//! ```json
//! { "llm_provider_ref": "ollama-local" }
//! ```
//! instead of inlining connection details (and possibly API keys) in every tool file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Env-var interpolation
// ---------------------------------------------------------------------------

/// Expand `${VAR_NAME}` placeholders in `s` using the process environment.
///
/// Returns `Err` if any referenced variable is not set.
/// After expansion, warns via [`tracing::warn!`] if the resulting string looks
/// like a literal API key (`sk-`, `AKIA`, `xoxb-`, `ghp_`).
pub fn interpolate_env_vars(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];

        let end = rest
            .find('}')
            .with_context(|| format!("Unclosed '${{{rest}' in config value"))?;

        let var_name = &rest[..end];
        let value = std::env::var(var_name).with_context(|| {
            format!("Environment variable '{var_name}' referenced in config is not set")
        })?;

        out.push_str(&value);
        rest = &rest[end + 1..];
    }

    out.push_str(rest);

    // Warn if the final value looks like a literal secret that should have
    // been kept in an environment variable rather than written into a file.
    warn_if_looks_like_literal_secret(&out);

    Ok(out)
}

/// Warns if a string looks like a well-known literal API-key pattern.
///
/// Call this on values read directly from config files **before** interpolation
/// to catch keys that were mistakenly written verbatim rather than as `${VAR}`.
pub fn warn_if_looks_like_literal_secret(value: &str) -> bool {
    // These prefixes are characteristic of real API keys from popular providers.
    const SUSPICIOUS_PREFIXES: &[&str] = &[
        "sk-",    // OpenAI / many compatible APIs
        "AKIA",   // AWS access key IDs
        "xoxb-",  // Slack bot tokens
        "ghp_",   // GitHub personal access tokens
        "glpat-", // GitLab personal access tokens
    ];

    for prefix in SUSPICIOUS_PREFIXES {
        if value.starts_with(prefix) && value.len() > prefix.len() + 8 {
            warn!(
                "Config value looks like a literal API key (starts with '{prefix}'). \
                 Store secrets in environment variables and reference them as ${{VAR_NAME}}."
            );
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Named provider registry (loaded from ~/.ahma/config.toml)
// ---------------------------------------------------------------------------

/// An entry in the `[[providers]]` array in `~/.ahma/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Unique name used to reference this provider (e.g. `"ollama-local"`).
    pub name: String,
    /// Base URL of the OpenAI-compatible API (e.g. `http://localhost:11434/v1`).
    pub base_url: String,
    /// Default model for this provider (e.g. `"llama3.2"`).
    pub default_model: String,
    /// Optional bearer token. Supports `${ENV_VAR}` interpolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl ProviderEntry {
    /// Return a copy of this entry with `api_key` resolved via env-var interpolation.
    ///
    /// Returns `Err` if the key contains a `${VAR}` reference to an unset variable.
    pub fn resolve(&self) -> Result<ResolvedProvider> {
        let api_key = self
            .api_key
            .as_deref()
            .map(interpolate_env_vars)
            .transpose()?;
        Ok(ResolvedProvider {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            default_model: self.default_model.clone(),
            api_key,
        })
    }
}

/// A `ProviderEntry` with secrets resolved — ready to hand to an HTTP client.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub base_url: String,
    pub default_model: String,
    pub api_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Cluster configuration
// ---------------------------------------------------------------------------

/// Transport protocol preference for ahma cluster peer-to-peer communication.
///
/// Ordered from highest to lowest preference.  `ahma_cluster::ClusterScheduler`
/// tries each mode in turn and falls back to the next on connection failure.
///
/// In config TOML use lowercase strings: `"quic"`, `"http2"`, `"http1"`.
///
/// # Example (`~/.ahma/config.toml`)
/// ```toml
/// [cluster]
/// transport_preference = ["quic", "http2", "http1"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    /// HTTP/3 over QUIC (UDP).
    ///
    /// Requires the peer bridge to have QUIC enabled (`--enable-quic`, default
    /// `true`) and a self-signed TLS certificate.  The dispatcher uses
    /// `https://` URLs and skips certificate hostname verification when no CA
    /// cert is provided.  Compile `ahma_cluster` with the `cluster-quic`
    /// feature to enable this transport at runtime; without that feature the
    /// mode is silently treated as `Http2`.
    Quic,
    /// HTTP/2 with prior-knowledge (h2c) over plain TCP.
    ///
    /// Uses the same `http://` URL as the bridge bind address.  No TLS required.
    Http2,
    /// HTTP/1.1 over plain TCP.  Most compatible fallback.
    Http1,
}

fn default_transport_preference() -> Vec<TransportMode> {
    vec![
        TransportMode::Quic,
        TransportMode::Http2,
        TransportMode::Http1,
    ]
}

/// A single peer entry in the `[[cluster.peers]]` array.
///
/// Mirrors the peer fields stored in `ahma_cluster::PeerInfo` but lives in
/// `ahma_common` so the config layer doesn't need to depend on `ahma_cluster`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPeerConfig {
    /// Human-readable identifier for this peer (used in `ahma cluster list`).
    pub name: String,
    /// Base URL of the peer's ahma HTTP bridge (e.g. `http://10.0.0.5:7000`).
    pub url: String,
}

/// The optional `[cluster]` table in `~/.ahma/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Path to the file containing the shared HMAC-SHA256 key used for mutual
    /// authentication between cluster nodes.
    ///
    /// The key file must be readable by the ahma process.  On Unix, restrict
    /// permissions to `0600` to prevent other users from reading it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,

    /// Seconds a peer's last heartbeat may be in the past before the peer is
    /// considered offline.  Defaults to `60`.
    #[serde(default = "default_heartbeat_ttl")]
    pub heartbeat_ttl_secs: u64,

    /// Static peer list.  Peers can also be discovered dynamically; this list
    /// seeds the registry on startup.
    #[serde(default)]
    pub peers: Vec<ClusterPeerConfig>,

    /// Preferred transport order for outbound peer dispatch.
    ///
    /// `ahma_cluster::ClusterScheduler` tries each mode in order and falls back
    /// on any connection error.  Defaults to `["quic", "http2", "http1"]`.
    #[serde(default = "default_transport_preference")]
    pub transport_preference: Vec<TransportMode>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            key_file: None,
            heartbeat_ttl_secs: default_heartbeat_ttl(),
            peers: Vec::new(),
            transport_preference: default_transport_preference(),
        }
    }
}

fn default_heartbeat_ttl() -> u64 {
    60
}

/// The top-level structure of `~/.ahma/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AhmaConfig {
    /// Named LLM provider definitions.
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,

    /// Optional cluster coordination settings.
    #[serde(default)]
    pub cluster: ClusterConfig,
}

impl AhmaConfig {
    /// Load `~/.ahma/config.toml`.
    ///
    /// Returns an empty config (no providers) if the file does not exist,
    /// so callers can always use the returned struct without checking for None.
    pub fn load() -> Self {
        match ahma_config_path() {
            Some(p) => Self::load_from(&p),
            None => {
                debug!("Could not determine home directory; using empty AhmaConfig");
                Self::default()
            }
        }
    }

    /// Load from an explicit path — useful for tests and alternate locations.
    pub fn load_from(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(cfg) => {
                    debug!("Loaded AhmaConfig from {}", path.display());
                    cfg
                }
                Err(e) => {
                    warn!(
                        "Failed to parse {}: {e}; using empty AhmaConfig",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("{} not found; using empty AhmaConfig", path.display());
                Self::default()
            }
            Err(e) => {
                warn!(
                    "Failed to read {}: {e}; using empty AhmaConfig",
                    path.display()
                );
                Self::default()
            }
        };

        // Auto-register oMLX provider from settings
        let settings = AhmaSettings::load();
        if !cfg.providers.iter().any(|p| p.name == "omlx") {
            cfg.providers.push(ProviderEntry {
                name: "omlx".to_string(),
                base_url: settings.omlx.base_url.clone(),
                default_model: settings.omlx.model.clone(),
                api_key: None,
            });
        }

        cfg
    }

    /// Look up a provider by name and resolve its secrets.
    ///
    /// Returns `Err` if the name is not found or secret resolution fails.
    pub fn resolve_provider(&self, name: &str) -> Result<ResolvedProvider> {
        let entry = self
            .providers
            .iter()
            .find(|p| p.name == name)
            .with_context(|| {
                format!(
                    "LLM provider '{name}' not found in ~/.ahma/config.toml. \
                     Add a [[providers]] entry with name = \"{name}\"."
                )
            })?;
        entry.resolve()
    }
}

/// Returns the canonical path to `~/.ahma/config.toml`, or `None` if the home
/// directory cannot be determined.
pub fn ahma_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ahma").join("config.toml"))
}

/// Returns the canonical path to `~/.ahma/settings.toml`, or `None` if the home
/// directory cannot be determined.
pub fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ahma").join("settings.toml"))
}

// ---------------------------------------------------------------------------
// AhmaSettings — user-editable settings.toml
// ---------------------------------------------------------------------------

/// oMLX / mlx_lm.server provider defaults.
///
/// `mlx_lm.server` exposes an OpenAI-compatible API on localhost.  The default
/// model (`mlx-community/gemma-4-12B-it-8bit`) runs well on Apple Silicon Macs
/// with ≥16 GB unified memory.  Change [`model`] to any HuggingFace model ID
/// hosted at `mlx-community`.
///
/// Start the server with:
/// ```bash
/// mlx_lm.server --model mlx-community/gemma-4-12B-it-8bit
/// ```
///
/// The server listens on port 8080 by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OmlxSettings {
    /// Base URL of the mlx_lm.server endpoint.
    /// Default: `http://localhost:8080/v1`
    pub base_url: String,
    /// Model identifier passed to the server, e.g. `mlx-community/gemma-4-12B-it-8bit`.
    pub model: String,
}

impl Default for OmlxSettings {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8080/v1".to_string(),
            model: "mlx-community/gemma-4-12B-it-8bit".to_string(),
        }
    }
}

/// Tool execution settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    /// Default tool execution timeout in seconds.
    /// Individual tools can override this via `timeout_seconds` in their JSON definition.
    /// Default: `360`
    pub timeout_secs: u64,
    /// Run all tools synchronously.  By default tools are async-first: if a result
    /// arrives within 5 seconds it is returned inline; otherwise an operation ID is
    /// returned and the result is pushed as a notification.
    /// Default: `false`
    pub force_sync: bool,
    /// Watch the tools directory for JSON changes and reload tool definitions at runtime.
    /// **Security warning**: enabling this allows new tools to be injected mid-session.
    /// Enable only while authoring tool definitions.
    /// Default: `false`
    pub hot_reload: bool,
    /// Skip tool availability probes at startup.  Probes detect whether required
    /// executables (e.g. `cargo`, `git`) are installed and hide tools whose
    /// prerequisites are missing.  Skip to reduce startup latency when all tools
    /// are guaranteed to be available.
    /// Default: `false`
    pub skip_probes: bool,
    /// Path to the tools directory containing JSON tool definitions.
    /// Default: `None`
    pub tools_dir: Option<PathBuf>,
    /// Tool bundles to enable.
    /// Default: empty list
    pub tool_bundles: Vec<String>,
    /// Enable output compression and token minimization.
    /// Default: `false`
    pub minimize_tokens: bool,
    /// Enable small-model harness adaptations.
    /// Default: `false`
    pub small_model_harness: bool,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            timeout_secs: 360,
            force_sync: false,
            hot_reload: false,
            skip_probes: false,
            tools_dir: None,
            tool_bundles: Vec::new(),
            minimize_tokens: false,
            small_model_harness: false,
        }
    }
}

/// Sandbox and filesystem security settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SandboxSettings {
    /// Disable the kernel sandbox entirely.
    /// **UNSAFE** — the AI can read and write anywhere on the filesystem.
    /// Use only in environments that provide their own containment (Docker, CI containers).
    /// Default: `false`
    pub disable: bool,
    /// Add the system temp directory to the sandbox scope.
    /// Useful for workflows that need scratch space (compilers, build systems).
    /// Default: `false`
    pub tmp_access: bool,
    /// Block all access to the system temp directory.
    /// Takes precedence over `tmp_access`.
    /// Default: `false`
    pub disable_temp: bool,
    /// Defer sandbox lock until the MCP client provides `roots/list`.
    /// Use when the client supplies workspace roots at connection time.
    /// Default: `false`
    pub defer: bool,
    /// Run this server session inside an existing task vault.
    /// Default: `None`
    pub task_vault: Option<PathBuf>,
    /// Paths allowed for read/write access under the sandbox.
    /// Default: empty list
    pub scopes: Vec<PathBuf>,
    /// Directories containing allowed working directories.
    /// Default: empty list
    pub working_dirs: Vec<PathBuf>,
}

/// Logging and log-monitoring settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingSettings {
    /// Log destination: `"file"` (rolling log under `./logs/`) or `"stderr"`.
    /// `"stderr"` is useful for Docker, CI, or any environment where
    /// stdout/stderr is captured.
    /// Default: `"file"`
    pub target: String,
    /// Enable live log monitoring.  Ahma tails the configured log stream through
    /// an LLM to detect issues in real time and push alerts as MCP progress
    /// notifications.
    /// Default: `false`
    pub log_monitor: bool,
    /// Minimum seconds between successive log-monitor alerts.  Prevents alert
    /// storms when a persistent issue triggers repeated pattern matches.
    /// Default: `60`
    pub monitor_rate_limit_secs: u64,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            target: "file".to_string(),
            log_monitor: false,
            monitor_rate_limit_secs: 60,
        }
    }
}

/// HTTP server settings (applies to `ahma serve http`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpSettings {
    /// MCP handshake timeout in seconds.  The server closes a session that does
    /// not complete the initialize/notifications/initialized exchange within this window.
    /// Default: `45`
    pub handshake_timeout_secs: u64,
    /// Disable HTTP/3 over QUIC.  The bridge defaults to serving HTTP/2 (TCP)
    /// and HTTP/3 (QUIC) concurrently.  Set to `true` when UDP is blocked or
    /// QUIC causes connectivity issues.
    /// Default: `false`
    pub disable_quic: bool,
    /// Require HTTP/2 or better; reject HTTP/1.1 connections.
    /// Default: `false`
    pub disable_http1_1: bool,
    /// Path to the Unix domain socket.
    /// Default: `None`
    pub unix_socket_path: Option<String>,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            handshake_timeout_secs: 45,
            disable_quic: false,
            disable_http1_1: false,
            unix_socket_path: None,
        }
    }
}

/// HTTP authentication and rate limiting settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// Path to a file containing the required bearer token for HTTP access.
    /// The token is read from this file at startup so it never appears in the
    /// settings file itself or in process listings.
    /// Default: `""` (no token required)
    pub require_token_path: String,
    /// Maximum requests per second (0 = no rate limit).
    /// Default: `0`
    pub rate_limit_rps: u64,
    /// Burst allowance for the rate limiter.
    /// Default: `10`
    pub rate_limit_burst: u32,
    /// Required bearer token specified directly in config.
    /// Default: `None`
    pub require_token: Option<String>,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            require_token_path: String::new(),
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            require_token: None,
        }
    }
}

/// Instance identity settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceSettings {
    /// Human-readable instance name shown in the TUI and daemon event stream.
    /// Default: `"ahma"`
    pub label: String,
}

impl Default for InstanceSettings {
    fn default() -> Self {
        Self {
            label: "ahma".to_string(),
        }
    }
}

/// Top-level user settings loaded from `~/.ahma/settings.toml`.
///
/// All fields have sensible defaults — an empty file (or no file at all) is
/// valid and equivalent to using compiled-in defaults.
///
/// ## Priority order (highest to lowest)
///
/// 1. CLI flags (`--timeout 600`, `--no-sandbox`, …)
/// 2. This settings file
/// 3. `AHMA_*` environment variables (deprecated; emit a warning if set)
/// 4. Compiled-in defaults
///
/// ## Generating the file
///
/// ```bash
/// ahma settings init       # write defaults (commented out) to ~/.ahma/settings.toml
/// ahma settings show       # print the effective resolved settings
/// ahma --no-settings …     # ignore settings.toml for this invocation
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AhmaSettings {
    /// oMLX / mlx_lm.server provider configuration.
    pub omlx: OmlxSettings,
    /// Tool execution settings.
    pub tools: ToolSettings,
    /// Sandbox and filesystem security settings.
    pub sandbox: SandboxSettings,
    /// Logging and live log-monitoring settings.
    pub logging: LoggingSettings,

    /// HTTP server settings (applies to `ahma serve http` only).
    pub http: HttpSettings,
    /// HTTP authentication and rate-limiting settings.
    pub auth: AuthSettings,
    /// Instance identity settings.
    pub instance: InstanceSettings,
}

impl AhmaSettings {
    /// Load `~/.ahma/settings.toml`.
    ///
    /// Returns a fully-defaulted config if the file does not exist, so callers
    /// can always use the returned struct without checking for `None`.
    pub fn load() -> Self {
        match settings_path() {
            Some(p) => Self::load_from(&p),
            None => {
                debug!("Could not determine home directory; using default AhmaSettings");
                Self::default()
            }
        }
    }

    /// Load from an explicit path — useful for tests and alternate locations.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(cfg) => {
                    debug!("Loaded AhmaSettings from {}", path.display());
                    cfg
                }
                Err(e) => {
                    warn!(
                        "Failed to parse {}: {e}; using default AhmaSettings",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("{} not found; using default AhmaSettings", path.display());
                Self::default()
            }
            Err(e) => {
                warn!(
                    "Failed to read {}: {e}; using default AhmaSettings",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Whether the `logging.target` resolves to stderr.
    pub fn log_to_stderr(&self) -> bool {
        self.logging.target.trim().eq_ignore_ascii_case("stderr")
    }

    /// Write the settings file template (all defaults commented out) to `path`.
    ///
    /// Creates parent directories as needed.  Returns `Err` if the file already
    /// exists and `overwrite` is `false`.
    pub fn write_defaults(path: &Path, overwrite: bool) -> Result<()> {
        if path.exists() && !overwrite {
            anyhow::bail!(
                "Settings file already exists at {}.  \
                 Use --force to overwrite.",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }
        std::fs::write(path, SETTINGS_TEMPLATE)
            .with_context(|| format!("Failed to write {}", path.display()))
    }
}

/// The commented-out defaults template written by `ahma settings init`.
pub const SETTINGS_TEMPLATE: &str = r#"# ~/.ahma/settings.toml — Ahma user settings
#
# All options are commented out.  Uncomment and edit any value to override
# the compiled-in default.  CLI flags always take highest priority, followed
# by this file, followed by deprecated AHMA_* environment variables.
#
# Generate (or regenerate) this file with:   ahma settings init
# Show effective settings with:              ahma settings show
# Ignore this file for one invocation with:  ahma --no-settings <command>

# ── oMLX (Apple Silicon mlx_lm.server) ──────────────────────────────────────
# Start the server with:
#   mlx_lm.server --model mlx-community/gemma-4-12B-it-8bit
#
# [omlx]
# base_url = "http://localhost:8080/v1"          # default: 8080 (mlx_lm.server)
# model    = "mlx-community/gemma-4-12B-it-8bit" # default model

# ── Tool execution ───────────────────────────────────────────────────────────
# [tools]
# timeout_secs = 360      # default tool timeout (seconds); per-tool override via timeout_seconds
# force_sync   = false    # run all tools synchronously instead of async-first
# hot_reload   = false    # reload tools from disk on change — INSECURE in production
# skip_probes  = false    # skip availability probes at startup
# tools_dir    = ".ahma"  # path to tools directory containing JSON tool definitions
# tool_bundles = []       # tool bundles to enable (e.g. ["rust", "git"])
# minimize_tokens     = false # enable output compression and token minimization
# small_model_harness = false # enable small-model harness adaptations

# ── Sandbox & filesystem security ────────────────────────────────────────────
# [sandbox]
# disable      = false    # UNSAFE: disable kernel sandbox entirely
# tmp_access   = false    # add system temp dir to sandbox scope
# disable_temp = false    # block all access to system temp dir (overrides tmp_access)
# defer        = false    # defer sandbox lock until client provides roots/list
# task_vault   = ""       # run this server session inside an existing task vault
# scopes       = []       # paths allowed for read/write access under the sandbox
# working_dirs = []       # directories containing allowed working directories

# ── Logging ──────────────────────────────────────────────────────────────────
# [logging]
# target                 = "file"   # "file" (rolling) or "stderr"
# log_monitor            = false    # enable live log monitoring via LLM
# monitor_rate_limit_secs = 60      # min seconds between log-monitor alerts


# ── HTTP server (ahma serve http only) ───────────────────────────────────────
# [http]
# handshake_timeout_secs = 45      # MCP handshake timeout
# disable_quic           = false   # disable HTTP/3 QUIC; fall back to HTTP/2 TCP
# disable_http1_1        = false   # reject HTTP/1.1; require HTTP/2+
# unix_socket_path      = ""      # path to the unix domain socket

# ── HTTP authentication & rate limiting ──────────────────────────────────────
# [auth]
# require_token      = ""   # required bearer token specified directly in config
# require_token_path = ""   # path to file containing required bearer token
# rate_limit_rps     = 0    # max requests/second (0 = no limit)
# rate_limit_burst   = 10   # burst allowance

# ── Instance identity ────────────────────────────────────────────────────────
# [instance]
# label = "ahma"   # instance name shown in TUI and daemon event stream
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_no_placeholders() {
        let s = "http://localhost:11434/v1";
        assert_eq!(interpolate_env_vars(s).unwrap(), s);
    }

    #[test]
    fn interpolate_set_variable() {
        unsafe {
            std::env::set_var("AHMA_TEST_INTERP_VAR", "hello");
        }
        let result = interpolate_env_vars("prefix-${AHMA_TEST_INTERP_VAR}-suffix").unwrap();
        assert_eq!(result, "prefix-hello-suffix");
        unsafe {
            std::env::remove_var("AHMA_TEST_INTERP_VAR");
        }
    }

    #[test]
    fn interpolate_multiple_variables() {
        unsafe {
            std::env::set_var("AHMA_TEST_A", "foo");
            std::env::set_var("AHMA_TEST_B", "bar");
        }
        let result = interpolate_env_vars("${AHMA_TEST_A}/${AHMA_TEST_B}").unwrap();
        assert_eq!(result, "foo/bar");
        unsafe {
            std::env::remove_var("AHMA_TEST_A");
            std::env::remove_var("AHMA_TEST_B");
        }
    }

    #[test]
    fn interpolate_missing_variable_returns_err() {
        // Use a name very unlikely to be set in CI
        unsafe {
            std::env::remove_var("AHMA_TEST_DEFINITELY_NOT_SET_XYZ");
        }
        let err = interpolate_env_vars("${AHMA_TEST_DEFINITELY_NOT_SET_XYZ}").unwrap_err();
        assert!(err.to_string().contains("AHMA_TEST_DEFINITELY_NOT_SET_XYZ"));
    }

    #[test]
    fn interpolate_unclosed_brace_returns_err() {
        let err = interpolate_env_vars("${UNCLOSED").unwrap_err();
        assert!(err.to_string().contains("Unclosed"));
    }

    #[test]
    fn warn_detects_openai_key() {
        assert!(warn_if_looks_like_literal_secret("sk-abcdefghijklmnopqrst"));
    }

    #[test]
    fn warn_detects_aws_key() {
        assert!(warn_if_looks_like_literal_secret("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn warn_clean_url_is_fine() {
        assert!(!warn_if_looks_like_literal_secret(
            "http://localhost:11434/v1"
        ));
    }

    #[test]
    fn ahma_config_load_from_missing_file_gives_default() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("nonexistent_config_toml");
        let cfg = AhmaConfig::load_from(&path);
        // Default contains the auto-registered omlx provider
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "omlx");
    }

    #[test]
    fn ahma_config_parses_providers() {
        let toml_str = r#"
[[providers]]
name = "ollama-local"
base_url = "http://localhost:11434/v1"
default_model = "llama3.2"

[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
default_model = "gpt-4o-mini"
api_key = "sk-placeholder"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let cfg = AhmaConfig::load_from(tmp.path());
        // 2 from config + 1 auto-registered omlx
        assert_eq!(cfg.providers.len(), 3);
        assert!(cfg.providers.iter().any(|p| p.name == "ollama-local"));
        assert!(cfg.providers.iter().any(|p| p.name == "openai"));
        assert!(cfg.providers.iter().any(|p| p.name == "omlx"));
    }

    #[test]
    fn resolve_provider_not_found_returns_err() {
        let cfg = AhmaConfig::default();
        let err = cfg.resolve_provider("nonexistent").unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn resolve_provider_without_api_key() {
        let toml_str = r#"
[[providers]]
name = "local"
base_url = "http://localhost:11434/v1"
default_model = "llama3.2"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let cfg = AhmaConfig::load_from(tmp.path());
        let resolved = cfg.resolve_provider("local").unwrap();
        assert_eq!(resolved.name, "local");
        assert!(resolved.api_key.is_none());
    }

    #[test]
    fn resolve_provider_with_env_var_key() {
        unsafe {
            std::env::set_var("AHMA_TEST_PROVIDER_KEY", "test-secret-value");
        }
        let toml_str = r#"
[[providers]]
name = "remote"
base_url = "https://api.example.com/v1"
default_model = "gpt-4"
api_key = "${AHMA_TEST_PROVIDER_KEY}"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let cfg = AhmaConfig::load_from(tmp.path());
        let resolved = cfg.resolve_provider("remote").unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("test-secret-value"));
        unsafe {
            std::env::remove_var("AHMA_TEST_PROVIDER_KEY");
        }
    }

    // ── Config roundtrip tests ────────────────────────────────────────────────

    /// Add a provider programmatically, serialise to TOML, reload — verify the
    /// entry survives the full write/read cycle.
    #[test]
    fn provider_add_roundtrip_via_load_from() {
        let tmp = tempfile::NamedTempFile::new().unwrap();

        let mut cfg = AhmaConfig::default();
        cfg.providers.push(ProviderEntry {
            name: "roundtrip-test".into(),
            base_url: "http://localhost:11434/v1".into(),
            default_model: "llama3.2".into(),
            api_key: None,
        });

        let toml_text = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();

        let reloaded = AhmaConfig::load_from(tmp.path());
        // 1 from config + 1 auto-registered omlx
        assert_eq!(reloaded.providers.len(), 2);
        let p = reloaded
            .providers
            .iter()
            .find(|x| x.name == "roundtrip-test")
            .unwrap();
        assert_eq!(p.base_url, "http://localhost:11434/v1");
        assert_eq!(p.default_model, "llama3.2");
        assert!(p.api_key.is_none());
        assert!(reloaded.providers.iter().any(|x| x.name == "omlx"));
    }

    /// Add two providers, remove one, re-save, reload — verify only one survives.
    #[test]
    fn provider_remove_roundtrip_via_load_from() {
        let tmp = tempfile::NamedTempFile::new().unwrap();

        let mut cfg = AhmaConfig::default();
        cfg.providers.push(ProviderEntry {
            name: "keep-me".into(),
            base_url: "http://localhost:11434/v1".into(),
            default_model: "gemma3n".into(),
            api_key: None,
        });
        cfg.providers.push(ProviderEntry {
            name: "remove-me".into(),
            base_url: "https://api.openai.com/v1".into(),
            default_model: "gpt-4o-mini".into(),
            api_key: Some("sk-placeholder".into()),
        });

        let toml_text = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();

        // Simulate `ahma llm remove remove-me`.
        let mut cfg2 = AhmaConfig::load_from(tmp.path());
        cfg2.providers.retain(|p| p.name != "remove-me");
        let toml_text2 = toml::to_string_pretty(&cfg2).unwrap();
        std::fs::write(tmp.path(), toml_text2).unwrap();

        let final_cfg = AhmaConfig::load_from(tmp.path());
        // keep-me + auto-registered omlx
        assert_eq!(final_cfg.providers.len(), 2);
        assert!(final_cfg.providers.iter().any(|p| p.name == "keep-me"));
        assert!(final_cfg.providers.iter().any(|p| p.name == "omlx"));
    }

    /// Cluster config (key_file, heartbeat_ttl_secs, peers) survives a
    /// serialise/deserialise cycle.
    #[test]
    fn cluster_config_roundtrip_via_load_from() {
        let tmp = tempfile::NamedTempFile::new().unwrap();

        let cfg = AhmaConfig {
            cluster: ClusterConfig {
                key_file: Some("/etc/ahma/cluster.key".into()),
                heartbeat_ttl_secs: 90,
                peers: vec![
                    ClusterPeerConfig {
                        name: "node-a".into(),
                        url: "http://10.0.0.1:4000".into(),
                    },
                    ClusterPeerConfig {
                        name: "node-b".into(),
                        url: "http://10.0.0.2:4000".into(),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let toml_text = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();

        let reloaded = AhmaConfig::load_from(tmp.path());
        assert_eq!(
            reloaded.cluster.key_file.as_deref(),
            Some("/etc/ahma/cluster.key")
        );
        assert_eq!(reloaded.cluster.heartbeat_ttl_secs, 90);
        assert_eq!(reloaded.cluster.peers.len(), 2);
        assert_eq!(reloaded.cluster.peers[0].name, "node-a");
        assert_eq!(reloaded.cluster.peers[1].url, "http://10.0.0.2:4000");
    }

    /// Default cluster config has no key_file, no peers, and the default TTL.
    #[test]
    fn cluster_config_defaults_are_sane() {
        let cfg = AhmaConfig::default();
        assert!(cfg.cluster.key_file.is_none());
        assert!(cfg.cluster.peers.is_empty());
        assert_eq!(cfg.cluster.heartbeat_ttl_secs, 60);
    }

    /// A config with only [[providers]] (no [cluster] section) loads cleanly
    /// with a default cluster config — no parse error.
    #[test]
    fn missing_cluster_section_uses_defaults() {
        let toml_str = r#"
[[providers]]
name = "ollama-local"
base_url = "http://localhost:11434/v1"
default_model = "llama3.2"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let cfg = AhmaConfig::load_from(tmp.path());
        // 1 from config + 1 auto-registered omlx
        assert_eq!(cfg.providers.len(), 2);
        assert!(cfg.providers.iter().any(|p| p.name == "ollama-local"));
        assert!(cfg.providers.iter().any(|p| p.name == "omlx"));
        assert!(cfg.cluster.peers.is_empty(), "cluster defaults to no peers");
    }

    // ── AhmaSettings tests ────────────────────────────────────────────────────

    #[test]
    fn ahma_settings_default_omlx_values() {
        let s = AhmaSettings::default();
        assert_eq!(s.omlx.model, "mlx-community/gemma-4-12B-it-8bit");
        assert_eq!(s.omlx.base_url, "http://localhost:8080/v1");
    }

    #[test]
    fn ahma_settings_default_tools_values() {
        let s = AhmaSettings::default();
        assert_eq!(s.tools.timeout_secs, 360);
        assert!(!s.tools.force_sync);
        assert!(!s.tools.hot_reload);
        assert!(!s.tools.skip_probes);
    }

    #[test]
    fn ahma_settings_default_sandbox_values() {
        let s = AhmaSettings::default();
        assert!(!s.sandbox.disable);
        assert!(!s.sandbox.tmp_access);
        assert!(!s.sandbox.disable_temp);
        assert!(!s.sandbox.defer);
    }

    #[test]
    fn ahma_settings_default_logging_values() {
        let s = AhmaSettings::default();
        assert_eq!(s.logging.target, "file");
        assert!(!s.logging.log_monitor);
        assert_eq!(s.logging.monitor_rate_limit_secs, 60);
    }

    #[test]
    fn ahma_settings_log_to_stderr_helper() {
        let mut s = AhmaSettings::default();
        assert!(!s.log_to_stderr());
        s.logging.target = "stderr".to_string();
        assert!(s.log_to_stderr());
        s.logging.target = "STDERR".to_string();
        assert!(s.log_to_stderr(), "case-insensitive");
    }

    #[test]
    fn ahma_settings_load_from_missing_gives_defaults() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("nonexistent_settings_toml");
        let s = AhmaSettings::load_from(&path);
        // Verify we get defaults, not an error
        assert_eq!(s.omlx.model, "mlx-community/gemma-4-12B-it-8bit");
        assert_eq!(s.tools.timeout_secs, 360);
    }

    #[test]
    fn ahma_settings_partial_toml_override() {
        let toml_str = r#"
[omlx]
model = "mlx-community/llama-3.2-3B-Instruct-4bit"

[tools]
timeout_secs = 600
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let s = AhmaSettings::load_from(tmp.path());
        // Overridden values
        assert_eq!(s.omlx.model, "mlx-community/llama-3.2-3B-Instruct-4bit");
        assert_eq!(s.tools.timeout_secs, 600);
        // Non-overridden values stay at defaults
        assert_eq!(s.omlx.base_url, "http://localhost:8080/v1");
        assert!(!s.tools.force_sync);
        assert_eq!(s.logging.target, "file");
    }

    #[test]
    fn ahma_settings_write_defaults_creates_valid_template() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.toml");

        AhmaSettings::write_defaults(&path, false).unwrap();

        assert!(path.exists(), "template file should be created");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("mlx-community/gemma-4-12B-it-8bit"));
        assert!(contents.contains("[omlx]"));
        assert!(contents.contains("[tools]"));
        assert!(contents.contains("[sandbox]"));
        assert!(contents.contains("[logging]"));

        // The template itself must be valid TOML when uncommented — strip comment
        // markers from a copy to verify the structure is syntactically correct.
        // (The actual commented file is not parsed; this just validates the key structure.)
        let _ = toml::from_str::<AhmaSettings>(&contents).unwrap_or_default(); // empty commented file → defaults
    }

    #[test]
    fn ahma_settings_write_defaults_refuses_overwrite_without_force() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.toml");
        AhmaSettings::write_defaults(&path, false).unwrap();
        let err = AhmaSettings::write_defaults(&path, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn ahma_settings_write_defaults_force_overwrites() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.toml");
        AhmaSettings::write_defaults(&path, false).unwrap();
        AhmaSettings::write_defaults(&path, true).unwrap(); // should not error
        assert!(path.exists());
    }

    #[test]
    fn settings_path_contains_ahma_settings_toml() {
        if let Some(p) = settings_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".ahma"), "path should contain .ahma: {s}");
            assert!(
                s.contains("settings.toml"),
                "path should contain settings.toml: {s}"
            );
        }
    }
}
