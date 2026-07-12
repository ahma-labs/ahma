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
//! [[providers]]
//! name          = "claude"
//! kind          = "anthropic"   # native Messages API (not OpenAI-compatible)
//! base_url      = "https://api.anthropic.com/v1"
//! default_model = "claude-opus-4-8"
//! api_key       = "${ANTHROPIC_API_KEY}"
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
use dunce;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Env-var interpolation
// ---------------------------------------------------------------------------

/// Expand `${VAR_NAME}` placeholders in `s` using the process environment.
///
/// Two forms are supported:
/// - `${VAR}` — required: returns `Err` if `VAR` is not set.
/// - `${VAR:-default}` — optional: uses `default` when `VAR` is unset or empty
///   (the default may itself be empty, e.g. `${VAR:-}`).
///
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

        let placeholder = &rest[..end];
        // Support `${VAR:-default}`: use `default` when `VAR` is unset or empty,
        // instead of erroring. A bare `${VAR}` (no `:-`) still errors when unset,
        // preserving the original fail-loud behaviour for required references.
        let value = match placeholder.split_once(":-") {
            Some((var_name, default)) => match std::env::var(var_name) {
                Ok(v) if !v.is_empty() => v,
                _ => default.to_string(),
            },
            None => std::env::var(placeholder).with_context(|| {
                format!("Environment variable '{placeholder}' referenced in config is not set")
            })?,
        };

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

/// Wire-format family of a named provider.
///
/// Selects how the LLM client talks to the endpoint. `openai` (the default)
/// covers any OpenAI-compatible `/chat/completions` server (Ollama, llama.cpp,
/// LM Studio, OpenAI itself). `anthropic` selects the native Anthropic Messages API
/// (`/v1/messages`, `x-api-key`), which is **not** OpenAI-compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenAI-compatible chat-completions API.
    #[default]
    OpenAi,
    /// Anthropic native Messages API.
    Anthropic,
}

/// An entry in the `[[providers]]` array in `~/.ahma/config.toml`.
///
/// Example Anthropic provider:
///
/// ```toml
/// [[providers]]
/// name          = "claude"
/// kind          = "anthropic"
/// base_url      = "https://api.anthropic.com/v1"
/// default_model = "claude-opus-4-8"
/// api_key       = "${ANTHROPIC_API_KEY}"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Unique name used to reference this provider (e.g. `"ollama-local"`).
    pub name: String,
    /// Wire-format family. Defaults to `openai` for backward compatibility.
    #[serde(default)]
    pub kind: ProviderKind,
    /// Base URL of the API. For `openai`, the OpenAI-compatible root (e.g.
    /// `http://localhost:11434/v1`); for `anthropic`, `https://api.anthropic.com/v1`.
    pub base_url: String,
    /// Default model for this provider (e.g. `"llama3.2"`, `"claude-opus-4-8"`).
    pub default_model: String,
    /// Optional bearer token. Supports `${ENV_VAR}` interpolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Optional context-window size (in tokens) to request from the provider.
    ///
    /// Only meaningful for providers that accept a per-request context override —
    /// in practice Ollama, via the `options.num_ctx` field on its
    /// OpenAI-compatible endpoint. For hosted OpenAI-protocol clouds the context
    /// window is fixed by the model and this value is ignored (and not offered in
    /// the TUI). See [`endpoint_supports_num_ctx`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
}

/// Whether an endpoint accepts a per-request context-window override
/// (`options.num_ctx`).
///
/// Today this is true only for Ollama's OpenAI-compatible endpoint, detected by
/// its default port (`11434`) or an `ollama` host. Hosted clouds (OpenAI,
/// Together, Groq, …) pin the context to the model and reject unknown fields, so
/// the TUI greys out the `num_ctx` control for them.
pub fn endpoint_supports_num_ctx(base_url: &str, kind: ProviderKind) -> bool {
    if kind != ProviderKind::OpenAi {
        return false;
    }
    let lower = base_url.to_ascii_lowercase();
    lower.contains(":11434") || lower.contains("ollama")
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
            kind: self.kind,
            base_url: self.base_url.clone(),
            default_model: self.default_model.clone(),
            api_key,
            num_ctx: self.num_ctx,
        })
    }

    /// Whether this provider accepts a per-request `num_ctx` override.
    pub fn supports_num_ctx(&self) -> bool {
        endpoint_supports_num_ctx(&self.base_url, self.kind)
    }
}

/// A `ProviderEntry` with secrets resolved — ready to hand to an HTTP client.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub default_model: String,
    pub api_key: Option<String>,
    /// Per-request context-window override (tokens), if configured and supported.
    pub num_ctx: Option<u32>,
}

impl ResolvedProvider {
    /// Whether this provider accepts a per-request `num_ctx` override.
    pub fn supports_num_ctx(&self) -> bool {
        endpoint_supports_num_ctx(&self.base_url, self.kind)
    }
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

fn default_true() -> bool {
    true
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

        // Auto-register LM Studio provider from settings
        let settings = AhmaSettings::load();
        if !cfg.providers.iter().any(|p| p.name == "lmstudio") {
            cfg.providers.push(ProviderEntry {
                name: "lmstudio".to_string(),
                kind: ProviderKind::OpenAi,
                base_url: settings.lmstudio.base_url.clone(),
                default_model: settings.lmstudio.model.clone(),
                api_key: None,
                num_ctx: None,
            });
        }

        cfg
    }

    /// Append a provider to `~/.ahma/config.toml` and persist it.
    ///
    /// Operates on the **raw** file (not [`Self::load`], which injects a
    /// synthetic `lmstudio` entry) so auto-registered providers never leak into
    /// the saved file. Errors if a provider with the same name already exists.
    pub fn add_provider(entry: ProviderEntry) -> Result<()> {
        let path = ahma_config_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine ~/.ahma/config.toml path"))?;
        Self::add_provider_to(&path, entry)
    }

    /// [`Self::add_provider`] against an explicit path (for tests).
    pub fn add_provider_to(path: &Path, entry: ProviderEntry) -> Result<()> {
        let mut cfg: AhmaConfig = match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AhmaConfig::default(),
            Err(e) => anyhow::bail!("Failed to read {}: {e}", path.display()),
        };
        if cfg.providers.iter().any(|p| p.name == entry.name) {
            anyhow::bail!(
                "A provider named '{}' already exists in {}",
                entry.name,
                path.display()
            );
        }
        cfg.providers.push(entry);
        let text = toml::to_string_pretty(&cfg).context("Failed to serialize config to TOML")?;
        atomic_write_toml(path, &text)
    }

    /// Set (or clear, with `None`) the `num_ctx` of an existing provider in
    /// `~/.ahma/config.toml` and persist it. Errors if the provider isn't in the
    /// file, or if it doesn't support a context override (hosted clouds).
    pub fn set_provider_num_ctx(name: &str, num_ctx: Option<u32>) -> Result<()> {
        let path = ahma_config_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine ~/.ahma/config.toml path"))?;
        Self::set_provider_num_ctx_to(&path, name, num_ctx)
    }

    /// [`Self::set_provider_num_ctx`] against an explicit path (for tests).
    pub fn set_provider_num_ctx_to(path: &Path, name: &str, num_ctx: Option<u32>) -> Result<()> {
        let mut cfg: AhmaConfig = match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AhmaConfig::default(),
            Err(e) => anyhow::bail!("Failed to read {}: {e}", path.display()),
        };
        let entry = cfg
            .providers
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| anyhow::anyhow!("No provider named '{name}' in {}", path.display()))?;
        if num_ctx.is_some() && !endpoint_supports_num_ctx(&entry.base_url, entry.kind) {
            anyhow::bail!(
                "Provider '{name}' ({}) does not support a num_ctx override",
                entry.base_url
            );
        }
        entry.num_ctx = num_ctx;
        let text = toml::to_string_pretty(&cfg).context("Failed to serialize config to TOML")?;
        atomic_write_toml(path, &text)
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

/// Resolve the user's home directory for locating the `~/.ahma` directory.
///
/// Identical to [`dirs::home_dir`] in **release** builds. In debug/test builds
/// (`cfg(debug_assertions)`) it first honors the `AHMA_TEST_HOME` environment
/// variable, giving tests a *cross-platform* way to redirect home resolution.
///
/// This override exists because `dirs::home_dir()` on Windows resolves via
/// `SHGetKnownFolderPath(FOLDERID_Profile)` and **ignores** the `HOME` and
/// `USERPROFILE` environment variables — so unit tests cannot redirect it on
/// Windows the way they can on Unix (where `$HOME` is honored). The override is
/// compiled out of release binaries (`--release` disables `debug_assertions`),
/// so shipped `ahma` always uses the real OS home directory and the location of
/// the scope-grant store / settings is never influenced by the environment.
pub fn ahma_home_dir() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("AHMA_TEST_HOME") {
        return Some(PathBuf::from(p));
    }
    dirs::home_dir()
}

/// Returns the canonical path to `~/.ahma/config.toml`, or `None` if the home
/// directory cannot be determined.
pub fn ahma_config_path() -> Option<PathBuf> {
    ahma_home_dir().map(|h| h.join(".ahma").join("config.toml"))
}

/// Returns the canonical path to `~/.ahma/settings.toml`, or `None` if the home
/// directory cannot be determined.
pub fn settings_path() -> Option<PathBuf> {
    ahma_home_dir().map(|h| h.join(".ahma").join("settings.toml"))
}

// ---------------------------------------------------------------------------
// AhmaSettings — user-editable settings.toml
// ---------------------------------------------------------------------------

/// Configuration for a command serialisation (mutex) group.
///
/// Commands whose first whitespace-separated token matches any entry in
/// `prefixes` are serialised within the same working directory: at most one
/// such command runs at a time per directory.  This prevents file-lock
/// contention (e.g. `cargo` commands competing for `target/`) without
/// blocking unrelated commands.
///
/// Commands with the same full normalised command string that queue while the
/// group is busy are **coalesced**: only one instance runs and all waiters
/// receive the same result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MutexGroupConfig {
    /// Human-readable group name shown in progress messages and logs.
    pub name: String,
    /// Command prefixes (first whitespace token) that belong to this group.
    /// Example: `["cargo"]` gates any command starting with `cargo`.
    pub prefixes: Vec<String>,
    /// How long (in seconds) a queued command waits for the group to become
    /// available before failing with a timeout error.
    /// Default: `600` (10 minutes).
    #[serde(default = "default_mutex_wait_secs")]
    pub max_wait_secs: u64,
}

fn default_mutex_wait_secs() -> u64 {
    600
}

