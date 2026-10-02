use super::*;
use serde_json::json;
use std::path::PathBuf;
use tempfile::tempdir;

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

// ── classify_grant_risk: hard denylist (Refused) ────────────────────────────

#[test]
fn refused_filesystem_root() {
    let risk = classify_grant_risk(Path::new("/"), Some(Path::new("/home/u")), &[]);
    assert!(
        matches!(risk, GrantRisk::Refused(_)),
        "root must be refused"
    );
}

#[test]
fn refused_exact_home() {
    let home = PathBuf::from("/home/u");
    let risk = classify_grant_risk(&home, Some(&home), &[]);
    match risk {
        GrantRisk::Refused(r) => assert!(r.contains("home directory"), "{r}"),
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn refused_strict_ancestor_of_live_scope() {
    let scopes = vec![PathBuf::from("/work/space/proj")];
    let risk = classify_grant_risk(
        Path::new("/work/space"),
        Some(Path::new("/home/u")),
        &scopes,
    );
    match risk {
        GrantRisk::Refused(r) => assert!(r.contains("parent of the active sandbox scope"), "{r}"),
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn allows_enclosing_git_repo_ancestor_of_live_scope() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktree_or_subdir = repo
        .join(".claude")
        .join("worktrees")
        .join("feature")
        .join("rust");
    std::fs::create_dir_all(&worktree_or_subdir).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();

    let canon_repo = dunce::canonicalize(&repo).unwrap();
    let canon_sub = dunce::canonicalize(&worktree_or_subdir).unwrap();

    let scopes = vec![canon_sub];
    let risk = classify_grant_risk(&canon_repo, Some(tmp.path()), &scopes);
    assert!(
        !matches!(risk, GrantRisk::Refused(_)),
        "enclosing git repo of a scope must not be refused: {risk:?}"
    );
}

#[test]
fn refused_credential_dirs() {
    let home = PathBuf::from("/home/u");
    for dir in [".ssh", ".aws", ".gnupg", ".kube", ".docker", ".ahma"] {
        let p = home.join(dir);
        assert!(
            matches!(
                classify_grant_risk(&p, Some(&home), &[]),
                GrantRisk::Refused(_)
            ),
            "{dir} must be refused"
        );
    }
    assert!(matches!(
        classify_grant_risk(&home.join(".config").join("gh"), Some(&home), &[]),
        GrantRisk::Refused(_)
    ));
}

#[test]
fn refused_system_dirs() {
    for d in ["/etc", "/usr", "/bin", "/var", "/System", "/Library"] {
        assert!(
            matches!(
                classify_grant_risk(Path::new(d), Some(Path::new("/home/u")), &[]),
                GrantRisk::Refused(_)
            ),
            "{d} must be refused"
        );
    }
    assert!(is_system_dir(Path::new("C:\\Windows")));
    // A subdir of a system dir is NOT itself a system dir — only exact matches.
    assert!(!is_system_dir(Path::new("/usr/local/cache")));
}

// ── classify_grant_risk: elevated (High) and ordinary (Normal) ──────────────

#[test]
fn normal_existing_dir_outside_special_paths() {
    let dir = tempdir().unwrap();
    let cache = dir.path().join("buildcache");
    std::fs::create_dir(&cache).unwrap();
    let home = tempdir().unwrap();
    let scope = tempdir().unwrap();
    let risk = classify_grant_risk(&cache, Some(home.path()), &[scope.path().to_path_buf()]);
    assert_eq!(
        risk,
        GrantRisk::Normal,
        "ordinary existing dir should be Normal"
    );
}

#[test]
fn high_for_nonexistent_path() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    match classify_grant_risk(&missing, Some(Path::new("/home/u")), &[]) {
        GrantRisk::High(w) => assert!(w.iter().any(|m| m.contains("does not exist")), "{w:?}"),
        other => panic!("expected High, got {other:?}"),
    }
}

#[test]
fn high_for_redundant_equal_scope() {
    let dir = tempdir().unwrap();
    let scope = dir.path().to_path_buf();
    match classify_grant_risk(
        &scope,
        Some(Path::new("/home/u")),
        std::slice::from_ref(&scope),
    ) {
        GrantRisk::High(w) => assert!(w.iter().any(|m| m.contains("redundant")), "{w:?}"),
        other => panic!("expected High, got {other:?}"),
    }
}

#[test]
fn high_for_hidden_home_dir_not_a_cache() {
    let home = tempdir().unwrap();
    // In production the path always arrives canonicalized (`resolve_grant_path`),
    // so canonicalize the home here to build the same shape — on macOS a tempdir
    // is `/var/folders/…`, a symlink to `/private/var/folders/…`.
    let canonical_home = dunce::canonicalize(home.path()).unwrap();
    let secret = canonical_home.join(".secrets");
    match classify_grant_risk(&secret, Some(home.path()), &[]) {
        GrantRisk::High(w) => assert!(w.iter().any(|m| m.contains("hidden directory")), "{w:?}"),
        other => panic!("expected High, got {other:?}"),
    }
}

#[test]
fn known_cache_dirs_are_recognised() {
    assert!(is_known_cache_dir(".cargo"));
    assert!(is_known_cache_dir(".rustup"));
    assert!(is_known_cache_dir(".sccache"));
    assert!(!is_known_cache_dir(".secrets"));
}

// ── render_scope_line ───────────────────────────────────────────────────────

#[test]
fn render_line_read_only_no_note() {
    let line = render_scope_line(
        Path::new("/cache/sccache"),
        ScopeAccess::Ro,
        "sandbox_grant",
        "2026-06-27",
        None,
    );
    assert_eq!(
        line,
        "{ path = \"/cache/sccache\", access = \"ro\", granted_by = \"sandbox_grant\", granted_at = \"2026-06-27\" }"
    );
}

#[test]
fn render_line_read_write_with_note() {
    let line = render_scope_line(
        Path::new("/cache/sccache"),
        ScopeAccess::Rw,
        "sandbox_grant",
        "2026-06-27",
        Some("build cache"),
    );
    assert!(line.contains("access = \"rw\""));
    assert!(line.ends_with("note = \"build cache\" }"));
}

#[test]
fn render_line_escapes_quotes_and_backslashes() {
    let line = render_scope_line(
        Path::new("C:\\weird\"path"),
        ScopeAccess::Rw,
        "sandbox_grant",
        "2026-06-27",
        None,
    );
    assert!(line.contains("C:\\\\weird\\\"path"), "{line}");
}

// ── path resolution helpers ─────────────────────────────────────────────────

#[test]
fn expand_tilde_variants() {
    let home = PathBuf::from("/home/u");
    assert_eq!(expand_tilde("~", Some(&home)), home.to_string_lossy());
    // `expand_tilde` joins onto `home`, so the expected value must be built the
    // same way — a hard-coded "/home/u/cache" uses a forward slash that differs
    // from the platform separator `home.join` produces on Windows (`\`).
    assert_eq!(
        expand_tilde("~/cache", Some(&home)),
        home.join("cache").to_string_lossy()
    );
    // No tilde — passthrough.
    assert_eq!(expand_tilde("/abs/path", Some(&home)), "/abs/path");
}

#[test]
fn clean_path_collapses_dot_and_parent() {
    assert_eq!(
        clean_path(Path::new("/a/b/../c/./d")),
        PathBuf::from("/a/c/d")
    );
}

#[test]
fn resolve_relative_joins_workspace() {
    let ws = tempdir().unwrap();
    let resolved = resolve_grant_path("sub/dir", Some(Path::new("/home/u")), Some(ws.path()));
    assert!(resolved.starts_with(ws.path()), "{resolved:?}");
    assert!(resolved.ends_with("sub/dir"));
}

// ── parse_access ────────────────────────────────────────────────────────────

#[test]
fn parse_access_defaults_to_read_only() {
    assert_eq!(parse_access(&args(&[])).unwrap(), ScopeAccess::Ro);
    assert_eq!(
        parse_access(&args(&[("access", json!("ro"))])).unwrap(),
        ScopeAccess::Ro
    );
}

#[test]
fn parse_access_read_write() {
    assert_eq!(
        parse_access(&args(&[("access", json!("rw"))])).unwrap(),
        ScopeAccess::Rw
    );
}

#[test]
fn parse_access_rejects_garbage() {
    assert!(parse_access(&args(&[("access", json!("yolo"))])).is_err());
}

// ── message builders ────────────────────────────────────────────────────────

#[test]
fn preview_text_shows_file_line_and_deny_default() {
    let text = preview_text(
        Path::new("/cache/sccache"),
        ScopeAccess::Rw,
        Path::new("/home/u/.ahma/settings.toml"),
        "{ path = \"/cache/sccache\", access = \"rw\" }",
        &GrantRisk::Normal,
    );
    assert!(text.contains("PREVIEW ONLY"));
    assert!(text.contains("/home/u/.ahma/settings.toml"));
    assert!(text.contains("confirm: true"));
    assert!(text.contains("Deny"));
}

#[test]
fn preview_text_surfaces_high_risk_warnings() {
    let text = preview_text(
        Path::new("/data"),
        ScopeAccess::Rw,
        Path::new("/home/u/.ahma/settings.toml"),
        "line",
        &GrantRisk::High(vec!["sits directly under the filesystem root".to_string()]),
    );
    assert!(text.contains("Risk: HIGH"));
    assert!(text.contains("filesystem root"));
}

#[tokio::test]
async fn handler_refuses_catastrophic_path_even_with_confirm() {
    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();
    let result = service
        .handle_sandbox_grant(
            args(&[
                ("path", json!("/")),
                ("access", json!("rw")),
                ("confirm", json!(true)),
            ]),
            crate::client_type::McpClientType::Cursor,
        )
        .await;
    let err = result.expect_err("granting the filesystem root must be refused");
    assert!(
        err.message.contains("REFUSED"),
        "error should explain the refusal: {}",
        err.message
    );
}

#[tokio::test]
async fn handler_previews_without_writing_when_unconfirmed() {
    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();
    let target = tempdir().unwrap();
    let result = service
        .handle_sandbox_grant(
            args(&[("path", json!(target.path().to_string_lossy()))]),
            crate::client_type::McpClientType::Cursor,
        )
        .await
        .expect("preview should succeed");
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    assert!(text.contains("PREVIEW ONLY"), "{text}");
    assert!(text.contains("confirm: true"), "{text}");
}

#[tokio::test]
async fn handler_autonomous_agent_does_not_self_persist_on_confirm() {
    // The in-process autonomous agent (McpClientType::Ahma) auto-approves its own
    // tool calls, so `confirm: true` must NOT write settings.toml — it must route
    // to the human approval surface instead.
    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();
    let target = tempdir().unwrap();
    let result = service
        .handle_sandbox_grant(
            args(&[
                ("path", json!(target.path().to_string_lossy())),
                ("access", json!("rw")),
                ("confirm", json!(true)),
            ]),
            crate::client_type::McpClientType::Ahma,
        )
        .await
        .expect("request should succeed (as a request, not a grant)");
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    assert!(
        text.contains("cannot widen your own sandbox"),
        "autonomous agent must be told it cannot self-grant: {text}"
    );
    assert!(
        !text.contains("✓ Granted"),
        "autonomous agent confirm must not persist a grant: {text}"
    );
    assert!(
        text.contains("A human must approve"),
        "message must direct to human approval: {text}"
    );
    // The external-client persist path is covered by the `persist_grant` unit
    // tests in `ahma_common::scope_grant`; it is not exercised here because the
    // in-process test service does not isolate HOME and would write real settings.
}

#[test]
fn schema_declares_path_access_confirm_and_note() {
    let schema = sandbox_grant_schema();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("schema must have a properties object");
    for key in ["path", "access", "confirm", "note"] {
        assert!(properties.contains_key(key), "missing property {key}");
    }

    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .expect("schema must declare required fields");
    assert_eq!(required, &vec![Value::String("path".to_string())]);

    // `access` defaults to "ro" and is constrained to the two valid values.
    let access_prop = properties.get("access").expect("access property");
    assert_eq!(
        access_prop.get("default").and_then(Value::as_str),
        Some("ro")
    );
    let access_enum = access_prop
        .get("enum")
        .and_then(Value::as_array)
        .expect("access enum");
    assert_eq!(
        access_enum,
        &vec![
            Value::String("ro".to_string()),
            Value::String("rw".to_string())
        ]
    );

    // `path` carries the `format: path` hint used by security validation.
    assert_eq!(
        properties
            .get("path")
            .and_then(|p| p.get("format"))
            .and_then(Value::as_str),
        Some("path")
    );
}

// ── resolve_grant_path: canonicalize failure fallback ───────────────────────

#[test]
fn resolve_grant_path_falls_back_to_lexical_clean_for_missing_path() {
    let base = tempdir().unwrap();
    let missing = base.path().join("nope").join("..").join("also-missing");
    // The path does not exist, so `dunce::canonicalize` fails and the code must
    // fall back to the lexical `..`-collapsing `clean_path`.
    let resolved = resolve_grant_path(&missing.to_string_lossy(), None, None);
    assert!(
        resolved.ends_with("also-missing"),
        "expected the collapsed lexical path, got {resolved:?}"
    );
    assert!(
        !resolved.to_string_lossy().contains(".."),
        "`..` should have been collapsed: {resolved:?}"
    );
}

// ── agent_requested_text (pure, both `raised` branches) ─────────────────────

#[test]
fn agent_requested_text_when_surface_raised() {
    let text = agent_requested_text(
        Path::new("/cache/x"),
        ScopeAccess::Ro,
        Path::new("/home/u/.ahma/settings.toml"),
        true,
    );
    assert!(
        text.contains("handed to the human approval surfaces"),
        "{text}"
    );
    assert!(text.contains("cannot widen your own sandbox"), "{text}");
    // Read-only access appends the `--read-only` CLI hint.
    assert!(
        text.contains("ahma sandbox grant /cache/x --read-only"),
        "{text}"
    );
}

#[test]
fn agent_requested_text_when_no_surface_attached() {
    let text = agent_requested_text(
        Path::new("/cache/x"),
        ScopeAccess::Rw,
        Path::new("/home/u/.ahma/settings.toml"),
        false,
    );
    assert!(
        text.contains("No interactive approval surface is attached"),
        "{text}"
    );
    // Read+write access must NOT append the `--read-only` flag.
    assert!(!text.contains("--read-only"), "{text}");
    assert!(text.contains("ahma sandbox grant /cache/x\n"), "{text}");
}

// ── declined_text (pure; unreachable via the handler without a mocked Peer) ─

// ── handler: argument validation errors ──────────────────────────────────────

#[tokio::test]
async fn handler_errors_when_path_missing() {
    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();
    let err = service
        .handle_sandbox_grant(args(&[]), crate::client_type::McpClientType::Cursor)
        .await
        .expect_err("a missing `path` argument must error");
    assert!(err.message.contains("requires a `path`"), "{}", err.message);
}

#[tokio::test]
async fn handler_errors_on_invalid_access_argument() {
    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();
    let target = tempdir().unwrap();
    let err = service
        .handle_sandbox_grant(
            args(&[
                ("path", json!(target.path().to_string_lossy())),
                ("access", json!("yolo")),
            ]),
            crate::client_type::McpClientType::Cursor,
        )
        .await
        .expect_err("an invalid `access` value must error before anything is written");
    assert!(err.message.contains("invalid access"), "{}", err.message);
}

// ── handler: High-risk path surfaced through the real classify+preview pipeline ─

#[tokio::test]
async fn handler_preview_surfaces_high_risk_for_nonexistent_path() {
    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();
    let base = tempdir().unwrap();
    let missing = base.path().join("does-not-exist-yet");
    let result = service
        .handle_sandbox_grant(
            args(&[("path", json!(missing.to_string_lossy()))]),
            crate::client_type::McpClientType::Cursor,
        )
        .await
        .expect("preview must succeed even for a High-risk path");
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    assert!(text.contains("PREVIEW ONLY"), "{text}");
    assert!(text.contains("Risk: HIGH"), "{text}");
    assert!(text.contains("does not exist"), "{text}");
}

// ── handler: `confirm: true` is a request, never a grant (SPEC R5.4.5) ───────

/// A client with no elicitation capability and no attached TUI — the shape of a
/// headless harness — must get nothing written, no matter what it passes.
#[tokio::test]
async fn sandbox_grant_confirm_without_elicitation_does_not_write_settings() {
    // nextest runs each test in its own process, so this env override is
    // isolated from other tests.
    let home_dir = tempdir().unwrap();
    let target = tempdir().unwrap();
    unsafe { std::env::set_var("AHMA_TEST_HOME", home_dir.path()) };

    let (service, _scope) = crate::test_utils::in_process::build_test_service()
        .await
        .unwrap();

    for client in [
        crate::client_type::McpClientType::Cursor,
        crate::client_type::McpClientType::Antigravity,
        crate::client_type::McpClientType::Ahma,
    ] {
        let result = service
            .handle_sandbox_grant(
                args(&[
                    ("path", json!(target.path().to_string_lossy())),
                    ("access", json!("rw")),
                    ("confirm", json!(true)),
                    ("note", json!("self-grant attempt")),
                ]),
                client,
            )
            .await
            .expect("a request is not an error");
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<String>();
        assert!(
            text.contains("cannot widen your own sandbox"),
            "{client:?}: {text}"
        );
        assert!(text.contains("A human must approve"), "{client:?}: {text}");
        assert!(
            !text.contains("✓"),
            "{client:?} must not report a grant: {text}"
        );
    }
    let settings = home_dir.path().join(".ahma").join("settings.toml");
    assert!(
        !settings.exists()
            || !std::fs::read_to_string(&settings)
                .unwrap()
                .contains("self-grant"),
        "nothing may be written on the model's confirm alone"
    );
    assert!(
        !service.adapter.sandbox().is_path_in_scope(target.path()),
        "the live sandbox must not widen either"
    );
    unsafe { std::env::remove_var("AHMA_TEST_HOME") };
}

/// With a human surface attached (here: a notifier standing in for the TUI
/// modal, which persists through the same chokepoint the real one uses), a
/// `confirm: true` request that the human approves ends up in the settings
/// file, audited, and applied to the live session.
#[tokio::test]
async fn sandbox_grant_human_approval_applies_live_and_persists() {
    use crate::sandbox::ScopeGrantNotifier;
    use ahma_common::config::ScopeAccess;
    use ahma_common::scope_grant::{GrantReason, NewGrant, persist_grant};

    #[derive(Debug)]
    struct ApprovingHuman {
        file: PathBuf,
    }
    #[async_trait::async_trait]
    impl ScopeGrantNotifier for ApprovingHuman {
        async fn notify_violation(
            &self,
            path: &std::path::Path,
            access: ScopeAccess,
            _reason: GrantReason,
            tool: Option<String>,
        ) {
            persist_grant(
                &self.file,
                NewGrant {
                    path,
                    access,
                    granted_by: tool,
                    granted_at: Some("2026-10-02".into()),
                    note: None,
                    surface: "tui",
                    live_scopes: &[],
                },
            )
            .unwrap();
        }
    }

    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    unsafe { std::env::set_var("AHMA_TEST_HOME", &home) };
    let settings_file = home.join(".ahma").join("settings.toml");

    let (service, _scope) = crate::test_utils::in_process::build_test_service_with_notifier(
        std::sync::Arc::new(ApprovingHuman {
            file: settings_file.clone(),
        }),
    )
    .await
    .unwrap();

    let external_dir = tmp.path().join("external_cache");
    std::fs::create_dir_all(&external_dir).unwrap();
    let canon_external = dunce::canonicalize(&external_dir).unwrap();
    assert!(!service.adapter.sandbox().is_path_in_scope(&canon_external));

    let result = service
        .handle_sandbox_grant(
            args(&[
                ("path", json!(canon_external.to_string_lossy())),
                ("access", json!("rw")),
                ("confirm", json!(true)),
            ]),
            crate::client_type::McpClientType::Cursor,
        )
        .await
        .expect("an approved request succeeds");
    unsafe { std::env::remove_var("AHMA_TEST_HOME") };

    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    assert!(text.contains("A human approved"), "{text}");
    assert!(
        service.adapter.sandbox().is_path_in_scope(&canon_external),
        "a human-approved grant widens the live session immediately (R5.4.6)"
    );
    let written = std::fs::read_to_string(&settings_file).unwrap();
    assert!(written.contains("external_cache"), "{written}");
    let audit = std::fs::read_to_string(home.join(".ahma").join("permissions-audit.jsonl"))
        .expect("the chokepoint audits every grant");
    assert!(audit.contains("\"surface\":\"tui\""), "{audit}");
}

/// The hard denylist must hold when `$HOME` reaches ahma through a symlink.
///
/// `resolve_grant_path` canonicalizes the requested path, but `ahma_home_dir()`
/// returns whatever the OS reports — and on a great many real machines that is a
/// symlink: `/home` → `/mnt/home`, an automounted corporate home, a macOS home
/// relocated to another volume. If the risk classifier compares a *resolved* path
/// against an *unresolved* home, every equality rule below it silently stops
/// matching: `$HOME` itself, `~/.ssh`, `~/.aws`, `~/.ahma` all become grantable.
///
/// A denylist that quietly stops matching is worse than no denylist, because
/// everything downstream is written assuming it held.
#[cfg(unix)]
#[test]
fn denylist_holds_when_home_is_reached_through_a_symlink() {
    let tmp = tempdir().unwrap();
    let real_home = tmp.path().join("real_home");
    std::fs::create_dir_all(real_home.join(".ssh")).unwrap();

    // `link_home` is a symlink to the real home — the shape `$HOME` often has.
    let link_home = tmp.path().join("link_home");
    std::os::unix::fs::symlink(&real_home, &link_home).unwrap();

    // The path arrives canonicalized (as resolve_grant_path leaves it); the home
    // arrives as the symlink (as ahma_home_dir leaves it).
    let canonical_home = dunce::canonicalize(&real_home).unwrap();

    assert!(
        matches!(
            classify_grant_risk(&canonical_home, Some(&link_home), &[]),
            GrantRisk::Refused(_)
        ),
        "granting $HOME must be refused even when $HOME is a symlink"
    );

    let canonical_ssh = dunce::canonicalize(real_home.join(".ssh")).unwrap();
    assert!(
        matches!(
            classify_grant_risk(&canonical_ssh, Some(&link_home), &[]),
            GrantRisk::Refused(_)
        ),
        "granting ~/.ssh must be refused even when $HOME is a symlink"
    );
}
