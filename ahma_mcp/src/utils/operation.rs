//! Helper functions for generating descriptive operation IDs.

/// Cleans and extracts up to 2 descriptive lowercase alphanumeric segments from an input string (e.g. a command or tool name),
/// ignoring flags, options, paths, etc.
pub fn clean_details(input: &str) -> Option<String> {
    let mut parts = Vec::new();
    for word in input.split_whitespace() {
        // Strip options/flags
        if word.starts_with('-') {
            continue;
        }
        // If it looks like a path, try to extract the last component
        let word_clean = if word.contains('/') || word.contains('\\') {
            word.split(&['/', '\\'][..]).last().unwrap_or(word)
        } else {
            word
        };
        // Clean characters: keep only alphanumeric and underscores
        let cleaned: String = word_clean
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !cleaned.is_empty() {
            parts.push(cleaned.to_lowercase());
            if parts.len() >= 2 {
                break;
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("_"))
    }
}

/// Formats a descriptive operation ID using the monotonically increasing counter value,
/// tool name, and command string.
pub fn generate_id_with_details(counter_val: u64, tool_name: &str, command: &str) -> String {
    let mut details = clean_details(command);
    if details.is_none() {
        details = clean_details(tool_name);
    }
    match details {
        Some(d) => format!("op_{}_{}", counter_val, d),
        None => format!("op_{}", counter_val),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_details() {
        assert_eq!(
            clean_details("cargo nextest run"),
            Some("cargo_nextest".to_string())
        );
        assert_eq!(
            clean_details("git commit -m 'fix'"),
            Some("git_commit".to_string())
        );
        assert_eq!(
            clean_details("/bin/sh -c 'cargo test'"),
            Some("sh_cargo".to_string())
        );
        assert_eq!(clean_details("echo hello"), Some("echo_hello".to_string()));
        assert_eq!(clean_details("cargo"), Some("cargo".to_string()));
        assert_eq!(clean_details("--version"), None);
    }

    #[test]
    fn test_generate_id_with_details() {
        assert_eq!(
            generate_id_with_details(42, "cargo", "cargo build"),
            "op_42_cargo_build"
        );
        assert_eq!(
            generate_id_with_details(4, "run_terminal_command", "cargo nextest run"),
            "op_4_cargo_nextest"
        );
        assert_eq!(generate_id_with_details(5, "--flag", ""), "op_5");
    }
}