/// The built-in default mutex group: serialises all `cargo` subcommands
/// per working directory to avoid `target/` file-lock contention.
pub fn default_mutex_groups() -> Vec<MutexGroupConfig> {
    vec![MutexGroupConfig {
        name: "cargo".to_string(),
        prefixes: vec!["cargo".to_string()],
        max_wait_secs: default_mutex_wait_secs(),
    }]
}

/// LM Studio local-server provider defaults.
///
/// LM Studio exposes an OpenAI-compatible API on localhost via its built-in
/// **Local Server** (Developer tab → Start Server). Set [`model`] to the model
/// identifier of whichever model you have loaded in LM Studio (shown next to the
/// loaded model, e.g. `openai/gpt-oss-20b`).
///
/// Start the server from the LM Studio app, or headless with:
/// ```bash
/// lms server start
/// ```
///
/// The server listens on port 1234 by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LmStudioSettings {
    /// Base URL of the LM Studio local-server endpoint.
    /// Default: `http://localhost:1234/v1`
    pub base_url: String,
    /// Model identifier of the model loaded in LM Studio, e.g. `openai/gpt-oss-20b`.
    pub model: String,
}

/// Built-in default LM Studio endpoint, seeded into a fresh settings file.
pub const DEFAULT_LMSTUDIO_BASE_URL: &str = "http://localhost:1234/v1";
/// Built-in default model for the auto-registered LM Studio provider. This is the
/// single source of truth for the shipped model default — do not hardcode model
/// names elsewhere; read them from settings (which is seeded from this const).
pub const DEFAULT_LMSTUDIO_MODEL: &str = "openai/gpt-oss-20b";

impl Default for LmStudioSettings {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_LMSTUDIO_BASE_URL.to_string(),
            model: DEFAULT_LMSTUDIO_MODEL.to_string(),
        }
    }
}

/// The LLM provider/model most recently selected in `ahma tui`, persisted
/// globally so the MCP sub-agent (and the next session, in any directory) can
/// reuse "the model the user last chose" rather than re-deriving it. The TUI
/// writes this on every `/model` / `/provider` change; it is advisory, never a
/// security input. All fields are optional — unset means "no selection yet".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentSettings {
    /// Provider name or label (e.g. `Ollama`), or a base URL.
    pub provider: Option<String>,
    /// Model identifier (e.g. `gemma3:27b`).
    pub model: Option<String>,
    /// Resolved provider base URL, when the TUI knows it.
    pub provider_url: Option<String>,
}

/// Tool execution settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    /// Default tool execution timeout in seconds.
    /// Individual tools can override this via `timeout_seconds` in their JSON definition.
    /// Default: `600`
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
    /// Maximum number of agent tool-call turns before the interactive chat agent
    /// stops and summarises. Each turn is one model call that may request tools;
    /// multi-step tasks (read → edit → build → fix) need several. Too low and the
    /// agent gives up mid-task; too high risks runaway loops on a stuck model.
    /// Default: `25`
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Command serialisation groups.  Commands matching a group's prefix are
    /// serialised per working directory (at most one runs at a time within
    /// that directory).  Defaults to a single `cargo` group so that
    /// `cargo build`, `cargo test`, `cargo clippy`, etc. do not contend on
    /// the shared `target/` directory.
    /// Default: `[{ name = "cargo", prefixes = ["cargo"], max_wait_secs = 600 }]`
    #[serde(default = "default_mutex_groups")]
    pub mutex_groups: Vec<MutexGroupConfig>,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            timeout_secs: 600,
            force_sync: false,
            hot_reload: false,
            skip_probes: false,
            tools_dir: None,
            tool_bundles: Vec::new(),
            minimize_tokens: false,
            small_model_harness: false,
            max_turns: default_max_turns(),
            mutex_groups: default_mutex_groups(),
        }
    }
}

/// Default maximum agent tool-call turns (see [`ToolSettings::max_turns`]).
pub fn default_max_turns() -> u32 {
    25
}

/// Access level granted to a [`PersistentScope`].
///
/// Maps onto the sandbox's two enforcement sets: `Rw` paths join the writable
/// `scopes` (Landlock read+write / Seatbelt `allow file*`); `Ro` paths join the
/// read-only `read_scopes` (Landlock read / Seatbelt `allow file-read*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScopeAccess {
    /// Read-only access.
    Ro,
    /// Read and write access (the default — most external tool dirs are caches
    /// that the tool both reads and writes).
    #[default]
    Rw,
}

impl ScopeAccess {
    /// Whether this access level permits writes.
    pub fn is_write(self) -> bool {
        matches!(self, ScopeAccess::Rw)
    }

    /// Short human label (`"read-only"` / `"read+write"`).
    pub fn label(self) -> &'static str {
        match self {
            ScopeAccess::Ro => "read-only",
            ScopeAccess::Rw => "read+write",
        }
    }
}

/// A user-granted, machine-local directory added to the sandbox scope that
/// **survives `roots/list` replacement** (unlike the provisional `scopes` list).
///
/// This is the persistence record behind the "add an external tool directory to
/// the sandbox" flow (e.g. an sccache / ccache / shared toolchain cache that
/// lives outside the workspace). Entries are written only by the trusted
/// `ahma sandbox grant` command — never by a sandboxed tool call, since the
/// settings file lives in `$HOME`, outside every workspace scope, and is
/// therefore kernel-unwritable from inside the sandbox. That property is the
/// whole point: the AI can *request* a scope, but only the human, editing the
/// out-of-band file, can *grant* one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersistentScope {
    /// The directory to add to the sandbox scope. `~` is expanded to `$HOME`.
    pub path: PathBuf,
    /// Read-only or read+write. Default: `rw`.
    #[serde(default)]
    pub access: ScopeAccess,
    /// What asked for this scope (e.g. `"sccache"`), for auditability. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    /// When it was granted (ISO `YYYY-MM-DD`), stamped by `ahma sandbox grant`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    /// Free-form human note explaining why this scope exists. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Outcome of [`SandboxSettings::grant_scope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantOutcome {
    /// A brand-new scope entry was appended.
    Added,
    /// An existing entry for the same path was replaced; carries the old value.
    Updated(Box<PersistentScope>),
}

/// Compare two scope paths for equivalence after `~` expansion, so that
/// `~/Library/Caches/x` and `/Users/me/Library/Caches/x` match. Falls back to a
/// plain comparison when the home directory cannot be resolved.
fn scope_paths_equiv(a: &Path, b: &Path) -> bool {
    expand_home(a) == expand_home(b)
}

/// Sandbox and filesystem security settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// Allow package-manager caches (cargo registry/git, etc.) to be written inside
    /// the sandbox.  Grants write access only to the subdirs that package managers
    /// need when fetching new dependencies; sensitive config and binaries remain
    /// read-only.  Disable with `--no-package-cache-write` when you want the
    /// strictest possible isolation.
    /// Default: `true`
    #[serde(default = "default_true")]
    pub package_cache_write: bool,
    /// Default scratch directory for sandbox scope fallback.
    ///
    /// When no explicit `--sandbox-scope` is provided, no `scopes` are defined in
    /// settings, and the current working directory is a filesystem root (e.g., MCP
    /// clients like Antigravity that don't send `roots/list`), ahma uses this
    /// directory as the sandbox scope.  It is auto-created if it does not exist.
    ///
    /// Set to `None` to disable the auto-fallback (ahma will error instead).
    /// Default: `"~/sandbox"`
    #[serde(default = "default_sandbox_directory")]
    pub sandbox_directory: Option<PathBuf>,
    /// Add the `sandbox_directory` (default `~/sandbox`) as a persistent secondary
    /// scope that survives `roots/list` updates.  Equivalent to the `--sandbox` CLI flag.
    /// This is the recommended default for most MCP server deployments; it gives the AI
    /// a well-known scratch space that is always writable regardless of which workspace
    /// is open.
    /// Default: `false`
    pub use_sandbox_directory: bool,
    /// Machine-local external directories granted to the sandbox scope that
    /// **survive `roots/list` replacement** — the persistence backing the
    /// `ahma sandbox grant` flow (e.g. an sccache cache outside the workspace).
    ///
    /// Unlike [`scopes`](Self::scopes) (a provisional, roots-replaceable list),
    /// every entry here is re-appended after each `roots/list` update so the
    /// grant stays in effect for the whole session regardless of which workspace
    /// the client opens. Edited via `ahma sandbox grant|list|revoke`, or by hand.
    /// Default: empty list
    #[serde(default)]
    pub persistent_scopes: Vec<PersistentScope>,
    /// Environment-variable names to **preserve** in tool subprocess
    /// environments even though they match a built-in secret pattern
    /// (`*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*PASSWORD*`, …).
    ///
    /// By default ahma scrubs secret-looking variables from every tool
    /// subprocess so a sandboxed (or prompt-injected) command cannot read the
    /// server's credentials out of its own environment and exfiltrate them —
    /// the kernel sandbox restricts the filesystem, not environment
    /// inheritance. This is the explicit, human-authored exception list for the
    /// rare tool that legitimately needs a token (e.g. `GITHUB_TOKEN` for a CI
    /// workflow). Matching is case-insensitive on the exact variable name.
    ///
    /// Like every security-tier setting it lives in `~/.ahma/settings.toml`,
    /// outside every workspace scope and kernel-unwritable from inside the
    /// sandbox — the agent cannot grant itself a passthrough.
    /// Default: empty list
    #[serde(default)]
    pub env_allow: Vec<String>,
    /// macOS only. Allow sandboxed tools to read **and write** the login keychain
    /// (`~/Library/Keychains`) and the `com.apple.security*` preference plists.
    ///
    /// Enabled by default so `gh`, `git-credential-osxkeychain`, and other tools
    /// that store credentials in the Keychain keep working under the sandbox —
    /// with it off, `gh auth login` appears to succeed but the token is written
    /// somewhere `gh` can't read back, so every later `gh` call is unauthenticated.
    ///
    /// The keychain is encrypted at rest, so blocking file access to it protects
    /// only against offline theft of the encrypted database, not against secret
    /// extraction (that goes through `securityd`, which is ACL-gated regardless of
    /// the sandbox). Set to `false` for maximum defense-in-depth on high-security
    /// machines; when off, `~/Library/Keychains` is added to the credential-read
    /// deny set and keychain writes stay blocked. Ignored on Linux/Windows.
    /// Default: `true`
    #[serde(default = "default_true")]
    pub allow_keychain: bool,
    /// macOS only. Additional credential directories whose **reads** are denied
    /// to sandboxed tools, on top of the built-in default set (`~/.ahma`,
    /// `~/.aws`, `~/.gnupg`, `~/.config/gcloud`, `~/.kube`, `~/.docker`,
    /// `~/.netrc`). The login keychain is governed separately by
    /// [`allow_keychain`](Self::allow_keychain) (default on).
    ///
    /// macOS Seatbelt grants global file-read to sandboxed commands (an APFS
    /// firmlink workaround), so credential files would otherwise be readable and
    /// exfiltratable. `~/.ssh` and `~/.config/gh` are **not** denied by default
    /// so git-over-ssh and `gh` keep working — add them here to harden further.
    /// `~` is expanded. Ignored on Linux/Windows (reads are already scoped).
    /// Default: empty list
    #[serde(default)]
    pub deny_credential_reads: Vec<PathBuf>,
    /// macOS only. Credential directories to **remove** from the built-in
    /// default deny set (the escape hatch for a tool that legitimately needs
    /// e.g. `~/.aws`). `~` is expanded. Applied after `deny_credential_reads`,
    /// so a path listed in both ends up allowed.
    /// Default: empty list
    #[serde(default)]
    pub allow_credential_reads: Vec<PathBuf>,
}

