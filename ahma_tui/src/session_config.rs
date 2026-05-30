use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiSessionConfig {
    pub provider: String,
    pub model: String,
    pub provider_url: Option<String>,
    pub mcp_enabled: bool,
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
