//! Per-session worker options (SPEC R-DAEMON.4).
//!
//! An editor's `mcp.json` can ask for things that change how *its* work runs:
//! `--tools simplify`, `--sandbox-scope`, `--no-sandbox`, a task vault. Those
//! used to be baked into the shared bridge at spawn time by whichever client
//! happened to start it, and every later client silently inherited them — so
//! the second window's `--tools` was ignored, and, worse, the first window's
//! `--no-sandbox` unsandboxed everybody.
//!
//! With one daemon per user that is no longer tenable, so the options travel
//! **with the session**: the frontend encodes them into the MCP URL's query,
//! and the daemon turns them back into arguments for that session's worker
//! alone. Forwarding per session is strictly narrower than the inheritance it
//! replaces — anything that can reach the socket already runs as this user and
//! could simply run `ahma --no-sandbox` itself.
//!
//! The allowlist is exhaustive on purpose: an unknown name is refused rather
//! than ignored, so a typo in a client config is a loud error instead of a flag
//! that silently did nothing.

use crate::shell::cli::AppConfig;

/// A per-session option: the query name, the worker flag it becomes, and
/// whether it carries a value.
struct Opt {
    query: &'static str,
    flag: &'static str,
    takes_value: bool,
    repeatable: bool,
}

/// Every option a session may carry.
///
/// Deliberately *not* here: rate limits, bearer tokens, handshake and idle
/// timeouts. Those govern the daemon as a whole, and letting one session's URL
/// change them would let one client reconfigure everyone else's.
const OPTIONS: &[Opt] = &[
    Opt {
        query: "sandbox_scope",
        flag: "--sandbox-scope",
        takes_value: true,
        repeatable: true,
    },
    Opt {
        query: "working_dir",
        flag: "--working-dir",
        takes_value: true,
        repeatable: true,
    },
    Opt {
        query: "tools_dir",
        flag: "--tools-dir",
        takes_value: true,
        repeatable: false,
    },
    Opt {
        query: "task_vault",
        flag: "--task-vault",
        takes_value: true,
        repeatable: false,
    },
    Opt {
        query: "tools",
        flag: "--tools",
        takes_value: true,
        repeatable: true,
    },
    Opt {
        query: "instance_label",
        flag: "--instance-label",
        takes_value: true,
        repeatable: false,
    },
    Opt {
        query: "no_sandbox",
        flag: "--no-sandbox",
        takes_value: false,
        repeatable: false,
    },
    Opt {
        query: "skip_probes",
        flag: "--skip-probes",
        takes_value: false,
        repeatable: false,
    },
    Opt {
        query: "sync",
        flag: "--sync",
        takes_value: false,
        repeatable: false,
    },
    Opt {
        query: "scratch",
        flag: "--scratch",
        takes_value: false,
        repeatable: false,
    },
    Opt {
        query: "tmp",
        flag: "--tmp",
        takes_value: false,
        repeatable: false,
    },
    Opt {
        query: "disable_temp_files",
        flag: "--disable-temp-files",
        takes_value: false,
        repeatable: false,
    },
];

