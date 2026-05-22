//! # Ahma configuration: env-var interpolation and named provider registry.
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
        match std::fs::read_to_string(path) {
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
        }
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
        assert!(cfg.providers.is_empty());
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
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].name, "ollama-local");
        assert_eq!(cfg.providers[1].name, "openai");
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
        assert_eq!(reloaded.providers.len(), 1);
        assert_eq!(reloaded.providers[0].name, "roundtrip-test");
        assert_eq!(reloaded.providers[0].base_url, "http://localhost:11434/v1");
        assert_eq!(reloaded.providers[0].default_model, "llama3.2");
        assert!(reloaded.providers[0].api_key.is_none());
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
        assert_eq!(final_cfg.providers.len(), 1);
        assert_eq!(final_cfg.providers[0].name, "keep-me");
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
        assert_eq!(cfg.providers.len(), 1);
        assert!(cfg.cluster.peers.is_empty(), "cluster defaults to no peers");
    }
}
