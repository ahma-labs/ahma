use serde_json::Value;

/// Resolves trailing commas in a JSON string.
pub fn clean_json_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ',' {
            let mut temp = chars.clone();
            let mut is_trailing = false;
            while let Some(&next) = temp.peek() {
                if next.is_whitespace() {
                    temp.next();
                } else if next == '}' || next == ']' {
                    is_trailing = true;
                    break;
                } else {
                    break;
                }
            }
            if is_trailing {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Other harnesses' names for ahma's tools. Models are trained on Claude
/// Code's (`Read`, `Edit`, `Grep`…), Anthropic's text editor (`str_replace`)
/// and Codex's (`apply_patch`, `shell`); a call under one of those names is a
/// call for the ahma tool that does the same thing, not a typo to reject.
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("Read", "read_file"),
    ("view", "read_file"),
    ("Write", "write_file"),
    ("create", "write_file"),
    ("Edit", "replace_in_file"),
    ("str_replace", "replace_in_file"),
    ("MultiEdit", "multi_edit"),
    ("Grep", "grep_search"),
    ("grep", "grep_search"),
    ("Glob", "file_search"),
    ("glob", "file_search"),
    ("LS", "list_dir"),
    ("ls", "list_dir"),
    ("WebFetch", "fetch_webpage"),
    ("TodoWrite", "todo_write"),
    ("Bash", "run_terminal_command"),
    ("shell", "run_terminal_command"),
    ("applyPatch", "apply_patch"),
    ("patch", "apply_patch"),
];

/// Argument names those harnesses use, renamed to ahma's for each tool. Only
/// fills a name the call did not already give.
fn rename_args(tool_name: &str, args: &mut serde_json::Map<String, Value>) {
    let renames: &[(&str, &str)] = match tool_name {
        "read_file" | "write_file" | "list_dir" => &[("file_path", "path")],
        "replace_in_file" => &[
            ("file_path", "path"),
            ("old_string", "old_str"),
            ("new_string", "new_str"),
        ],
        "multi_edit" => &[("file_path", "path")],
        "grep_search" => &[
            ("path", "base_dir"),
            ("glob", "include_pattern"),
            ("-C", "context"),
        ],
        "file_search" => &[("path", "base_dir")],
        "apply_patch" => &[("input", "patch")],
        "fetch_webpage" => &[("prompt", "query")],
        _ => &[],
    };
    for (from, to) in renames {
        if !args.contains_key(*to)
            && let Some(v) = args.remove(*from)
        {
            args.insert((*to).to_string(), v);
        }
    }
    // Claude Code's Grep takes a ripgrep regex in `pattern`.
    if tool_name == "grep_search"
        && !args.contains_key("query")
        && let Some(p) = args.remove("pattern")
    {
        args.insert("query".into(), p);
        args.entry("is_regex").or_insert(Value::Bool(true));
    }
    if tool_name == "grep_search"
        && let Some(Value::Bool(insensitive)) = args.remove("-i")
    {
        args.entry("case_sensitive")
            .or_insert(Value::Bool(!insensitive));
    }
    if tool_name == "multi_edit"
        && let Some(Value::Array(edits)) = args.get_mut("edits")
    {
        for edit in edits.iter_mut().filter_map(Value::as_object_mut) {
            for (from, to) in [("old_string", "old_str"), ("new_string", "new_str")] {
                if !edit.contains_key(to)
                    && let Some(v) = edit.remove(from)
                {
                    edit.insert(to.to_string(), v);
                }
            }
        }
    }
}

/// Heals semantic issues in arguments (e.g. converting a string parameter to a
/// singleton array, or another harness's argument names).
pub fn heal_tool_arguments(tool_name: &str, args: &mut serde_json::Map<String, Value>) {
    if tool_name == "run_terminal_command"
        && let Some(args_val) = args.get_mut("args")
        && let Value::String(s) = args_val
    {
        *args_val = Value::Array(vec![Value::String(s.clone())]);
    }
    rename_args(tool_name, args);
}

/// Levenshtein distance implementation for string fuzzy matching.
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let mut dp = vec![vec![0; b_chars.len() + 1]; a_chars.len() + 1];

    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, val) in dp[0].iter_mut().enumerate() {
        *val = j;
    }

    for i in 1..=a_chars.len() {
        for j in 1..=b_chars.len() {
            if a_chars[i - 1] == b_chars[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            } else {
                dp[i][j] =
                    1 + std::cmp::min(dp[i - 1][j - 1], std::cmp::min(dp[i - 1][j], dp[i][j - 1]));
            }
        }
    }

    dp[a_chars.len()][b_chars.len()]
}