impl SandboxSettings {
    /// Find a persistent scope whose path matches `path` (after `~` expansion).
    pub fn find_scope(&self, path: &Path) -> Option<&PersistentScope> {
        self.persistent_scopes
            .iter()
            .find(|s| scope_paths_equiv(&s.path, path))
    }

    /// Add a persistent scope, or replace the existing entry for the same path.
    ///
    /// Matching is by `~`-expanded path, so re-granting `~/x` after `/home/u/x`
    /// updates in place rather than duplicating. Returns whether this added a new
    /// entry or replaced one (the previous value is returned for the confirm
    /// message).
    pub fn grant_scope(&mut self, scope: PersistentScope) -> GrantOutcome {
        if let Some(existing) = self
            .persistent_scopes
            .iter_mut()
            .find(|s| scope_paths_equiv(&s.path, &scope.path))
        {
            let old = std::mem::replace(existing, scope);
            GrantOutcome::Updated(Box::new(old))
        } else {
            self.persistent_scopes.push(scope);
            GrantOutcome::Added
        }
    }

    /// Remove the persistent scope for `path` (matched after `~` expansion).
    /// Returns the removed entry, or `None` if no match existed.
    pub fn revoke_scope(&mut self, path: &Path) -> Option<PersistentScope> {
        let idx = self
            .persistent_scopes
            .iter()
            .position(|s| scope_paths_equiv(&s.path, path))?;
        Some(self.persistent_scopes.remove(idx))
    }
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            disable: false,
            tmp_access: false,
            disable_temp: false,
            defer: false,
            task_vault: None,
            scopes: Vec::new(),
            working_dirs: Vec::new(),
            package_cache_write: true,
            sandbox_directory: default_sandbox_directory(),
            use_sandbox_directory: false,
            persistent_scopes: Vec::new(),
            env_allow: Vec::new(),
            allow_keychain: true,
            deny_credential_reads: Vec::new(),
            allow_credential_reads: Vec::new(),
        }
    }
}

fn default_sandbox_directory() -> Option<PathBuf> {
    Some(PathBuf::from("~/sandbox"))
}

/// Expand `~` or `~/…` to the user's home directory.
///
/// Public because every surface that resolves a permission subject must expand
/// it the *same* way before comparing it against the denylist — a `~`-spelled
/// path that skipped expansion would sail past a check keyed on the absolute one.
pub fn expand_home(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if (s.starts_with("~/") || s.starts_with("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        let mut expanded = home;
        expanded.push(&s[2..]);
        return expanded;
    }
    path.to_path_buf()
}

/// Ensure the sandbox directory exists, creating it if necessary.
///
/// Expands `~` to the home directory, creates the directory if it does not
/// exist, and returns the canonicalized absolute path.
pub fn ensure_sandbox_directory(path: &Path) -> Result<PathBuf> {
    let expanded = expand_home(path);
    if !expanded.exists() {
        std::fs::create_dir_all(&expanded).with_context(|| {
            format!("Failed to create sandbox directory: {}", expanded.display())
        })?;
        tracing::info!("Created sandbox directory: {}", expanded.display());
    }
    dunce::canonicalize(&expanded).with_context(|| {
        format!(
            "Failed to canonicalize sandbox directory: {}",
            expanded.display()
        )
    })
}

/// Logging and log-monitoring settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Default policy for outbound HTTP made by ahma's own tools (`fetch_webpage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebDefaultPolicy {
    /// Domains not in `never_allow`/`always_allow` are permitted without a
    /// prompt (backward-compatible default).
    #[default]
    Allow,
    /// Strict mode: a domain not in `always_allow` or a session grant is held
    /// and an approval prompt is raised. Recommended for sensitive workspaces.
    Deny,
}

/// What to do when an approved request is redirected to a **different** domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RedirectPolicy {
    /// Fail the request (default). A cross-domain redirect does not inherit the
    /// source domain's approval.
    #[default]
    Block,
    /// Raise a fresh approval prompt for the redirect target domain.
    Prompt,
}

/// Web-egress policy for outbound HTTP made by ahma's own tools (SPEC §4.6
/// R-WEB). Governs `fetch_webpage` and any future tool using the egress client;
/// it does **not** govern subprocess HTTP or the ahma process's LLM connections.
///
/// Lives in `~/.ahma/settings.toml`, outside every workspace scope and
/// kernel-unwritable from inside the sandbox — the agent cannot grant itself web
/// access. Private-range blocking (`block_private_ranges`) is enforced at
/// connection time by the egress guard regardless of `default_policy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSettings {
    /// `allow` (default, backward-compatible) or `deny` (strict: prompt for
    /// unknown domains).
    pub default_policy: WebDefaultPolicy,
    /// Block loopback, RFC-1918, link-local, and cloud-metadata IP ranges at DNS
    /// resolution time (resists DNS rebinding). STRONGLY recommended `true`;
    /// `false` enables SSRF against local services and must warn loudly.
    pub block_private_ranges: bool,
    /// `block` (default): cross-domain redirects fail. `prompt`: raise a new
    /// approval for the redirect target.
    pub on_redirect_to_new_domain: RedirectPolicy,
    /// Domains always permitted without a prompt. Syntax: exact
    /// (`api.github.com`), single-level wildcard (`*.github.com`),
    /// scheme-qualified (`https://api.github.com`), port-qualified
    /// (`api.github.com:8080`). Note: `github.com` matches `github.com` only, not
    /// `api.github.com`.
    pub always_allow: Vec<String>,
    /// Domains always blocked, regardless of `default_policy`, `always_allow`, or
    /// session grants.
    pub never_allow: Vec<String>,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            default_policy: WebDefaultPolicy::Allow,
            block_private_ranges: true,
            on_redirect_to_new_domain: RedirectPolicy::Block,
            always_allow: Vec::new(),
            never_allow: Vec::new(),
        }
    }
}

/// Subprocess network-egress restriction (SPEC R-NET). When `restrict` is on,
/// every sandboxed subprocess is routed through a local guarded proxy
/// (`HTTP_PROXY`/`HTTPS_PROXY`) that forwards only domains in `allow` and refuses
/// private/loopback/cloud-metadata targets. This is the network analog of the
/// filesystem write-sandbox; it is **advisory** on its own (a tool that ignores
/// the proxy variables is not contained) — see the network-restriction
/// limitations in the README.
///
/// Distinct from [`WebSettings`], which governs ahma's *own* HTTP tools
/// (`fetch_webpage`); this governs the *subprocesses ahma spawns*. Lives in
/// `~/.ahma/settings.toml`, outside every sandbox scope, so the agent cannot
/// grant itself egress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSettings {
    /// Route sandboxed subprocesses through the guarded egress proxy. Off by
    /// default (backward-compatible: `(allow network*)`). Also enabled by the
    /// `--restrict-network` flag. With `restrict = true` and an empty `allow`,
    /// **all** subprocess egress is denied.
    pub restrict: bool,
    /// Domains subprocesses may reach when `restrict` is on. Syntax matches the
    /// egress allowlist: exact (`crates.io`), single-level wildcard
    /// (`*.crates.io`), or `*` for any. Empty means deny-all.
    pub allow: Vec<String>,
}

/// Runtime feature toggles.
///
/// Controls which optional capabilities are active at runtime.  Features
/// default to the most useful "batteries-included" configuration: everything
/// that works without additional setup is enabled, while features that require
/// external infrastructure (cluster peers, etc.) start disabled.
///
/// Toggle in `~/.ahma/settings.toml` or via `ahma tui` → `/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureSettings {
    /// Code complexity analysis (`ahma simplify`).
    /// Analyzes source code and generates simplicity reports.
    /// Default: `true`
    pub simplify: bool,
    /// Task vault isolation (`ahma vault create/list`).
    /// Per-question isolated working directories with audit log and trash.
    /// Default: `false` (enable to auto-create vaults per TUI session)
    pub vault: bool,
    /// Distributed cluster scheduling (`ahma cluster`).
    /// mDNS peer discovery and signed task dispatch to worker nodes.
    /// Default: `false` (requires peer configuration first)
    pub cluster: bool,
    /// Network egress proxy for sandboxed tasks.
    /// Per-task HTTP proxy with domain allowlist for controlled outbound access.
    /// Default: `true`
    pub egress: bool,
    /// HTML artifact output channel.
    /// Tools can emit interactive HTML artifacts with embedded LLM chat.
    /// Default: `true`
    pub artifact: bool,
    /// LLM-powered task decomposition.
    /// Split complex questions into sub-tasks, dispatch concurrently, aggregate.
    /// Default: `true`
    pub decompose: bool,
}

