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
}
