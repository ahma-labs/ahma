//! Chat transcripts, one per window.
//!
//! Each window (client section) keeps its own conversation: switching windows
//! used to keep one transcript and send it to a different instance and model.
//! After every turn the window's transcript is saved under
//! `~/.ahma/transcripts/`, keyed by the window's durable identity (client and
//! workspace, see `AppState::window_llm_key`), and `/resume` loads it back.
//! Not in the project: a conversation can hold secrets, and anything in the
//! repository can be committed by accident.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{ChatEntry, ChatHistory};

/// One saved message. Thinking is not saved (it is never sent back to the
/// model), nor anything still in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedEntry {
    /// `user`, `assistant` or `tool`.
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Tool name, for `tool` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default)]
    pub failed: bool,
}

pub fn to_saved(history: &ChatHistory) -> Vec<SavedEntry> {
    history
        .entries()
        .iter()
        .filter_map(|e| match e {
            ChatEntry::User { text, payload, .. } => Some(SavedEntry {
                role: "user".into(),
                text: text.clone(),
                payload: payload.clone(),
                name: None,
                result: None,
                failed: false,
            }),
            ChatEntry::Assistant {
                content,
                streaming: false,
            } if !content.is_empty() => Some(SavedEntry {
                role: "assistant".into(),
                text: content.clone(),
                payload: None,
                name: None,
                result: None,
                failed: false,
            }),
            ChatEntry::ToolCall {
                name,
                args,
                result: Some(result),
                failed,
                ..
            } => Some(SavedEntry {
                role: "tool".into(),
                text: args.clone(),
                payload: None,
                name: Some(name.clone()),
                result: Some(result.clone()),
                failed: *failed,
            }),
            _ => None,
        })
        .collect()
}

pub fn from_saved(saved: Vec<SavedEntry>) -> ChatHistory {
    let mut history = ChatHistory::default();
    for (i, e) in saved.into_iter().enumerate() {
        match e.role.as_str() {
            "user" => history.push(ChatEntry::User {
                text: e.text,
                payload: e.payload,
                started_at: None,
                duration_ms: None,
            }),
            "assistant" => history.push(ChatEntry::Assistant {
                content: e.text,
                streaming: false,
            }),
            "tool" => history.push(ChatEntry::ToolCall {
                id: format!("resumed-{i}"),
                name: e.name.unwrap_or_default(),
                args: e.text,
                result: e.result,
                failed: e.failed,
            }),
            _ => {}
        }
    }
    history
}

/// `<dir>/<key>.json`, with the key reduced to a safe file name.
pub fn path_for(dir: &Path, key: &str) -> PathBuf {
    let mut name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.truncate(120);
    if name.is_empty() || name.chars().all(|c| c == '.') {
        name = "this-terminal".into();
    }
    dir.join(format!("{name}.json"))
}

/// The transcripts directory under a home directory (`~/.ahma/transcripts`).
pub fn dir_for(home: &Path) -> PathBuf {
    home.join(".ahma").join("transcripts")
}

pub fn save(path: &Path, entries: &[SavedEntry]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(entries)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load(path: &Path) -> anyhow::Result<Option<Vec<SavedEntry>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_round_trips_through_disk() {
        let mut h = ChatHistory::default();
        h.push(ChatEntry::User {
            text: "/review".into(),
            payload: Some("full skill text".into()),
            started_at: None,
            duration_ms: Some(10),
        });
        h.start_tool_call("c1".into(), "read_file".into(), "{\"path\":\"a\"}".into());
        h.finish_tool_call("c1", "contents".into(), false);
        h.append_token("done");
        h.finish_stream();
        h.push(ChatEntry::Thinking {
            content: "not saved".into(),
            streaming: false,
        });

        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "claude-code@/w/ahma");
        assert!(path.starts_with(dir.path()), "stays in the directory");
        save(&path, &to_saved(&h)).unwrap();
        let back = from_saved(load(&path).unwrap().expect("saved"));

        let saved = to_saved(&back);
        let roles: Vec<&str> = saved.iter().map(|e| e.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "tool", "assistant"]);
        assert_eq!(saved[0].payload.as_deref(), Some("full skill text"));
    }

    #[test]
    fn keys_become_safe_file_names() {
        let dir = Path::new("/t");
        assert_eq!(path_for(dir, "../../etc"), dir.join(".._.._etc.json"));
        assert_eq!(path_for(dir, ""), dir.join("this-terminal.json"));
    }
}