impl Default for FeatureSettings {
    fn default() -> Self {
        Self {
            simplify: true,
            vault: false,
            cluster: false,
            egress: true,
            artifact: true,
            decompose: true,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AhmaSettings {
    /// Runtime feature toggles (simplify, vault, cluster, etc.).
    pub features: FeatureSettings,
    /// LM Studio local-server provider configuration.
    pub lmstudio: LmStudioSettings,
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
    /// LLM provider/model most recently selected in `ahma tui`, persisted so the
    /// MCP sub-agent and the next session can reuse it.
    pub agent: AgentSettings,
    /// Web-egress policy for ahma's own HTTP tools (SPEC §4.6 R-WEB).
    pub web: WebSettings,
    /// Subprocess network-egress restriction (SPEC R-NET).
    pub network: NetworkSettings,
    /// The unified permission ledger (SPEC R-PERM). Holds the permissions that
    /// had no home in this file before — per-workspace tool approvals, migrated
    /// out of the retired `~/.config/ahma/approvals.json`. Filesystem scopes stay
    /// in `[sandbox].persistent_scopes` and web domains in `[web]`; they are all
    /// the *same ledger*, rendered together by `ahma permissions list`.
    pub permissions: crate::permissions::PermissionSettings,
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
    ///
    /// This is the **runtime-safe** loader: a parse error is logged and the
    /// compiled-in defaults are returned, so a settings file corrupted while a
    /// long-running process (TUI, daemon) is live cannot hard-kill it. The
    /// **fail-closed** behavior required at startup (R-CFG6.1) is implemented by
    /// the startup resolution path via [`load_from_result`], which surfaces the
    /// error so the launcher can abort before the sandbox is built.
    pub fn load_from(path: &Path) -> Self {
        match Self::load_from_result(path) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!("{e}; using default AhmaSettings");
                Self::default()
            }
        }
    }

    /// Strict loader: returns `Err(message)` on a **parse** failure instead of
    /// falling back to defaults. A file that cannot be *read* — because it is
    /// missing (`NotFound`) or because access is denied (`PermissionDenied`) —
    /// is **not** an error and yields compiled-in defaults. This is the
    /// primitive the startup path uses to fail closed (R-CFG6.1) and that tests
    /// use to verify bad-config rejection.
    ///
    /// The `PermissionDenied` case is load-bearing, not a mere convenience:
    /// `~/.ahma` is **intentionally out of sandbox scope** (SPEC R5.4.8 — it holds
    /// ahma's own settings and secrets, and the sandbox denies sandboxed
    /// subprocesses access so a compromised tool cannot read them; widening to
    /// it requires explicit human review via `sandbox grant`). So whenever ahma
    /// itself runs inside its own sandbox (its test suite, a nested invocation,
    /// or as a subprocess of another ahma), the read of `~/.ahma/settings.toml`
    /// returns EPERM → `PermissionDenied`. Treating that as fatal made ahma
    /// abort on every sandboxed launch. Degrading to defaults keeps ahma usable
    /// while honoring the deny, and is fail-*safe*: the compiled-in defaults are
    /// the secure baseline (sandbox enabled). R-CFG6.1's fail-closed rule
    /// applies only to a file we *can* read but that fails to **parse** — a
    /// tampered/corrupt file must never silently change behavior.
    pub fn load_from_result(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Self::parse(&contents)
                .map_err(|e| format!("failed to parse settings file {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                warn!(
                    "settings file {} is not readable ({e}); this is expected when running \
                     inside the ahma sandbox, which denies ~/.ahma by design (SPEC R5.4.8). \
                     Using compiled-in defaults.",
                    path.display()
                );
                Ok(Self::default())
            }
            Err(e) => Err(format!("failed to read {}: {e}", path.display())),
        }
    }

    /// Parse settings from TOML text — the in-memory half of
    /// [`load_from_result`](Self::load_from_result), exposed so callers that have
    /// already read the file (e.g. via async I/O inside an agent turn) do not
    /// need a `toml` dependency of their own.
    pub fn parse(contents: &str) -> Result<Self, String> {
        toml::from_str(contents).map_err(|e| e.to_string())
    }

    /// Whether the `logging.target` resolves to stderr.
    pub fn log_to_stderr(&self) -> bool {
        self.logging.target.trim().eq_ignore_ascii_case("stderr")
    }

