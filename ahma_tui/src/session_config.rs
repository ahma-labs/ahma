use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_mcp_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiSessionConfig {
    pub provider: String,
    pub model: String,
    pub provider_url: Option<String>,
    #[serde(default = "default_mcp_enabled")]
    pub mcp_enabled: bool,
    #[serde(default)]
    pub active_profile: Option<String>,
}

impl TuiSessionConfig {
    pub fn load(cwd: &Path) -> Result<Option<Self>> {
        let path = cwd.join(".ahma").join("session.toml");
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(Some(config))
    }

    pub fn save(&self, cwd: &Path) -> Result<()> {
        let dir = cwd.join(".ahma");
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }
        let path = dir.join("session.toml");
        let content = toml::to_string(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_enabled_default_true() {
        let content = r#"
            provider = "Ollama"
            model = "gemma4:26b-mlx"
        "#;
        let config: TuiSessionConfig = toml::from_str(content).unwrap();
        assert!(config.mcp_enabled);
    }

    #[test]
    fn test_mcp_enabled_respects_false() {
        let content = r#"
            provider = "Ollama"
            model = "gemma4:26b-mlx"
            mcp_enabled = false
        "#;
        let config: TuiSessionConfig = toml::from_str(content).unwrap();
        assert!(!config.mcp_enabled);
    }

    // ── coverage batch: load / save round-trips ───────────────────────────────
    use tempfile::TempDir;

    #[test]
    fn load_returns_none_when_no_session_file_exists() {
        let temp = TempDir::new().unwrap();
        let result = TuiSessionConfig::load(temp.path()).unwrap();
        assert!(
            result.is_none(),
            "expected Ok(None) when no session file exists"
        );
    }

    #[test]
    fn save_then_load_round_trip_preserves_all_fields_with_some_options() {
        let temp = TempDir::new().unwrap();
        let config = TuiSessionConfig {
            provider: "Ollama".to_string(),
            model: "gemma4:26b-mlx".to_string(),
            provider_url: Some("http://localhost:11434".to_string()),
            mcp_enabled: false,
            active_profile: Some("dev".to_string()),
        };

        config.save(temp.path()).unwrap();
        let loaded = TuiSessionConfig::load(temp.path())
            .unwrap()
            .expect("expected Some(config) after save");

        assert_eq!(loaded.provider, "Ollama");
        assert_eq!(loaded.model, "gemma4:26b-mlx");
        assert_eq!(
            loaded.provider_url,
            Some("http://localhost:11434".to_string())
        );
        assert!(!loaded.mcp_enabled);
        assert_eq!(loaded.active_profile, Some("dev".to_string()));
    }

    #[test]
    fn save_then_load_round_trip_preserves_none_options() {
        let temp = TempDir::new().unwrap();
        let config = TuiSessionConfig {
            provider: "OpenAI".to_string(),
            model: "gpt-x".to_string(),
            provider_url: None,
            mcp_enabled: true,
            active_profile: None,
        };

        config.save(temp.path()).unwrap();
        let loaded = TuiSessionConfig::load(temp.path())
            .unwrap()
            .expect("expected Some(config) after save");

        assert_eq!(loaded.provider, "OpenAI");
        assert_eq!(loaded.model, "gpt-x");
        assert_eq!(loaded.provider_url, None);
        assert!(loaded.mcp_enabled);
        assert_eq!(loaded.active_profile, None);
    }

    #[test]
    fn save_creates_ahma_directory_and_session_file() {
        let temp = TempDir::new().unwrap();
        let ahma_dir = temp.path().join(".ahma");
        let session_file = ahma_dir.join("session.toml");
        assert!(!ahma_dir.exists(), "precondition: .ahma must not exist yet");

        let config = TuiSessionConfig {
            provider: "Ollama".to_string(),
            model: "m".to_string(),
            provider_url: None,
            mcp_enabled: true,
            active_profile: None,
        };
        config.save(temp.path()).unwrap();

        assert!(ahma_dir.is_dir(), "save must create the .ahma directory");
        assert!(session_file.is_file(), "save must write session.toml");
    }

    #[test]
    fn save_is_idempotent_and_overwrites_when_ahma_dir_already_exists() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".ahma")).unwrap();

        let first = TuiSessionConfig {
            provider: "Ollama".to_string(),
            model: "first-model".to_string(),
            provider_url: None,
            mcp_enabled: true,
            active_profile: None,
        };
        first.save(temp.path()).unwrap();

        let second = TuiSessionConfig {
            provider: "OpenAI".to_string(),
            model: "second-model".to_string(),
            provider_url: Some("http://example".to_string()),
            mcp_enabled: false,
            active_profile: Some("prod".to_string()),
        };
        second.save(temp.path()).unwrap();

        let loaded = TuiSessionConfig::load(temp.path())
            .unwrap()
            .expect("expected Some(config) after overwrite");
        assert_eq!(loaded.provider, "OpenAI");
        assert_eq!(loaded.model, "second-model");
        assert_eq!(loaded.provider_url, Some("http://example".to_string()));
        assert!(!loaded.mcp_enabled);
        assert_eq!(loaded.active_profile, Some("prod".to_string()));
    }

    #[test]
    fn load_returns_err_on_malformed_toml() {
        let temp = TempDir::new().unwrap();
        let ahma_dir = temp.path().join(".ahma");
        std::fs::create_dir_all(&ahma_dir).unwrap();
        std::fs::write(
            ahma_dir.join("session.toml"),
            "this is not = valid toml = = [[[",
        )
        .unwrap();

        let result = TuiSessionConfig::load(temp.path());
        assert!(
            result.is_err(),
            "expected Err when session.toml is malformed"
        );
    }
}
