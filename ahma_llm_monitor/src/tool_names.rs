//! Tool names as a provider accepts them.
//!
//! ahma names a tool on an external MCP server `server::tool`, and that form is
//! load-bearing inside ahma (routing, approvals, the tool menu). OpenAI and
//! Anthropic both restrict a function name to `^[a-zA-Z0-9_-]{1,64}$`, so a
//! request carrying `jira::search` is rejected with a 400 — which, before the
//! error classification was narrowed, also read as "this model does not support
//! tools" and silently dropped tool use. The fix belongs at the wire: each name
//! that a provider would reject is replaced, for that request only, by a valid
//! one, and the provider's tool calls are mapped back before anything else in
//! ahma sees them. A name that is already valid is sent as it is.

use std::collections::HashMap;

use serde_json::Value;

/// The longest function name the providers accept.
const MAX_LEN: usize = 64;

fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn short_hash(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// A valid provider name for `name`: unchanged when already valid, else `::`
/// becomes `__`, any other disallowed character `_`, and a name that is too
/// long keeps its head and gains a short hash of the whole.
pub fn wire_name(name: &str) -> String {
    if is_valid(name) {
        return name.to_string();
    }
    let mut out: String = name
        .replace("::", "__")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    if out.len() > MAX_LEN {
        out.truncate(MAX_LEN - 9);
        out.push('_');
        out.push_str(&short_hash(name));
    }
    out
}

/// The names one request renamed, so the reply can be mapped back.
#[derive(Debug, Default, Clone)]
pub struct WireNames {
    to_original: HashMap<String, String>,
    to_wire: HashMap<String, String>,
}

impl WireNames {
    /// The wire name for `name`, unique within this request.
    fn assign(&mut self, name: &str) -> String {
        if let Some(w) = self.to_wire.get(name) {
            return w.clone();
        }
        let mut wire = wire_name(name);
        if wire != name && self.to_original.contains_key(&wire) {
            // Two distinct names sanitized alike: keep both callable.
            let suffix = format!("_{}", short_hash(name));
            wire.truncate(MAX_LEN - suffix.len());
            wire.push_str(&suffix);
        }
        if wire != name {
            self.to_original.insert(wire.clone(), name.to_string());
        }
        self.to_wire.insert(name.to_string(), wire.clone());
        wire
    }

    /// The name ahma uses for a tool the provider called `wire`.
    pub fn original<'a>(&'a self, wire: &'a str) -> &'a str {
        self.to_original
            .get(wire)
            .map(String::as_str)
            .unwrap_or(wire)
    }
}

/// Rewrite every tool name in `tools` (OpenAI function definitions) and in the
/// assistant tool calls of `messages` to its wire name.
pub fn to_wire(tools: &[Value], messages: &[Value]) -> (Vec<Value>, Vec<Value>, WireNames) {
    let mut names = WireNames::default();
    let tools = tools
        .iter()
        .map(|t| {
            let mut t = t.clone();
            if let Some(n) = t.pointer("/function/name").and_then(Value::as_str) {
                let w = names.assign(n);
                t["function"]["name"] = Value::String(w);
            }
            t
        })
        .collect();
    let messages = messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(calls) = m.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    if let Some(n) = call.pointer("/function/name").and_then(Value::as_str) {
                        let w = names.assign(n);
                        call["function"]["name"] = Value::String(w);
                    }
                }
            }
            if m.get("role").and_then(Value::as_str) == Some("tool")
                && let Some(n) = m.get("name").and_then(Value::as_str)
            {
                let w = names.assign(n);
                m["name"] = Value::String(w);
            }
            m
        })
        .collect();
    (tools, messages, names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_names_pass_and_invalid_ones_become_valid() {
        assert_eq!(wire_name("run_terminal_command"), "run_terminal_command");
        assert_eq!(wire_name("jira::search"), "jira__search");
        assert_eq!(wire_name("my server::look up"), "my_server__look_up");
        let long = format!("srv::{}", "x".repeat(80));
        let w = wire_name(&long);
        assert!(is_valid(&w), "{w}");
        assert_eq!(w.len(), MAX_LEN);
    }

    #[test]
    fn a_request_is_renamed_and_its_calls_map_back() {
        let tools = vec![
            json!({"type":"function","function":{"name":"jira::search","parameters":{}}}),
            json!({"type":"function","function":{"name":"await","parameters":{}}}),
        ];
        let messages = vec![
            json!({"role":"user","content":"find it"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"jira::search","arguments":"{}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"c1","content":"[]"}),
        ];
        let (tools, messages, names) = to_wire(&tools, &messages);
        assert_eq!(tools[0]["function"]["name"], "jira__search");
        assert_eq!(tools[1]["function"]["name"], "await");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "jira__search"
        );
        let wire = serde_json::to_string(&(&tools, &messages)).unwrap();
        assert!(!wire.contains("::"), "{wire}");
        assert_eq!(names.original("jira__search"), "jira::search");
        assert_eq!(names.original("await"), "await");
    }

    #[test]
    fn two_names_that_sanitize_alike_stay_distinct() {
        let tools = vec![
            json!({"type":"function","function":{"name":"a::b"}}),
            json!({"type":"function","function":{"name":"a.:b"}}),
        ];
        let (tools, _, names) = to_wire(&tools, &[]);
        let (x, y) = (
            tools[0]["function"]["name"].as_str().unwrap(),
            tools[1]["function"]["name"].as_str().unwrap(),
        );
        assert_ne!(x, y);
        assert!(is_valid(x) && is_valid(y));
        assert_eq!(names.original(x), "a::b");
        assert_eq!(names.original(y), "a.:b");
    }
}