/// Attempts to correct minor typos in tool names, and maps other harnesses'
/// names for the same tools (`TOOL_ALIASES`). A name that is itself a known
/// tool is never remapped.
pub fn heal_tool_name(requested_name: &str, known_names: &[&str]) -> Option<String> {
    if known_names.contains(&requested_name) {
        return Some(requested_name.to_string());
    }
    if let Some((_, canonical)) = TOOL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == requested_name)
        .filter(|(_, canonical)| known_names.contains(canonical))
    {
        return Some((*canonical).to_string());
    }
    let mut best_match: Option<(&str, usize)> = None;
    for &known in known_names {
        let dist = levenshtein_distance(requested_name, known);
        if dist <= 2 {
            match best_match {
                None => best_match = Some((known, dist)),
                Some((_, best_dist)) if dist < best_dist => best_match = Some((known, dist)),
                _ => {}
            }
        }
    }
    best_match.map(|(name, _)| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn other_harnesses_names_reach_the_same_tool() {
        let known = ["read_file", "replace_in_file", "grep_search", "apply_patch"];
        assert_eq!(
            heal_tool_name("Edit", &known).as_deref(),
            Some("replace_in_file")
        );
        assert_eq!(
            heal_tool_name("str_replace", &known).as_deref(),
            Some("replace_in_file")
        );
        assert_eq!(heal_tool_name("Read", &known).as_deref(), Some("read_file"));
        assert_eq!(
            heal_tool_name("patch", &known).as_deref(),
            Some("apply_patch")
        );
        // An alias for a tool this server does not have is not invented.
        assert_eq!(heal_tool_name("WebFetch", &known), None);
        // A real tool that happens to share an alias's name is left alone.
        assert_eq!(
            heal_tool_name("grep", &["grep", "grep_search"]).as_deref(),
            Some("grep")
        );
    }

    #[test]
    fn other_harnesses_argument_names_are_renamed() {
        let mut args = json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"})
            .as_object()
            .cloned()
            .unwrap();
        heal_tool_arguments("replace_in_file", &mut args);
        assert_eq!(
            Value::Object(args),
            json!({"path": "a.rs", "old_str": "x", "new_str": "y"})
        );

        let mut grep = json!({"pattern": "fn \\w+", "path": "src", "glob": "*.rs", "-i": true})
            .as_object()
            .cloned()
            .unwrap();
        heal_tool_arguments("grep_search", &mut grep);
        assert_eq!(grep["query"], json!("fn \\w+"));
        assert_eq!(grep["is_regex"], json!(true));
        assert_eq!(grep["base_dir"], json!("src"));
        assert_eq!(grep["include_pattern"], json!("*.rs"));
        assert_eq!(grep["case_sensitive"], json!(false));

        let mut multi =
            json!({"file_path": "a", "edits": [{"old_string": "1", "new_string": "2"}]})
                .as_object()
                .cloned()
                .unwrap();
        heal_tool_arguments("multi_edit", &mut multi);
        assert_eq!(multi["edits"][0], json!({"old_str": "1", "new_str": "2"}));
    }

    #[test]
    fn test_clean_trailing_commas() {
        let bad_json = "{\n  \"a\": 1,\n  \"b\": 2,\n}";
        assert_eq!(
            clean_json_trailing_commas(bad_json),
            "{\n  \"a\": 1,\n  \"b\": 2\n}"
        );
    }

    #[test]
    fn test_heal_tool_arguments() {
        let mut args = serde_json::Map::new();
        args.insert("args".to_string(), json!("cargo test"));
        heal_tool_arguments("run_terminal_command", &mut args);
        assert_eq!(args.get("args").unwrap(), &json!(["cargo test"]));
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(
            levenshtein_distance("run_terminal_command", "run_terminal_cmd"),
            4
        );
        assert_eq!(levenshtein_distance("await", "wait"), 1);
    }

    #[test]
    fn test_heal_tool_name() {
        let known = vec!["run_terminal_command", "await", "status"];
        assert_eq!(heal_tool_name("wait", &known), Some("await".to_string()));
        assert_eq!(
            heal_tool_name("run_terminal_commnd", &known),
            Some("run_terminal_command".to_string())
        );
        assert_eq!(heal_tool_name("completely_different", &known), None);
    }
}
