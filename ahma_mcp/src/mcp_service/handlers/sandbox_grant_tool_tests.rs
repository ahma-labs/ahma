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
    let secret = home.path().join(".secrets");
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

#[test]
fn success_text_added_points_to_restart() {
    let text = success_text(
        Path::new("/cache/sccache"),
        ScopeAccess::Rw,
        Path::new("/home/u/.ahma/settings.toml"),
        "line",
        &GrantOutcome::Added,
        &GrantRisk::Normal,
    );
    assert!(text.contains("✓ Granted"));
    assert!(text.contains("restart"));
    assert!(text.contains("next server start"));
}

// ── handler integration (real service, real scopes) ─────────────────────────

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
        text.contains("cannot widen its own sandbox"),
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
