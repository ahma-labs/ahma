//! # Ahma configuration: env-var interpolation and named provider registry.
//!
//! ## `~/.ahma/config.toml` format
//!
//! ```toml
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

/// The top-level structure of `~/.ahma/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AhmaConfig {
    /// Named LLM provider definitions.
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
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
}
