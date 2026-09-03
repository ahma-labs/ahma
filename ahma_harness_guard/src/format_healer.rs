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

/// Heals semantic issues in arguments (e.g. converting a string parameter to a singleton array).
pub fn heal_tool_arguments(tool_name: &str, args: &mut serde_json::Map<String, Value>) {
    if tool_name == "run_terminal_command"
        && let Some(args_val) = args.get_mut("args")
        && let Value::String(s) = args_val
    {
        *args_val = Value::Array(vec![Value::String(s.clone())]);
    }
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

/// Attempts to correct minor typos in tool names.
pub fn heal_tool_name(requested_name: &str, known_names: &[&str]) -> Option<String> {
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