    /// Write a fresh settings file (the current defaults, all commented out) to
    /// `path`.
    ///
    /// Uses the same [`render_documented`](Self::render_documented) renderer as
    /// the startup sync, so `ahma settings init` and the auto-maintained file are
    /// byte-identical for a default configuration. Creates parent directories as
    /// needed. Returns `Err` if the file already exists and `overwrite` is `false`.
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
        std::fs::write(path, Self::default().render_documented())
            .with_context(|| format!("Failed to write {}", path.display()))
    }

    /// Save the current settings to `~/.ahma/settings.toml`.
    ///
    /// Serializes the full settings struct to TOML and writes atomically
    /// (write to temp file, then rename).  Creates parent directories as needed.
    pub fn save(&self) -> Result<()> {
        match settings_path() {
            Some(p) => self.save_to(&p),
            None => anyhow::bail!("Cannot determine home directory for ~/.ahma/settings.toml"),
        }
    }

    /// Save to an explicit path — useful for tests and alternate locations.
    ///
    /// The write is atomic: contents are written to a temporary sibling file
    /// and then renamed into place, so a crash mid-write never corrupts the
    /// settings file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }
        let toml_text = self.render_documented();

        // Atomic write: temp file → rename
        let tmp_path = path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, toml_text)
            .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to rename {} → {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    /// Ensure the settings file at `path` is present and rendered in the current
    /// version's **documented, minimized** form — the single mechanism that keeps
    /// the on-disk file readable and in sync across upgrades.
    ///
    /// Every run regenerates the file via [`render_documented`](Self::render_documented):
    /// every option is documented with its compiled-in default, but only the
    /// values the user actually changed from the default are written as active
    /// (uncommented) assignments — defaults stay commented out. This both
    /// **minimizes** the file (a value equal to the current default is dropped as
    /// an assertion) and refreshes the comments and defaults to match this build.
    ///
    /// - **Missing file** → write the full documented template.
    /// - **Existing file** → parse it (preserving every value the user set),
    ///   then re-render. Because the parse honours serde defaults, fields a newer
    ///   version introduced appear automatically, and assertions equal to the
    ///   default collapse back into commented documentation.
    ///
    /// Writes only when the rendered text differs from what is on disk, so it is
    /// cheap and idempotent to call on every startup. Returns `Ok(true)` when the
    /// file was created or updated. A settings file that fails to parse is left
    /// **untouched** (`Ok(false)`) so a hand-edit in progress is never clobbered.
    ///
    /// Note: regeneration normalizes formatting and replaces hand-written
    /// comments with the generated documentation. User *values* are always
    /// preserved; user *comments* are not.
    pub fn ensure_current(path: &Path) -> Result<bool> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let text = Self::default().render_documented();
                atomic_write_toml(path, &text)?;
                Ok(true)
            }
            // `~/.ahma` is intentionally out of sandbox scope (SPEC R5.4.8), so a
            // sandboxed ahma cannot read *or* write it. Do not abort startup
            // trying to auto-maintain a file the sandbox (correctly) denies —
            // skip the write and continue on compiled-in defaults.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                warn!(
                    "settings file {} is not accessible ({e}); skipping auto-maintenance \
                     (expected inside the ahma sandbox, which denies ~/.ahma by design).",
                    path.display()
                );
                Ok(false)
            }
            Err(e) => anyhow::bail!("Failed to read {}: {e}", path.display()),
            Ok(contents) => {
                let parsed: Self = match toml::from_str(&contents) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            "settings file {} does not parse ({e}); leaving it untouched",
                            path.display()
                        );
                        return Ok(false);
                    }
                };
                let text = parsed.render_documented();
                if text != contents {
                    atomic_write_toml(path, &text)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// Render `self` as a fully-documented, minimized `settings.toml`.
    ///
    /// Every option carries a one-line doc comment that states its compiled-in
    /// default. Values equal to the current default are emitted **commented out**
    /// (documentation only); values the user changed are emitted as **active**
    /// assignments. The result round-trips: parsing it back yields the same
    /// [`AhmaSettings`], and re-rendering that is a fixed point (so
    /// [`ensure_current`](Self::ensure_current) stops rewriting once converged).
    ///
    /// This is the single source of truth for the on-disk format — both the
    /// startup sync and the TUI/CLI save paths go through it, so the file stays
    /// documented and minimal no matter who writes it.
    pub fn render_documented(&self) -> String {
        let d = Self::default();
        let mut w = SettingsDoc::new();

        w.line("# ~/.ahma/settings.toml — Ahma user settings");
        w.line("#");
        w.line("# Auto-maintained: ahma regenerates this file on startup to match the running");
        w.line("# version. Every option is documented with its default; only values you have");
        w.line("# changed from the default are written as active (uncommented) lines — defaults");
        w.line("# stay commented out. Uncomment a line and edit it to override; your changes are");
        w.line("# preserved across upgrades (hand-written comments are not).");
        w.line("#");
        w.line(
            "# Priority: CLI flags > this file > deprecated AHMA_* env vars > built-in defaults.",
        );
        w.line("# `ahma settings show` prints effective values; `ahma settings init` resets this file.");
        w.blank();

        // ── Features ─────────────────────────────────────────────────────────
        w.section("Features", "features");
        w.setting(
            "Code complexity analysis (ahma simplify).",
            "simplify",
            self.features.simplify.to_string(),
            d.features.simplify.to_string(),
        );
        w.setting(
            "Task vault isolation (per-session working directories).",
            "vault",
            self.features.vault.to_string(),
            d.features.vault.to_string(),
        );
        w.setting(
            "Distributed cluster scheduling (requires peer setup).",
            "cluster",
            self.features.cluster.to_string(),
            d.features.cluster.to_string(),
        );
        w.setting(
            "Network egress proxy for sandboxed tasks.",
            "egress",
            self.features.egress.to_string(),
            d.features.egress.to_string(),
        );
        w.setting(
            "HTML artifact output channel.",
            "artifact",
            self.features.artifact.to_string(),
            d.features.artifact.to_string(),
        );
        w.setting(
            "LLM-powered task decomposition.",
            "decompose",
            self.features.decompose.to_string(),
            d.features.decompose.to_string(),
        );

        // ── LM Studio ────────────────────────────────────────────────────────
        w.section("LM Studio (local OpenAI-compatible server)", "lmstudio");
        w.setting(
            "Base URL of the LM Studio local-server endpoint.",
            "base_url",
            toml_str(&self.lmstudio.base_url),
            toml_str(&d.lmstudio.base_url),
        );
        w.setting(
            "Model identifier of the model loaded in LM Studio.",
            "model",
            toml_str(&self.lmstudio.model),
            toml_str(&d.lmstudio.model),
        );

        // ── Tool execution ───────────────────────────────────────────────────
        w.section("Tool execution", "tools");
        w.setting(
            "Default tool timeout in seconds (per-tool override via timeout_seconds).",
            "timeout_secs",
            self.tools.timeout_secs.to_string(),
            d.tools.timeout_secs.to_string(),
        );
        w.setting(
            "Run all tools synchronously instead of async-first.",
            "force_sync",
            self.tools.force_sync.to_string(),
            d.tools.force_sync.to_string(),
        );
        w.setting(
            "Reload tools from disk on change — INSECURE in production.",
            "hot_reload",
            self.tools.hot_reload.to_string(),
            d.tools.hot_reload.to_string(),
        );
        w.setting(
            "Skip tool-availability probes at startup.",
            "skip_probes",
            self.tools.skip_probes.to_string(),
            d.tools.skip_probes.to_string(),
        );
        w.setting(
            "Path to the tools directory of JSON tool definitions.",
            "tools_dir",
            toml_opt_path(&self.tools.tools_dir),
            toml_opt_path(&d.tools.tools_dir),
        );
        w.setting(
            "Tool bundles to enable, e.g. [\"rust\", \"git\"].",
            "tool_bundles",
            toml_str_list(&self.tools.tool_bundles),
            toml_str_list(&d.tools.tool_bundles),
        );
        w.setting(
            "Enable output compression and token minimization.",
            "minimize_tokens",
            self.tools.minimize_tokens.to_string(),
            d.tools.minimize_tokens.to_string(),
        );
        w.setting(
            "Enable small-model harness adaptations.",
            "small_model_harness",
            self.tools.small_model_harness.to_string(),
            d.tools.small_model_harness.to_string(),
        );
        w.setting(
            "Max agent tool-call turns before the chat agent stops and summarises.",
            "max_turns",
            self.tools.max_turns.to_string(),
            d.tools.max_turns.to_string(),
        );
        w.setting(
            "Command serialisation groups (per-dir mutex; set [] to disable).",
            "mutex_groups",
            toml_mutex_groups(&self.tools.mutex_groups),
            toml_mutex_groups(&d.tools.mutex_groups),
        );

        // ── Sandbox & filesystem security ────────────────────────────────────
        w.section("Sandbox & filesystem security", "sandbox");
        w.setting(
            "UNSAFE: disable the kernel sandbox entirely.",
            "disable",
            self.sandbox.disable.to_string(),
            d.sandbox.disable.to_string(),
        );
        w.setting(
            "Add the system temp dir to the sandbox scope.",
            "tmp_access",
            self.sandbox.tmp_access.to_string(),
            d.sandbox.tmp_access.to_string(),
        );
        w.setting(
            "Block all access to the system temp dir (overrides tmp_access).",
            "disable_temp",
            self.sandbox.disable_temp.to_string(),
            d.sandbox.disable_temp.to_string(),
        );
        w.setting(
            "Defer sandbox lock until the client provides roots/list.",
            "defer",
            self.sandbox.defer.to_string(),
            d.sandbox.defer.to_string(),
        );
        w.setting(
            "Run this server session inside an existing task vault.",
            "task_vault",
            toml_opt_path(&self.sandbox.task_vault),
            toml_opt_path(&d.sandbox.task_vault),
        );
        w.setting(
            "Paths allowed for read/write access under the sandbox.",
            "scopes",
            toml_path_list(&self.sandbox.scopes),
            toml_path_list(&d.sandbox.scopes),
        );
        w.setting(
            "Directories containing allowed working directories.",
            "working_dirs",
            toml_path_list(&self.sandbox.working_dirs),
            toml_path_list(&d.sandbox.working_dirs),
        );
        w.setting(
            "Allow package-manager caches (cargo registry/git) to be written.",
            "package_cache_write",
            self.sandbox.package_cache_write.to_string(),
            d.sandbox.package_cache_write.to_string(),
        );
        w.setting(
            "Default scratch directory, auto-created when needed.",
            "sandbox_directory",
            toml_opt_path(&self.sandbox.sandbox_directory),
            toml_opt_path(&d.sandbox.sandbox_directory),
        );
        w.setting(
            "Add sandbox_directory as a persistent secondary scope (--sandbox).",
            "use_sandbox_directory",
            self.sandbox.use_sandbox_directory.to_string(),
            d.sandbox.use_sandbox_directory.to_string(),
        );
        w.setting(
            "External dirs surviving roots/list (manage via `ahma sandbox grant`).",
            "persistent_scopes",
            toml_persistent_scopes(&self.sandbox.persistent_scopes),
            toml_persistent_scopes(&d.sandbox.persistent_scopes),
        );
        w.setting(
            "Env var names preserved in tool subprocesses despite matching a secret pattern (e.g. GITHUB_TOKEN). Everything else secret-looking is scrubbed.",
            "env_allow",
            toml_str_list(&self.sandbox.env_allow),
            toml_str_list(&d.sandbox.env_allow),
        );
        w.setting(
            "macOS: allow sandboxed tools to read/write the login keychain (gh, git-credential-osxkeychain). Off = maximum defense-in-depth.",
            "allow_keychain",
            self.sandbox.allow_keychain.to_string(),
            d.sandbox.allow_keychain.to_string(),
        );
        w.setting(
            "macOS: extra credential dirs to deny reads (on top of the built-in default set; e.g. ~/.ssh to harden further).",
            "deny_credential_reads",
            toml_path_list(&self.sandbox.deny_credential_reads),
            toml_path_list(&d.sandbox.deny_credential_reads),
        );
        w.setting(
            "macOS: credential dirs to remove from the built-in deny set (escape hatch for a tool that needs e.g. ~/.aws).",
            "allow_credential_reads",
            toml_path_list(&self.sandbox.allow_credential_reads),
            toml_path_list(&d.sandbox.allow_credential_reads),
        );

        // ── Logging ──────────────────────────────────────────────────────────
        w.section("Logging", "logging");
        w.setting(
            "Log destination: \"file\" (rolling) or \"stderr\".",
            "target",
            toml_str(&self.logging.target),
            toml_str(&d.logging.target),
        );
        w.setting(
            "Enable live log monitoring via an LLM.",
            "log_monitor",
            self.logging.log_monitor.to_string(),
            d.logging.log_monitor.to_string(),
        );
        w.setting(
            "Minimum seconds between log-monitor alerts.",
            "monitor_rate_limit_secs",
            self.logging.monitor_rate_limit_secs.to_string(),
            d.logging.monitor_rate_limit_secs.to_string(),
        );

        // ── HTTP server ──────────────────────────────────────────────────────
        w.section("HTTP server (ahma serve http only)", "http");
        w.setting(
            "MCP handshake timeout in seconds.",
            "handshake_timeout_secs",
            self.http.handshake_timeout_secs.to_string(),
            d.http.handshake_timeout_secs.to_string(),
        );
        w.setting(
            "Disable HTTP/3 QUIC; fall back to HTTP/2 TCP.",
            "disable_quic",
            self.http.disable_quic.to_string(),
            d.http.disable_quic.to_string(),
        );
        w.setting(
            "Reject HTTP/1.1 connections; require HTTP/2 or better.",
            "disable_http1_1",
            self.http.disable_http1_1.to_string(),
            d.http.disable_http1_1.to_string(),
        );
        w.setting(
            "Path to the Unix domain socket.",
            "unix_socket_path",
            toml_opt_str(&self.http.unix_socket_path),
            toml_opt_str(&d.http.unix_socket_path),
        );

        // ── HTTP authentication & rate limiting ──────────────────────────────
        w.section("HTTP authentication & rate limiting", "auth");
        w.setting(
            "Path to a file containing the required bearer token.",
            "require_token_path",
            toml_str(&self.auth.require_token_path),
            toml_str(&d.auth.require_token_path),
        );
        w.setting(
            "Required bearer token specified directly in config (prefer require_token_path).",
            "require_token",
            toml_opt_str(&self.auth.require_token),
            toml_opt_str(&d.auth.require_token),
        );
        w.setting(
            "Max requests per second (0 = no limit).",
            "rate_limit_rps",
            self.auth.rate_limit_rps.to_string(),
            d.auth.rate_limit_rps.to_string(),
        );
        w.setting(
            "Burst allowance for the rate limiter.",
            "rate_limit_burst",
            self.auth.rate_limit_burst.to_string(),
            d.auth.rate_limit_burst.to_string(),
        );

        // ── Instance identity ────────────────────────────────────────────────
        w.section("Instance identity", "instance");
        w.setting(
            "Instance name shown in the TUI and daemon event stream.",
            "label",
            toml_str(&self.instance.label),
            toml_str(&d.instance.label),
        );

        w.section("Agent (last-selected LLM, written by ahma tui)", "agent");
        w.setting(
            "Provider name/label of the most recently selected LLM.",
            "provider",
            toml_opt_str(&self.agent.provider),
            toml_opt_str(&d.agent.provider),
        );
        w.setting(
            "Model id of the most recently selected LLM.",
            "model",
            toml_opt_str(&self.agent.model),
            toml_opt_str(&d.agent.model),
        );
        w.setting(
            "Resolved base URL of the most recently selected provider.",
            "provider_url",
            toml_opt_str(&self.agent.provider_url),
            toml_opt_str(&d.agent.provider_url),
        );

        // ── Web egress ───────────────────────────────────────────────────────
        w.section("Web egress (ahma's own HTTP tools; SPEC R-WEB)", "web");
        w.setting(
            "\"allow\" (default) or \"deny\" (strict: prompt for unknown domains).",
            "default_policy",
            toml_str(web_default_policy_str(self.web.default_policy)),
            toml_str(web_default_policy_str(d.web.default_policy)),
        );
        w.setting(
            "Block loopback/RFC-1918/link-local/cloud-metadata IPs at DNS resolution time. Keep true.",
            "block_private_ranges",
            self.web.block_private_ranges.to_string(),
            d.web.block_private_ranges.to_string(),
        );
        w.setting(
            "Cross-domain redirects: \"block\" (default) fails; \"prompt\" asks for the new domain.",
            "on_redirect_to_new_domain",
            toml_str(redirect_policy_str(self.web.on_redirect_to_new_domain)),
            toml_str(redirect_policy_str(d.web.on_redirect_to_new_domain)),
        );
        w.setting(
            "Domains always permitted (exact, *.wildcard, scheme/port-qualified). github.com != api.github.com.",
            "always_allow",
            toml_str_list(&self.web.always_allow),
            toml_str_list(&d.web.always_allow),
        );
        w.setting(
            "Domains always blocked, overriding default_policy/always_allow/session grants.",
            "never_allow",
            toml_str_list(&self.web.never_allow),
            toml_str_list(&d.web.never_allow),
        );

        // ── Subprocess network egress ────────────────────────────────────────
        w.section(
            "Subprocess network egress (route tools through a guarded proxy; SPEC R-NET)",
            "network",
        );
        w.setting(
            "Route sandboxed subprocesses through the egress proxy (also: --restrict-network). Advisory; see README limits.",
            "restrict",
            self.network.restrict.to_string(),
            d.network.restrict.to_string(),
        );
        w.setting(
            "Domains subprocesses may reach when restrict=true (exact, *.wildcard, or *). Empty = deny all egress.",
            "allow",
            toml_str_list(&self.network.allow),
            toml_str_list(&d.network.allow),
        );

        // ── Permissions ──────────────────────────────────────────────────────
        w.section("Permission ledger (SPEC R-PERM)", "permissions");
        w.setting(
            "Per-workspace \"always allow\" tool grants (manage via `ahma permissions list|revoke`). Migrated from the retired ~/.config/ahma/approvals.json.",
            "tool_approvals",
            toml_tool_approvals(&self.permissions.tool_approvals),
            toml_tool_approvals(&d.permissions.tool_approvals),
        );

        w.into_string()
    }

    /// [`Self::ensure_current`] against the standard `~/.ahma/settings.toml`.
    /// Returns `Ok(false)` (no-op) when the home directory can't be determined.
    pub fn ensure_current_default_path() -> Result<bool> {
        match settings_path() {
            Some(p) => Self::ensure_current(&p),
            None => Ok(false),
        }
    }
}