/// Encode the options this client asked for into URL query form.
///
/// Returns an empty string when there is nothing to say, so the common case
/// produces a plain `/mcp` URL.
pub fn session_query_from_config(config: &AppConfig) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();

    for scope in &config.sandbox_scopes {
        pairs.push(("sandbox_scope".into(), scope.to_string_lossy().into_owned()));
    }
    for dir in &config.working_dirs {
        pairs.push(("working_dir".into(), dir.to_string_lossy().into_owned()));
    }
    if config.explicit_tools_dir
        && let Some(dir) = &config.tools_dir
    {
        pairs.push(("tools_dir".into(), dir.to_string_lossy().into_owned()));
    }
    if let Some(vault) = &config.task_vault {
        pairs.push(("task_vault".into(), vault.to_string_lossy().into_owned()));
    }
    for bundle in &config.tool_bundles {
        pairs.push(("tools".into(), bundle.clone()));
    }
    if !config.instance_label.is_empty() {
        pairs.push(("instance_label".into(), config.instance_label.clone()));
    }
    for (name, enabled) in [
        ("no_sandbox", config.no_sandbox),
        ("skip_probes", config.skip_availability_probes),
        ("sync", config.force_sync),
        ("scratch", config.use_scratch_dir),
        ("tmp", config.tmp_access),
        ("disable_temp_files", config.no_temp_files),
    ] {
        if enabled {
            pairs.push((name.into(), "1".into()));
        }
    }

    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(&v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Turn a session's query pairs into worker arguments.
///
/// An unrecognised name is an error, not a shrug: a client that misspells an
/// option should be told, not silently served something else.
pub fn session_query_to_worker_args(pairs: &[(String, String)]) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (name, value) in pairs {
        let Some(opt) = OPTIONS.iter().find(|o| o.query == name) else {
            return Err(format!(
                "unknown session option `{name}`. Known options: {}",
                OPTIONS
                    .iter()
                    .map(|o| o.query)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        if !opt.repeatable && seen.contains(&opt.query) {
            return Err(format!("session option `{name}` may be given only once"));
        }
        seen.push(opt.query);
        if opt.takes_value {
            if value.is_empty() {
                return Err(format!("session option `{name}` needs a value"));
            }
            args.push(opt.flag.to_string());
            args.push(value.clone());
        } else {
            // A flag is present or absent; `?no_sandbox=0` meaning "on" would
            // be a trap, so only a truthy value turns it on.
            if is_truthy(value) {
                args.push(opt.flag.to_string());
            }
        }
    }
    Ok(args)
}

fn is_truthy(value: &str) -> bool {
    matches!(value, "1" | "true" | "yes" | "on" | "")
}

/// Percent-encode everything that is not unreserved, so a path with a space,
/// an `&`, or a `#` survives the round trip.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Decode a query string into pairs. Tolerates an empty string.
pub fn parse_session_query(query: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = match part.split_once('=') {
            Some((n, v)) => (n, v),
            None => (part, ""),
        };
        out.push((name.to_string(), percent_decode(value)?));
    }
    Ok(out)
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = value
                    .get(i + 1..i + 3)
                    .ok_or_else(|| "truncated percent-escape in session options".to_string())?;
                let byte = u8::from_str_radix(hex, 16)
                    .map_err(|_| format!("invalid percent-escape `%{hex}` in session options"))?;
                out.push(byte);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "session options must be UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(query: &str) -> Vec<String> {
        let pairs = parse_session_query(query).expect("query parses");
        session_query_to_worker_args(&pairs).expect("options are known")
    }

    /// Every allowlisted option becomes the flag it stands for.
    #[test]
    fn each_allowlisted_option_becomes_its_flag() {
        let args = args_for(
            "sandbox_scope=%2Fwork%2Fproj&tools=simplify&tools=rust&tools_dir=%2Fopt%2Ftools\
             &task_vault=%2Fv&working_dir=%2Fwd&instance_label=cursor\
             &no_sandbox=1&skip_probes=1&sync=1&scratch=1&tmp=1&disable_temp_files=1",
        );
        for expected in [
            "--sandbox-scope",
            "/work/proj",
            "--tools",
            "simplify",
            "--tools-dir",
            "/opt/tools",
            "--task-vault",
            "/v",
            "--working-dir",
            "/wd",
            "--instance-label",
            "cursor",
            "--no-sandbox",
            "--skip-probes",
            "--sync",
            "--scratch",
            "--tmp",
            "--disable-temp-files",
        ] {
            assert!(
                args.contains(&expected.to_string()),
                "{expected} missing from {args:?}"
            );
        }
        assert_eq!(
            args.iter().filter(|a| *a == "--tools").count(),
            2,
            "a repeatable option keeps every value: {args:?}"
        );
    }

    /// A misspelled option is refused. Ignoring it would serve the client
    /// something other than what it asked for, silently.
    #[test]
    fn an_unknown_option_is_refused_and_the_error_lists_the_known_ones() {
        let pairs = parse_session_query("sandbox_scpoe=%2Fwork").unwrap();
        let err = session_query_to_worker_args(&pairs).expect_err("typo must be refused");
        assert!(err.contains("sandbox_scpoe"), "{err}");
        assert!(
            err.contains("sandbox_scope"),
            "the error must name the real option: {err}"
        );
    }

    /// Daemon-wide settings are not session options: one client must not be
    /// able to change the handshake timeout or the bearer token for everyone.
    #[test]
    fn daemon_wide_settings_are_not_session_options() {
        for name in [
            "require_token",
            "rate_limit_rps",
            "handshake_timeout",
            "idle_timeout",
            "max_sessions",
        ] {
            let pairs = parse_session_query(&format!("{name}=5")).unwrap();
            assert!(
                session_query_to_worker_args(&pairs).is_err(),
                "{name} must not be settable per session"
            );
        }
    }

    /// A flag with a falsy value stays off, so `?no_sandbox=0` cannot become
    /// "sandboxing disabled".
    #[test]
    fn a_falsy_flag_stays_off() {
        assert!(args_for("no_sandbox=0").is_empty());
        assert!(args_for("no_sandbox=false").is_empty());
        assert_eq!(args_for("no_sandbox=1"), vec!["--no-sandbox".to_string()]);
    }

    #[test]
    fn a_value_option_with_no_value_is_refused() {
        let pairs = parse_session_query("sandbox_scope=").unwrap();
        assert!(session_query_to_worker_args(&pairs).is_err());
    }

    /// Paths with characters that mean something in a URL survive the trip.
    #[test]
    fn awkward_paths_round_trip() {
        let config = AppConfig {
            sandbox_scopes: vec![std::path::PathBuf::from("/work/my project&x=1")],
            instance_label: String::new(),
            ..AppConfig::default()
        };
        let query = session_query_from_config(&config);
        let args = args_for(&query);
        assert_eq!(
            args,
            vec![
                "--sandbox-scope".to_string(),
                "/work/my project&x=1".to_string()
            ]
        );
    }

    /// The common case is a plain URL: nothing configured, nothing sent.
    #[test]
    fn a_default_config_sends_nothing() {
        let config = AppConfig {
            instance_label: String::new(),
            ..AppConfig::default()
        };
        assert_eq!(session_query_from_config(&config), "");
    }

    /// What the shared bridge used to inherit process-wide now travels with the
    /// session that asked for it.
    #[test]
    fn a_clients_own_flags_reach_only_its_own_worker() {
        let config = AppConfig {
            tool_bundles: vec!["simplify".into()],
            use_scratch_dir: true,
            instance_label: String::new(),
            ..AppConfig::default()
        };
        let args = args_for(&session_query_from_config(&config));
        assert_eq!(
            args,
            vec![
                "--tools".to_string(),
                "simplify".to_string(),
                "--scratch".to_string()
            ]
        );
    }
}
