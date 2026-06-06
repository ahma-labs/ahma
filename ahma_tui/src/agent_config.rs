use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub model: String,
    pub provider_url: String,
    pub system_prompt: String,
    pub tool_approval: bool,
    pub max_turns: u32,
    pub mcp_servers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentProfilesFile {
    pub profiles: BTreeMap<String, AgentProfile>,
}

pub fn load_profiles(cwd: &Path) -> Result<AgentProfilesFile> {
    let path = profiles_path(cwd);
    if !path.exists() {
        return Ok(AgentProfilesFile::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn save_profiles(cwd: &Path, profiles: &AgentProfilesFile) -> Result<()> {
    let dir = cwd.join(".ahma");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    let path = profiles_path(cwd);
    let text = toml::to_string_pretty(profiles).context("Failed to serialize profiles")?;
    std::fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))
}

pub fn upsert_profile(cwd: &Path, profile: AgentProfile) -> Result<()> {
    let mut file = load_profiles(cwd)?;
    file.profiles.insert(profile.name.clone(), profile);
    save_profiles(cwd, &file)
}

pub fn get_profile(cwd: &Path, name: &str) -> Result<AgentProfile> {
    let file = load_profiles(cwd)?;
    file.profiles
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("Profile not found: {name}"))
}

pub fn delete_profile(cwd: &Path, name: &str) -> Result<bool> {
    let mut file = load_profiles(cwd)?;
    let removed = file.profiles.remove(name).is_some();
    save_profiles(cwd, &file)?;
    Ok(removed)
}

pub fn append_transcript_entry(cwd: &Path, profile_name: &str, line_json: &str) -> Result<()> {
    let dir = transcripts_dir(cwd, profile_name);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    let path = dir.join(format!("{}.jsonl", chrono::Local::now().format("%Y-%m-%d")));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    writeln!(file, "{line_json}").with_context(|| format!("Failed to append {}", path.display()))
}

fn profiles_path(cwd: &Path) -> PathBuf {
    cwd.join(".ahma").join("agent-profiles.toml")
}

fn transcripts_dir(cwd: &Path, profile_name: &str) -> PathBuf {
    cwd.join(".ahma").join("conversations").join(profile_name)
}