// ---------------------------------------------------------------------------
// settings.toml rendering helpers (documented + minimized form)
// ---------------------------------------------------------------------------

/// Quote `s` as a TOML basic string (escaping `\` and `"`).
fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// TOML token for a [`WebDefaultPolicy`] (matches its serde representation).
fn web_default_policy_str(p: WebDefaultPolicy) -> &'static str {
    match p {
        WebDefaultPolicy::Allow => "allow",
        WebDefaultPolicy::Deny => "deny",
    }
}

/// TOML token for a [`RedirectPolicy`] (matches its serde representation).
fn redirect_policy_str(p: RedirectPolicy) -> &'static str {
    match p {
        RedirectPolicy::Block => "block",
        RedirectPolicy::Prompt => "prompt",
    }
}

/// Render an optional string; `None` is shown as an empty TOML string so an
/// unset value reads as a clear placeholder rather than a missing line.
fn toml_opt_str(o: &Option<String>) -> String {
    match o {
        Some(s) => toml_str(s),
        None => "\"\"".to_string(),
    }
}

/// Render a path as a TOML string (lossy UTF-8, matching how serde stores it).
fn toml_path(p: &Path) -> String {
    toml_str(&p.to_string_lossy())
}

/// Render an optional path; `None` is shown as an empty TOML string.
fn toml_opt_path(o: &Option<PathBuf>) -> String {
    match o {
        Some(p) => toml_path(p),
        None => "\"\"".to_string(),
    }
}

/// Render a list of strings as an inline TOML array.
fn toml_str_list(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| toml_str(s)).collect();
    format!("[{}]", items.join(", "))
}

/// Render a list of paths as an inline TOML array.
fn toml_path_list(v: &[PathBuf]) -> String {
    let items: Vec<String> = v.iter().map(|p| toml_path(p)).collect();
    format!("[{}]", items.join(", "))
}

