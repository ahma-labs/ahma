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
            word.split(&['/', '\\'][..]).next_back().unwrap_or(word)
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

/// Length of the per-process generation tag in every operation id.
pub const GENERATION_LEN: usize = 4;

/// This process's generation: a random tag of [`GENERATION_LEN`] lowercase
/// letters, minted once and carried in every operation id it issues.
///
/// Operation ids are counters, and counters restart with the process. Without
/// the tag, `op_3_cargo_build` issued after a restart or an update could name a
/// *different* operation than the one an agent is still waiting on, and
/// `await` would hand it that operation's result without a word. With it, an
/// id from an earlier process is recognisably foreign, so it can be answered
/// honestly (see `mcp_service::handlers::common::unknown_operation_message`).
/// Letters only, so it can never be mistaken for the counter that follows it.
pub fn generation() -> &'static str {
    static GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GENERATION.get_or_init(|| {
        (0..GENERATION_LEN)
            .map(|_| char::from(b'a' + rand::random_range(0..26u8)))
            .collect()
    })
}

/// The generation tag of an operation id (`op_<generation>_<n>[_<details>]`),
/// or `None` for anything else, including ids minted before tags existed.
pub fn id_generation(id: &str) -> Option<&str> {
    let mut parts = id.strip_prefix("op_")?.splitn(3, '_');
    let generation = parts.next()?;
    let counter = parts.next()?;
    let tagged = generation.len() == GENERATION_LEN
        && generation.chars().all(|c| c.is_ascii_lowercase())
        && !counter.is_empty()
        && counter.chars().all(|c| c.is_ascii_digit());
    tagged.then_some(generation)
}

/// Formats a descriptive operation ID from this process's [`generation`], the
/// monotonically increasing counter value, the tool name and the command string.
pub fn generate_id_with_details(counter_val: u64, tool_name: &str, command: &str) -> String {
    let mut details = clean_details(command);
    if details.is_none() {
        details = clean_details(tool_name);
    }
    let generation = generation();
    match details {
        Some(d) => format!("op_{generation}_{counter_val}_{d}"),
        None => format!("op_{generation}_{counter_val}"),
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
        let g = generation();
        assert_eq!(
            generate_id_with_details(42, "cargo", "cargo build"),
            format!("op_{g}_42_cargo_build")
        );
        assert_eq!(
            generate_id_with_details(4, "run_terminal_command", "cargo nextest run"),
            format!("op_{g}_4_cargo_nextest")
        );
        assert_eq!(
            generate_id_with_details(5, "--flag", ""),
            format!("op_{g}_5")
        );
    }

    #[test]
    fn generation_is_stable_lowercase_and_never_numeric() {
        let g = generation();
        assert_eq!(g, generation(), "one generation per process");
        assert_eq!(g.len(), GENERATION_LEN);
        assert!(g.chars().all(|c| c.is_ascii_lowercase()), "{g}");
    }

    #[test]
    fn id_generation_reads_the_generation_back() {
        let id = generate_id_with_details(7, "cargo", "cargo build");
        assert_eq!(id_generation(&id), Some(generation()));
        assert_eq!(id_generation("op_abcd_7_cargo_build"), Some("abcd"));
        assert_eq!(id_generation("op_abcd_7"), Some("abcd"));
        // Pre-generation ids, and anything that is not an op id, have none.
        assert_eq!(id_generation("op_7_cargo_build"), None);
        assert_eq!(id_generation("op_7"), None);
        assert_eq!(id_generation("op_cargo_7"), None);
        assert_eq!(id_generation("banana"), None);
    }
}
