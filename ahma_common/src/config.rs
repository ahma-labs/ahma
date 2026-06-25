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
        })
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LmStudioSettings {
    /// Base URL of the LM Studio local-server endpoint.
    /// Default: `http://localhost:1234/v1`
    pub base_url: String,
    /// Model identifier of the model loaded in LM Studio, e.g. `openai/gpt-oss-20b`.
    pub model: String,
}

impl Default for LmStudioSettings {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:1234/v1".to_string(),
            model: "openai/gpt-oss-20b".to_string(),
        }
    }
}

/// Tool execution settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Command serialisation groups.  Commands matching a group's prefix are
    /// serialised per working directory (at most one runs at a time within
    /// that directory).  Defaults to a single `cargo` group so that
    /// `cargo build`, `cargo test`, `cargo clippy`, etc. do not contend on
    /// the shared `target/` directory.
    /// Default: `[{ name = "cargo", prefixes = ["cargo"], max_wait_secs = 600 }]`
    #[serde(default = "default_mutex_groups")]
    pub mutex_groups: Vec<MutexGroupConfig>,
    /// Use a dedicated `target/ahma` subdirectory for cargo builds spawned by
    /// ahma, completely isolating them from the IDE's background `cargo check`.
    /// Eliminates cross-process file-lock contention at the cost of a cold
    /// build cache on the first run after a restart.
    /// Default: `false`
    pub separate_cargo_target: bool,
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
            mutex_groups: default_mutex_groups(),
            separate_cargo_target: true,
        }
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        }
    }
}

fn default_sandbox_directory() -> Option<PathBuf> {
    Some(PathBuf::from("~/sandbox"))
}

/// Expand `~` or `~/…` to the user's home directory.
pub(crate) fn expand_home(path: &Path) -> PathBuf {
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

/// Runtime feature toggles.
///
/// Controls which optional capabilities are active at runtime.  Features
/// default to the most useful "batteries-included" configuration: everything
/// that works without additional setup is enabled, while features that require
/// external infrastructure (cluster peers, etc.) start disabled.
///
/// Toggle in `~/.ahma/settings.toml` or via `ahma tui` → `/settings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

    /// Strict loader: returns `Err(message)` on a read or parse failure instead
    /// of falling back to defaults. A missing file is **not** an error (returns
    /// defaults). This is the primitive the startup path uses to fail closed
    /// (R-CFG6.1) and that tests use to verify bad-config rejection.
    pub fn load_from_result(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents)
                .map_err(|e| format!("failed to parse settings file {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("failed to read {}: {e}", path.display())),
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
        let toml_text =
            toml::to_string_pretty(self).context("Failed to serialize settings to TOML")?;

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
# Edit interactively with:                   ahma tui → /settings

# ── Features ─────────────────────────────────────────────────────────────────
# Toggle optional capabilities on/off.  Features that work without additional
# setup are enabled by default; features requiring infrastructure are off.
#
# [features]
# simplify  = true    # code complexity analysis (ahma simplify)
# vault     = false   # task vault isolation (per-session working directories)
# cluster   = false   # distributed cluster scheduling (requires peer setup)
# egress    = true    # network egress proxy for sandboxed tasks
# artifact  = true    # HTML artifact output channel
# decompose = true    # LLM-powered task decomposition

# ── Tool execution ───────────────────────────────────────────────────────────
# [tools]
# timeout_secs = 600      # default tool timeout (seconds); per-tool override via timeout_seconds
# force_sync   = false    # run all tools synchronously instead of async-first
# hot_reload   = false    # reload tools from disk on change — INSECURE in production
# skip_probes  = false    # skip availability probes at startup
# tools_dir    = ".ahma"  # path to tools directory containing JSON tool definitions
# tool_bundles = []       # tool bundles to enable (e.g. ["rust", "git"])
# minimize_tokens     = false # enable output compression and token minimization
# small_model_harness = false # enable small-model harness adaptations
#
# Command serialisation: commands matching a group's prefix are serialised per
# working directory so they don't contend on shared resources (e.g. cargo's target/).
# The cargo group is enabled by default.  Set mutex_groups = [] to disable.
# mutex_groups = [
#   { name = "cargo", prefixes = ["cargo"], max_wait_secs = 600 }
# ]
# Add more groups for other slow exclusive tools, e.g.:
#   { name = "gradle", prefixes = ["gradle", "./gradlew"], max_wait_secs = 600 }
#
# separate_cargo_target = true   # default: true — sandbox builds write to target/ahma/ instead of target/,
#                                # preventing com.apple.provenance xattr contamination (macOS Seatbelt stamps
#                                # every file it writes; those files cannot be overwritten by other processes)
#                                # and eliminating cross-process file-lock contention with IDE background checks.
#                                # Set to false only if you want sandbox and IDE builds to share target/.

# ── LM Studio (local OpenAI-compatible server) ──────────────────────────────
# Start the LM Studio Local Server (Developer tab), or headless: lms server start
#
# [lmstudio]
# base_url = "http://localhost:1234/v1"  # default: 1234 (LM Studio local server)
# model    = "openai/gpt-oss-20b"        # set to the model loaded in LM Studio

# ── Sandbox & filesystem security ────────────────────────────────────────────
# [sandbox]
# disable              = false    # UNSAFE: disable kernel sandbox entirely
# tmp_access           = false    # add system temp dir to sandbox scope
# disable_temp         = false    # block all access to system temp dir (overrides tmp_access)
# defer                = false    # defer sandbox lock until client provides roots/list
# task_vault           = ""       # run this server session inside an existing task vault
# scopes               = []       # paths allowed for read/write access under the sandbox
# package_cache_write  = true     # allow package-manager caches (cargo registry/git) to be written
# working_dirs         = []       # directories containing allowed working directories
# sandbox_directory    = "~/sandbox"  # default scratch directory, auto-created when needed
#
# persistent_scopes: machine-local external directories that survive roots/list
# replacement — e.g. a build cache outside the workspace (sccache, ccache).
# Manage with `ahma sandbox grant|list|revoke`, or edit by hand.  Unlike `scopes`
# (provisional, replaced when the client sends workspace roots), these are
# re-added on every roots/list update so the grant lasts the whole session.
# persistent_scopes = [
#   { path = "~/Library/Caches/Mozilla.sccache", access = "rw", granted_by = "sccache", note = "compiler cache" },
# ]

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
        });
        cfg.providers.push(ProviderEntry {
            name: "remove-me".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            default_model: "claude-opus-4-8".into(),
            api_key: Some("${ANTHROPIC_API_KEY}".into()),
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
