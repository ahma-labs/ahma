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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn sample_profile(name: &str) -> AgentProfile {
        AgentProfile {
            name: name.to_string(),
            model: "claude-opus-4".to_string(),
            provider_url: "https://api.anthropic.com".to_string(),
            system_prompt: "You are a helpful assistant.".to_string(),
            tool_approval: true,
            max_turns: 12,
            mcp_servers: vec!["ahma".to_string(), "github".to_string()],
        }
    }

    #[test]
    fn load_profiles_missing_file_returns_default() {
        let tmp = tempdir().unwrap();
        let file = load_profiles(tmp.path()).expect("missing file should yield default");
        assert!(file.profiles.is_empty());
    }

    #[test]
    fn save_and_load_round_trips_profiles() {
        let tmp = tempdir().unwrap();
        // .ahma dir does not exist yet — save must create it.
        assert!(!tmp.path().join(".ahma").exists());

        let mut data = AgentProfilesFile::default();
        data.profiles
            .insert("dev".to_string(), sample_profile("dev"));
        save_profiles(tmp.path(), &data).expect("save should succeed");

        assert!(
            tmp.path()
                .join(".ahma")
                .join("agent-profiles.toml")
                .exists()
        );

        let loaded = load_profiles(tmp.path()).expect("load should succeed");
        assert_eq!(loaded.profiles.len(), 1);
        let p = loaded.profiles.get("dev").expect("profile present");
        assert_eq!(p.name, "dev");
        assert_eq!(p.model, "claude-opus-4");
        assert_eq!(p.provider_url, "https://api.anthropic.com");
        assert_eq!(p.system_prompt, "You are a helpful assistant.");
        assert!(p.tool_approval);
        assert_eq!(p.max_turns, 12);
        assert_eq!(
            p.mcp_servers,
            vec!["ahma".to_string(), "github".to_string()]
        );
    }

    #[test]
    fn save_profiles_with_existing_ahma_dir() {
        let tmp = tempdir().unwrap();
        // Pre-create .ahma so the `if !dir.exists()` branch is skipped.
        fs::create_dir_all(tmp.path().join(".ahma")).unwrap();

        let data = AgentProfilesFile::default();
        save_profiles(tmp.path(), &data).expect("save with existing dir should succeed");
        assert!(
            tmp.path()
                .join(".ahma")
                .join("agent-profiles.toml")
                .exists()
        );
    }

    #[test]
    fn load_profiles_parse_error_returns_err() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".ahma")).unwrap();
        fs::write(
            tmp.path().join(".ahma").join("agent-profiles.toml"),
            "this = is = not valid toml ===",
        )
        .unwrap();

        let result = load_profiles(tmp.path());
        assert!(result.is_err(), "invalid TOML should produce an error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("Failed to parse"), "got: {msg}");
    }

    #[test]
    fn upsert_profile_inserts_then_updates() {
        let tmp = tempdir().unwrap();

        upsert_profile(tmp.path(), sample_profile("agent")).expect("insert should succeed");
        let got = get_profile(tmp.path(), "agent").expect("profile should be found");
        assert_eq!(got.model, "claude-opus-4");
        assert_eq!(got.max_turns, 12);

        // Upsert again with same name but different fields -> update in place.
        let mut updated = sample_profile("agent");
        updated.model = "claude-sonnet-4".to_string();
        updated.max_turns = 99;
        upsert_profile(tmp.path(), updated).expect("update should succeed");

        let after = load_profiles(tmp.path()).expect("load should succeed");
        assert_eq!(after.profiles.len(), 1, "same name must not duplicate");
        let p = after.profiles.get("agent").unwrap();
        assert_eq!(p.model, "claude-sonnet-4");
        assert_eq!(p.max_turns, 99);
    }

    #[test]
    fn get_profile_not_found_returns_err_with_name() {
        let tmp = tempdir().unwrap();
        let err = get_profile(tmp.path(), "nope").expect_err("missing profile should be Err");
        let msg = err.to_string();
        assert!(msg.contains("nope"), "error should mention the name: {msg}");
        assert!(msg.contains("Profile not found"), "got: {msg}");
    }

    #[test]
    fn delete_profile_true_when_present_false_when_absent() {
        let tmp = tempdir().unwrap();
        upsert_profile(tmp.path(), sample_profile("temp")).unwrap();

        let removed = delete_profile(tmp.path(), "temp").expect("delete should succeed");
        assert!(removed, "should report true when a profile was removed");
        assert!(
            get_profile(tmp.path(), "temp").is_err(),
            "profile should be gone"
        );

        let removed_again = delete_profile(tmp.path(), "temp").expect("delete should succeed");
        assert!(!removed_again, "should report false when nothing to remove");
    }

    #[test]
    fn append_transcript_entry_creates_dir_and_appends_lines() {
        let tmp = tempdir().unwrap();
        let convo_dir = tmp.path().join(".ahma").join("conversations").join("agent");
        assert!(!convo_dir.exists());

        append_transcript_entry(tmp.path(), "agent", r#"{"role":"user","text":"hi"}"#)
            .expect("first append should succeed");
        // Second call hits the `dir.exists()` true branch.
        append_transcript_entry(tmp.path(), "agent", r#"{"role":"assistant","text":"yo"}"#)
            .expect("second append should succeed");

        assert!(convo_dir.exists(), "conversations dir should be created");

        // Find the single dated .jsonl file (don't hardcode today's date).
        let entries: Vec<_> = fs::read_dir(&convo_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect();
        assert_eq!(entries.len(), 1, "exactly one dated transcript file");

        let content = fs::read_to_string(&entries[0]).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "two appended lines");
        assert_eq!(lines[0], r#"{"role":"user","text":"hi"}"#);
        assert_eq!(lines[1], r#"{"role":"assistant","text":"yo"}"#);
    }
}