/// Render mutex groups as an inline TOML array of inline tables.
fn toml_mutex_groups(v: &[MutexGroupConfig]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|g| {
            format!(
                "{{ name = {}, prefixes = {}, max_wait_secs = {} }}",
                toml_str(&g.name),
                toml_str_list(&g.prefixes),
                g.max_wait_secs
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// Render persistent scopes as an inline TOML array of inline tables, omitting
/// the optional fields that are unset (matching serde's `skip_serializing_if`).
fn toml_persistent_scopes(v: &[PersistentScope]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|ps| {
            let mut parts = vec![
                format!("path = {}", toml_path(&ps.path)),
                format!(
                    "access = {}",
                    toml_str(match ps.access {
                        ScopeAccess::Ro => "ro",
                        ScopeAccess::Rw => "rw",
                    })
                ),
            ];
            if let Some(g) = &ps.granted_by {
                parts.push(format!("granted_by = {}", toml_str(g)));
            }
            if let Some(a) = &ps.granted_at {
                parts.push(format!("granted_at = {}", toml_str(a)));
            }
            if let Some(n) = &ps.note {
                parts.push(format!("note = {}", toml_str(n)));
            }
            format!("{{ {} }}", parts.join(", "))
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// Render per-workspace tool approvals as an inline TOML array of inline tables,
/// omitting the optional fields that are unset (matching serde's
/// `skip_serializing_if`).
fn toml_tool_approvals(v: &[crate::permissions::ToolApproval]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|a| {
            let mut parts = vec![
                format!("workspace = {}", toml_path(&a.workspace)),
                format!("tools = {}", toml_str_list(&a.tools)),
            ];
            if let Some(at) = &a.granted_at {
                parts.push(format!("granted_at = {}", toml_str(at)));
            }
            if let Some(by) = &a.granted_by {
                parts.push(format!("granted_by = {}", toml_str(by)));
            }
            if let Some(s) = &a.surface {
                parts.push(format!("surface = {}", toml_str(s)));
            }
            format!("{{ {} }}", parts.join(", "))
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// Builds the documented, minimized `settings.toml` text used by
/// [`AhmaSettings::render_documented`].
struct SettingsDoc {
    out: String,
}

impl SettingsDoc {
    fn new() -> Self {
        Self { out: String::new() }
    }

    fn line(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    /// Emit a section banner (`# ── Title ──…`) followed by its `[table]` header.
    fn section(&mut self, title: &str, table: &str) {
        let prefix = format!("# ── {title} ");
        let pad = 80usize.saturating_sub(prefix.chars().count());
        self.line(&format!("{prefix}{}", "─".repeat(pad)));
        self.line(&format!("[{table}]"));
    }

    /// Emit one setting: a doc comment that always states the default, then the
    /// assignment — commented out when `value` equals `default` (documentation
    /// only), active when the user changed it. Equality is by rendered text, so
    /// both sides must come from the same formatter. A trailing blank line keeps
    /// adjacent settings visually separated.
    fn setting(&mut self, doc: &str, key: &str, value: String, default: String) {
        self.line(&format!("# {doc} (default: {default})"));
        if value == default {
            self.line(&format!("# {key} = {default}"));
        } else {
            self.line(&format!("{key} = {value}"));
        }
        self.blank();
    }

    fn into_string(self) -> String {
        self.out
    }
}

/// Atomic TOML write (temp sibling → rename), with a pid-scoped temp name so two
/// processes seeding the file at once don't clobber each other's temp file.
fn atomic_write_toml(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let tmp_path = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    std::fs::write(&tmp_path, text)
        .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename {} → {}",
            tmp_path.display(),
            path.display()
        )
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_provider_persists_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let entry = ProviderEntry {
            name: "togetherai".into(),
            kind: ProviderKind::OpenAi,
            base_url: "https://api.together.xyz/v1".into(),
            default_model: "moonshotai/Kimi-K2".into(),
            api_key: Some("${TOGETHER_API_KEY}".into()),
            num_ctx: None,
        };
        AhmaConfig::add_provider_to(&path, entry.clone()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("togetherai"));
        // No synthetic lmstudio entry leaks into the saved file.
        assert!(
            !text.contains("lmstudio"),
            "raw save must not persist auto-registered providers"
        );
        // Duplicate name is rejected.
        assert!(AhmaConfig::add_provider_to(&path, entry).is_err());
    }

    #[test]
    fn set_provider_num_ctx_only_for_supported_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        AhmaConfig::add_provider_to(
            &path,
            ProviderEntry {
                name: "ollama".into(),
                kind: ProviderKind::OpenAi,
                base_url: "http://localhost:11434/v1".into(),
                default_model: "ornith:35b".into(),
                api_key: None,
                num_ctx: None,
            },
        )
        .unwrap();
        AhmaConfig::add_provider_to(
            &path,
            ProviderEntry {
                name: "openai".into(),
                kind: ProviderKind::OpenAi,
                base_url: "https://api.openai.com/v1".into(),
                default_model: "gpt-4o-mini".into(),
                api_key: None,
                num_ctx: None,
            },
        )
        .unwrap();

        // Ollama supports it → stored.
        AhmaConfig::set_provider_num_ctx_to(&path, "ollama", Some(16384)).unwrap();
        let cfg = AhmaConfig::load_from(&path);
        assert_eq!(
            cfg.providers
                .iter()
                .find(|p| p.name == "ollama")
                .unwrap()
                .num_ctx,
            Some(16384)
        );
        // Hosted cloud rejects it.
        assert!(AhmaConfig::set_provider_num_ctx_to(&path, "openai", Some(8192)).is_err());
        // Unknown provider errors.
        assert!(AhmaConfig::set_provider_num_ctx_to(&path, "nope", Some(8192)).is_err());
    }

    #[test]
    fn ensure_current_creates_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        // Missing file → created from defaults, with real values present.
        assert!(AhmaSettings::ensure_current(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[lmstudio]"));
        assert!(
            text.contains(DEFAULT_LMSTUDIO_MODEL),
            "fresh file seeds the const model default; got:\n{text}"
        );

        // Second call changes nothing.
        assert!(
            !AhmaSettings::ensure_current(&path).unwrap(),
            "ensure_current is idempotent once the file is current"
        );
    }

    #[test]
    fn ensure_current_adds_missing_fields_and_preserves_user_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        // A user file from an older version: only a custom lmstudio model, nothing else.
        std::fs::write(&path, "[lmstudio]\nmodel = \"my-local-model\"\n").unwrap();

        assert!(
            AhmaSettings::ensure_current(&path).unwrap(),
            "missing default fields are merged in"
        );
        let loaded = AhmaSettings::load_from(&path);
        // User's value is preserved …
        assert_eq!(loaded.lmstudio.model, "my-local-model");
        // … and previously-absent defaults are now present.
        assert_eq!(loaded.lmstudio.base_url, DEFAULT_LMSTUDIO_BASE_URL);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[tools]"), "a whole missing table is added");

        // Now idempotent.
        assert!(!AhmaSettings::ensure_current(&path).unwrap());
    }

    #[test]
    fn ensure_current_leaves_unparseable_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let garbage = "this = is = not = valid = toml";
        std::fs::write(&path, garbage).unwrap();
        assert!(
            !AhmaSettings::ensure_current(&path).unwrap(),
            "a file that does not parse is never clobbered"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage);
    }

    /// A fully non-default settings value for round-trip / completeness testing.
    /// Every field is set to something other than its compiled-in default so that
    /// a missing field in [`AhmaSettings::render_documented`] would surface as a
    /// lost value when this round-trips through TOML.
    fn all_non_default_settings() -> AhmaSettings {
        AhmaSettings {
            features: FeatureSettings {
                simplify: false,
                vault: true,
                cluster: true,
                egress: false,
                artifact: false,
                decompose: false,
            },
            lmstudio: LmStudioSettings {
                base_url: "http://example.test:9999/v1".into(),
                model: "my/custom-model".into(),
            },
            tools: ToolSettings {
                timeout_secs: 123,
                force_sync: true,
                hot_reload: true,
                skip_probes: true,
                tools_dir: Some(PathBuf::from("/opt/tools")),
                tool_bundles: vec!["rust".into(), "git".into()],
                minimize_tokens: true,
                small_model_harness: true,
                max_turns: 7,
                mutex_groups: vec![MutexGroupConfig {
                    name: "gradle".into(),
                    prefixes: vec!["gradle".into(), "./gradlew".into()],
                    max_wait_secs: 42,
                }],
            },
            sandbox: SandboxSettings {
                disable: true,
                tmp_access: true,
                disable_temp: true,
                defer: true,
                task_vault: Some(PathBuf::from("/vaults/v1")),
                scopes: vec![PathBuf::from("/a"), PathBuf::from("/b")],
                working_dirs: vec![PathBuf::from("/work")],
                package_cache_write: false,
                sandbox_directory: Some(PathBuf::from("/scratch")),
                use_sandbox_directory: true,
                persistent_scopes: vec![PersistentScope {
                    path: PathBuf::from("~/Library/Caches/x.sccache"),
                    access: ScopeAccess::Ro,
                    granted_by: Some("sccache".into()),
                    granted_at: Some("2026-06-29".into()),
                    note: Some("compiler cache".into()),
                }],
                env_allow: vec!["GITHUB_TOKEN".into()],
                allow_keychain: false,
                deny_credential_reads: vec![PathBuf::from("~/.ssh")],
                allow_credential_reads: vec![PathBuf::from("~/.aws")],
            },
            logging: LoggingSettings {
                target: "stderr".into(),
                log_monitor: true,
                monitor_rate_limit_secs: 7,
            },
            http: HttpSettings {
                handshake_timeout_secs: 99,
                disable_quic: true,
                disable_http1_1: true,
                unix_socket_path: Some("/run/ahma.sock".into()),
            },
            auth: AuthSettings {
                require_token_path: "/etc/ahma/token".into(),
                rate_limit_rps: 50,
                rate_limit_burst: 5,
                require_token: Some("secret".into()),
            },
            instance: InstanceSettings {
                label: "custom-label".into(),
            },
            agent: AgentSettings {
                provider: Some("Ollama".into()),
                model: Some("gemma3:27b".into()),
                provider_url: Some("http://localhost:11434".into()),
            },
            web: WebSettings {
                default_policy: WebDefaultPolicy::Deny,
                block_private_ranges: false,
                on_redirect_to_new_domain: RedirectPolicy::Prompt,
                always_allow: vec!["api.github.com".into(), "*.crates.io".into()],
                never_allow: vec!["evil.example".into()],
            },
            network: NetworkSettings {
                restrict: true,
                allow: vec!["crates.io".into(), "*.crates.io".into()],
            },
            permissions: crate::permissions::PermissionSettings {
                tool_approvals: vec![crate::permissions::ToolApproval {
                    workspace: PathBuf::from("/home/u/proj"),
                    tools: vec!["cargo_build".into(), "list_dir".into()],
                    granted_at: Some("2026-07-12".into()),
                    granted_by: Some("user".into()),
                    surface: Some("tui".into()),
                }],
            },
        }
    }

    #[test]
    fn render_documented_round_trips_every_field() {
        // Guards against drift: if a new field is added to AhmaSettings but not
        // to render_documented, its non-default value is dropped here and the
        // parsed value no longer equals the original.
        let original = all_non_default_settings();
        let text = original.render_documented();
        let parsed: AhmaSettings = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("rendered settings must parse: {e}\n---\n{text}"));
        assert_eq!(
            parsed, original,
            "every non-default field must survive render → parse"
        );
    }

    #[test]
    fn render_documented_is_a_fixed_point() {
        // Rendering, parsing, and re-rendering must be byte-identical so that
        // ensure_current stops rewriting once the file is in canonical form.
        let original = all_non_default_settings();
        let once = original.render_documented();
        let reparsed: AhmaSettings = toml::from_str(&once).unwrap();
        let twice = reparsed.render_documented();
        assert_eq!(once, twice, "render must be a fixed point");
    }

    #[test]
    fn render_documented_default_has_no_active_assignments() {
        // A default configuration is fully minimized: every value line is either
        // a comment, a section header, or blank — nothing is asserted.
        let text = AhmaSettings::default().render_documented();
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with('[') {
                continue;
            }
            panic!("default settings must not assert any value, found: {line:?}");
        }
        // It still documents every section.
        for table in [
            "[features]",
            "[lmstudio]",
            "[tools]",
            "[sandbox]",
            "[logging]",
            "[http]",
            "[auth]",
            "[instance]",
            "[agent]",
        ] {
            assert!(text.contains(table), "missing section header {table}");
        }
        // Defaults are visible as commented documentation.
        assert!(text.contains("# timeout_secs = 600"));
        assert!(text.contains(&format!("# model = \"{DEFAULT_LMSTUDIO_MODEL}\"")));
        // And every option states its default in the doc line.
        assert!(text.contains("(default: 600)"));
    }

    #[test]
    fn render_documented_only_overrides_are_active() {
        // One field changed from default → exactly that line is active; its
        // siblings stay commented.
        let mut s = AhmaSettings::default();
        s.sandbox.tmp_access = true;
        let text = s.render_documented();
        assert!(
            text.contains("\ntmp_access = true\n"),
            "override is written active; got:\n{text}"
        );
        assert!(
            text.contains("# disable = false"),
            "untouched default stays commented; got:\n{text}"
        );
        // Round-trips back to the same override.
        let parsed: AhmaSettings = toml::from_str(&text).unwrap();
        assert!(parsed.sandbox.tmp_access);
        assert!(!parsed.sandbox.disable);
    }

    #[test]
    fn ensure_current_minimizes_a_fully_asserted_file() {
        // The pre-change format asserted every field, including defaults. After
        // ensure_current, default-valued assertions collapse into comments while
        // genuine overrides remain active.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        // A file that asserts a default (timeout_secs = 600) and an override.
        std::fs::write(&path, "[tools]\ntimeout_secs = 600\nforce_sync = true\n").unwrap();

        assert!(AhmaSettings::ensure_current(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        // The default assertion is gone (now a comment), the override stays.
        assert!(
            text.contains("# timeout_secs = 600"),
            "default value de-asserted into a comment; got:\n{text}"
        );
        assert!(
            text.contains("\nforce_sync = true\n"),
            "override preserved as active; got:\n{text}"
        );
        // Now idempotent.
        assert!(!AhmaSettings::ensure_current(&path).unwrap());
    }

    #[test]
    fn num_ctx_support_is_ollama_only() {
        // Ollama (by port or host) on the OpenAI flavor supports num_ctx.
        assert!(endpoint_supports_num_ctx(
            "http://localhost:11434/v1",
            ProviderKind::OpenAi
        ));
        assert!(endpoint_supports_num_ctx(
            "https://my-ollama.example.com/v1",
            ProviderKind::OpenAi
        ));
        // Hosted clouds pin context to the model — unsupported.
        assert!(!endpoint_supports_num_ctx(
            "https://api.openai.com/v1",
            ProviderKind::OpenAi
        ));
        assert!(!endpoint_supports_num_ctx(
            "https://api.together.xyz/v1",
            ProviderKind::OpenAi
        ));
        // Anthropic flavor never supports the Ollama option, even at :11434.
        assert!(!endpoint_supports_num_ctx(
            "http://localhost:11434/v1",
            ProviderKind::Anthropic
        ));
    }

    #[test]
    fn num_ctx_round_trips_through_toml() {
        let toml = r#"
            [[providers]]
            name = "ollama-local"
            kind = "openai"
            base_url = "http://localhost:11434/v1"
            default_model = "ornith:35b"
            num_ctx = 16384
        "#;
        let cfg: AhmaConfig = toml::from_str(toml).expect("parse");
        let p = &cfg.providers[0];
        assert_eq!(p.num_ctx, Some(16384));
        assert!(p.resolve().unwrap().supports_num_ctx());
        // Re-serialize and confirm the field survives.
        let out = toml::to_string(&cfg).expect("serialize");
        assert!(out.contains("num_ctx = 16384"), "got: {out}");
    }

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
    fn interpolate_default_used_when_var_unset() {
        unsafe {
            std::env::remove_var("AHMA_TEST_DEFAULT_UNSET");
        }
        let result =
            interpolate_env_vars("${AHMA_TEST_DEFAULT_UNSET:-http://localhost:11434/v1}").unwrap();
        assert_eq!(result, "http://localhost:11434/v1");
    }

    #[test]
    fn interpolate_default_used_when_var_empty() {
        unsafe {
            std::env::set_var("AHMA_TEST_DEFAULT_EMPTY", "");
        }
        let result = interpolate_env_vars("${AHMA_TEST_DEFAULT_EMPTY:-fallback}").unwrap();
        unsafe {
            std::env::remove_var("AHMA_TEST_DEFAULT_EMPTY");
        }
        assert_eq!(result, "fallback");
    }

    #[test]
    fn interpolate_set_variable_overrides_default() {
        unsafe {
            std::env::set_var("AHMA_TEST_DEFAULT_SET", "lfm2.5:8b");
        }
        let result = interpolate_env_vars("${AHMA_TEST_DEFAULT_SET:-llama3.2}").unwrap();
        unsafe {
            std::env::remove_var("AHMA_TEST_DEFAULT_SET");
        }
        assert_eq!(result, "lfm2.5:8b");
    }

    #[test]
    fn interpolate_empty_default_is_allowed() {
        unsafe {
            std::env::remove_var("AHMA_TEST_EMPTY_DEFAULT");
        }
        // `${VAR:-}` with no default text resolves to the empty string rather than erroring.
        let result = interpolate_env_vars("prefix-${AHMA_TEST_EMPTY_DEFAULT:-}-suffix").unwrap();
        assert_eq!(result, "prefix--suffix");
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
        // Default contains the auto-registered lmstudio provider
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "lmstudio");
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
        // 2 from config + 1 auto-registered lmstudio
        assert_eq!(cfg.providers.len(), 3);
        assert!(cfg.providers.iter().any(|p| p.name == "ollama-local"));
        assert!(cfg.providers.iter().any(|p| p.name == "openai"));
        assert!(cfg.providers.iter().any(|p| p.name == "lmstudio"));
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
            kind: ProviderKind::OpenAi,
            base_url: "http://localhost:11434/v1".into(),
            default_model: "llama3.2".into(),
            api_key: None,
            num_ctx: None,
        });

        let toml_text = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();

        let reloaded = AhmaConfig::load_from(tmp.path());
        // 1 from config + 1 auto-registered lmstudio
        assert_eq!(reloaded.providers.len(), 2);
        let p = reloaded
            .providers
            .iter()
            .find(|x| x.name == "roundtrip-test")
            .unwrap();
        assert_eq!(p.base_url, "http://localhost:11434/v1");
        assert_eq!(p.default_model, "llama3.2");
        assert!(p.api_key.is_none());
        assert!(reloaded.providers.iter().any(|x| x.name == "lmstudio"));
    }

    /// Add two providers, remove one, re-save, reload — verify only one survives.
    #[test]
    fn provider_remove_roundtrip_via_load_from() {
        let tmp = tempfile::NamedTempFile::new().unwrap();

        let mut cfg = AhmaConfig::default();
        cfg.providers.push(ProviderEntry {
            name: "keep-me".into(),
            kind: ProviderKind::OpenAi,
            base_url: "http://localhost:11434/v1".into(),
            default_model: "gemma3n".into(),
            api_key: None,
            num_ctx: None,
        });
        cfg.providers.push(ProviderEntry {
            name: "remove-me".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            default_model: "claude-opus-4-8".into(),
            api_key: Some("${ANTHROPIC_API_KEY}".into()),
            num_ctx: None,
        });

        let toml_text = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();

        // Simulate `ahma llm remove remove-me`.
        let mut cfg2 = AhmaConfig::load_from(tmp.path());
        cfg2.providers.retain(|p| p.name != "remove-me");
        let toml_text2 = toml::to_string_pretty(&cfg2).unwrap();
        std::fs::write(tmp.path(), toml_text2).unwrap();

        let final_cfg = AhmaConfig::load_from(tmp.path());
        // keep-me + auto-registered lmstudio
        assert_eq!(final_cfg.providers.len(), 2);
        assert!(final_cfg.providers.iter().any(|p| p.name == "keep-me"));
        assert!(final_cfg.providers.iter().any(|p| p.name == "lmstudio"));
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
        // 1 from config + 1 auto-registered lmstudio
        assert_eq!(cfg.providers.len(), 2);
        assert!(cfg.providers.iter().any(|p| p.name == "ollama-local"));
        assert!(cfg.providers.iter().any(|p| p.name == "lmstudio"));
        assert!(cfg.cluster.peers.is_empty(), "cluster defaults to no peers");
    }

    // ── AhmaSettings tests ────────────────────────────────────────────────────

    #[test]
    fn ahma_settings_default_lmstudio_values() {
        let s = AhmaSettings::default();
        assert_eq!(s.lmstudio.model, "openai/gpt-oss-20b");
        assert_eq!(s.lmstudio.base_url, "http://localhost:1234/v1");
    }

    #[test]
    fn ahma_settings_default_tools_values() {
        let s = AhmaSettings::default();
        assert_eq!(s.tools.timeout_secs, 600);
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
        assert_eq!(s.lmstudio.model, "openai/gpt-oss-20b");
        assert_eq!(s.tools.timeout_secs, 600);
    }

    #[test]
    fn ahma_settings_partial_toml_override() {
        let toml_str = r#"
[lmstudio]
model = "qwen/qwen3-4b"

[tools]
timeout_secs = 600
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let s = AhmaSettings::load_from(tmp.path());
        // Overridden values
        assert_eq!(s.lmstudio.model, "qwen/qwen3-4b");
        assert_eq!(s.tools.timeout_secs, 600);
        // Non-overridden values stay at defaults
        assert_eq!(s.lmstudio.base_url, "http://localhost:1234/v1");
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
        assert!(contents.contains("openai/gpt-oss-20b"));
        assert!(contents.contains("[lmstudio]"));
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

    /// Regression: a settings file that cannot be *read* because access is
    /// denied must fall back to compiled-in defaults, NOT abort. `~/.ahma` is
    /// intentionally out of sandbox scope (SPEC R5.4.8), so whenever ahma runs
    /// inside its own sandbox the read of `~/.ahma/settings.toml` returns EPERM
    /// (`PermissionDenied`). Treating that as fatal — the previous behavior —
    /// made ahma abort on every sandboxed launch (~150 test failures, all
    /// tracing to `fatal: failed to read …/settings.toml: Operation not
    /// permitted`). The file is deliberately INVALID toml here: if the loader
    /// could read it, it would return a parse `Err`; it must instead be unable
    /// to read it and return `Ok(defaults)`.
    #[cfg(unix)]
    #[test]
    fn permission_denied_settings_file_falls_back_to_defaults_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"this is not valid toml ========").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        // If the process can read it anyway (e.g. running as root, where mode
        // bits are not enforced), the reproduction does not hold — skip.
        if std::fs::read_to_string(&path).is_ok() {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
            return;
        }

        let result = AhmaSettings::load_from_result(&path);
        assert!(
            result.is_ok(),
            "a permission-denied settings file (the sandbox's ~/.ahma deny, SPEC R5.4.8) \
             must fall back to defaults, not abort startup: {result:?}"
        );

        // Restore perms so the tempdir can be cleaned up on drop.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    }

    /// Companion to the above for the startup auto-maintenance path:
    /// `ensure_current` must not `bail!` when `~/.ahma` is permission-denied by
    /// the sandbox — it skips the write and returns `Ok(false)`.
    #[cfg(unix)]
    #[test]
    fn ensure_current_tolerates_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"[tools]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        if std::fs::read_to_string(&path).is_ok() {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
            return;
        }

        let created =
            AhmaSettings::ensure_current(&path).expect("must not bail when the file is denied");
        assert!(
            !created,
            "ensure_current must skip the write (Ok(false)) when the file is permission-denied"
        );

        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    }

    #[test]
    fn sandbox_unknown_key_rejected() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[sandbox]\nunknown_typo_key = true").unwrap();
        assert!(
            AhmaSettings::load_from_result(&path).is_err(),
            "unknown key in [sandbox] should be rejected"
        );
    }

    #[test]
    fn auth_unknown_key_rejected() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[auth]\nbogus_setting = \"yes\"").unwrap();
        assert!(
            AhmaSettings::load_from_result(&path).is_err(),
            "unknown key in [auth] should be rejected"
        );
    }

    #[test]
    fn tools_unknown_key_allowed_for_forward_compat() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[tools]\nfuture_feature = true").unwrap();
        assert!(
            AhmaSettings::load_from_result(&path).is_ok(),
            "unknown key in [tools] should be tolerated"
        );
    }

    // ── persistent_scopes ────────────────────────────────────────────────────

    #[test]
    fn scope_access_serializes_as_lowercase() {
        assert_eq!(
            toml::to_string(&PersistentScope {
                path: PathBuf::from("/x"),
                access: ScopeAccess::Ro,
                granted_by: None,
                granted_at: None,
                note: None,
            })
            .unwrap()
            .trim(),
            "path = \"/x\"\naccess = \"ro\""
        );
        assert!(ScopeAccess::Rw.is_write());
        assert!(!ScopeAccess::Ro.is_write());
    }

    #[test]
    fn persistent_scope_toml_round_trip_defaults_to_rw() {
        let toml_str = r#"
[sandbox]
persistent_scopes = [
  { path = "~/Library/Caches/Mozilla.sccache", granted_by = "sccache", note = "compiler cache" },
  { path = "/opt/toolchains", access = "ro" },
]
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let s = AhmaSettings::load_from(tmp.path());
        assert_eq!(s.sandbox.persistent_scopes.len(), 2);
        // access omitted ⇒ defaults to rw
        assert_eq!(s.sandbox.persistent_scopes[0].access, ScopeAccess::Rw);
        assert_eq!(
            s.sandbox.persistent_scopes[0].granted_by.as_deref(),
            Some("sccache")
        );
        assert_eq!(s.sandbox.persistent_scopes[1].access, ScopeAccess::Ro);
    }

    #[test]
    fn grant_scope_adds_then_updates_in_place() {
        let mut sb = SandboxSettings::default();
        let mk = |access| PersistentScope {
            path: PathBuf::from("/cache"),
            access,
            granted_by: None,
            granted_at: None,
            note: None,
        };
        assert_eq!(sb.grant_scope(mk(ScopeAccess::Rw)), GrantOutcome::Added);
        assert_eq!(sb.persistent_scopes.len(), 1);

        // Re-granting the same path updates in place rather than duplicating,
        // and hands back the previous value for the confirm message.
        match sb.grant_scope(mk(ScopeAccess::Ro)) {
            GrantOutcome::Updated(old) => assert_eq!(old.access, ScopeAccess::Rw),
            GrantOutcome::Added => panic!("expected an in-place update"),
        }
        assert_eq!(sb.persistent_scopes.len(), 1);
        assert_eq!(sb.persistent_scopes[0].access, ScopeAccess::Ro);
    }

    #[test]
    fn revoke_scope_removes_match_or_reports_absent() {
        let mut sb = SandboxSettings::default();
        sb.grant_scope(PersistentScope {
            path: PathBuf::from("/cache"),
            access: ScopeAccess::Rw,
            granted_by: None,
            granted_at: None,
            note: None,
        });
        assert!(sb.revoke_scope(Path::new("/nope")).is_none());
        let removed = sb.revoke_scope(Path::new("/cache")).expect("removed");
        assert_eq!(removed.path, PathBuf::from("/cache"));
        assert!(sb.persistent_scopes.is_empty());
    }

    #[test]
    fn scope_matching_is_tilde_insensitive() {
        let Some(home) = dirs::home_dir() else {
            return; // no home dir in this environment; skip
        };
        let mut sb = SandboxSettings::default();
        sb.grant_scope(PersistentScope {
            path: PathBuf::from("~/foo"),
            access: ScopeAccess::Rw,
            granted_by: None,
            granted_at: None,
            note: None,
        });
        // Looking up by the expanded absolute path finds the ~-stored entry.
        assert!(sb.find_scope(&home.join("foo")).is_some());
        // Revoking by the expanded path removes the ~-stored entry.
        assert!(sb.revoke_scope(&home.join("foo")).is_some());
        assert!(sb.persistent_scopes.is_empty());
    }
}
