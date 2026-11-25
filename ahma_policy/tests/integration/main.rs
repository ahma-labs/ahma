//! Team policy in Cedar (SPEC R-POLICY.3–5).

use ahma_common::policy::{Action, PolicyOverlay, Request, Resource};
use ahma_policy::{LoadError, TeamPolicy, host_chain, path_chain};
use std::path::{Path, PathBuf};

fn policy(src: &str) -> TeamPolicy {
    TeamPolicy::from_sources(&[(PathBuf::from("team.cedar"), src.to_string())])
        .unwrap_or_else(|e| panic!("policy should load: {e}"))
}

fn write(path: &str) -> Request<'_> {
    Request::new(Action::Write, Resource::Path(Path::new(path)))
}

#[test]
fn a_forbid_on_a_directory_covers_everything_under_it() {
    let p = policy(
        r#"@reason("credentials stay unwritable")
        forbid(principal, action == Ahma::Action::"write", resource in Ahma::Path::"/home/me/.aws");"#,
    );
    assert_eq!(
        p.forbids(&write("/home/me/.aws/credentials")).as_deref(),
        Some("credentials stay unwritable")
    );
    assert_eq!(
        p.forbids(&write("/home/me/.aws")).as_deref(),
        Some("credentials stay unwritable")
    );
    assert_eq!(
        p.forbids(&write("/home/me/.awsx/file")),
        None,
        "a sibling is not inside"
    );
    assert_eq!(
        p.forbids(&Request::new(
            Action::Read,
            Resource::Path(Path::new("/home/me/.aws/x"))
        )),
        None,
        "only the action named"
    );
}

#[test]
fn a_forbid_on_a_domain_covers_its_subdomains() {
    let p = policy(
        r#"forbid(principal, action in [Ahma::Action::"connect", Ahma::Action::"fetch"], resource in Ahma::Host::"pastebin.com");"#,
    );
    let connect = |h| Request::new(Action::Connect, Resource::Host(h));
    assert!(p.forbids(&connect("pastebin.com")).is_some());
    assert!(p.forbids(&connect("api.pastebin.com")).is_some());
    assert!(p.forbids(&connect("github.com")).is_none());
    let reason = p.forbids(&connect("pastebin.com")).unwrap();
    assert!(
        reason.contains("team#0"),
        "no @reason names the policy: {reason}"
    );
}

#[test]
fn a_condition_can_use_the_workspace() {
    let p = policy(
        r#"forbid(principal, action == Ahma::Action::"call_tool", resource == Ahma::Tool::"agent")
        when { principal has workspace && principal.workspace like "/srv/prod/*" };"#,
    );
    let tool = Request::new(Action::CallTool, Resource::Tool("agent"));
    assert!(
        p.forbids(&tool.in_workspace(Path::new("/srv/prod/api")))
            .is_some()
    );
    assert!(
        p.forbids(&tool.in_workspace(Path::new("/home/me/play")))
            .is_none()
    );
    assert!(p.forbids(&tool).is_none(), "no workspace, no match");
}

#[test]
fn a_permit_is_refused_at_load() {
    let err = TeamPolicy::from_sources(&[(
        PathBuf::from("wide.cedar"),
        r#"permit(principal, action, resource);"#.into(),
    )])
    .unwrap_err();
    assert!(matches!(err, LoadError::Permit { .. }), "{err}");
}

#[test]
fn a_file_that_does_not_parse_is_refused() {
    let err =
        TeamPolicy::from_sources(&[(PathBuf::from("bad.cedar"), "forbid(".into())]).unwrap_err();
    assert!(matches!(err, LoadError::Parse { .. }), "{err}");
    assert!(err.to_string().contains("bad.cedar"), "{err}");
}

#[test]
fn a_policy_naming_what_the_schema_does_not_know_is_refused() {
    let err = TeamPolicy::from_sources(&[(
        PathBuf::from("typo.cedar"),
        r#"forbid(principal, action == Ahma::Action::"delete", resource);"#.into(),
    )])
    .unwrap_err();
    assert!(matches!(err, LoadError::Invalid(_)), "{err}");
}

#[test]
fn ids_come_from_the_file_or_an_id_annotation() {
    let p = TeamPolicy::from_sources(&[
        (
            PathBuf::from("/etc/ahma/policy/a.cedar"),
            r#"forbid(principal, action == Ahma::Action::"fetch", resource in Ahma::Host::"a.example");"#.into(),
        ),
        (
            PathBuf::from("/etc/ahma/policy/b.cedar"),
            r#"@id("no-b") forbid(principal, action == Ahma::Action::"fetch", resource in Ahma::Host::"b.example");"#.into(),
        ),
    ])
    .unwrap();
    let ids: Vec<&str> = p.rules().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["a#0", "no-b"]);
}

#[test]
fn no_files_means_no_policy() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        TeamPolicy::load(&[dir.path().to_path_buf()])
            .unwrap()
            .is_none()
    );
    assert!(
        TeamPolicy::load(&[dir.path().join("missing")])
            .unwrap()
            .is_none()
    );
}

#[test]
fn files_are_read_from_every_directory() {
    let user = tempfile::tempdir().unwrap();
    let system = tempfile::tempdir().unwrap();
    std::fs::write(
        user.path().join("mine.cedar"),
        r#"forbid(principal, action == Ahma::Action::"fetch", resource in Ahma::Host::"one.example");"#,
    )
    .unwrap();
    std::fs::write(
        system.path().join("org.cedar"),
        r#"forbid(principal, action == Ahma::Action::"fetch", resource in Ahma::Host::"two.example");"#,
    )
    .unwrap();
    std::fs::write(system.path().join("notes.txt"), "not policy").unwrap();
    let p = TeamPolicy::load(&[user.path().to_path_buf(), system.path().to_path_buf()])
        .unwrap()
        .unwrap();
    assert_eq!(p.rules().len(), 2);
    let fetch = |h| Request::new(Action::Fetch, Resource::Host(h));
    assert!(p.forbids(&fetch("one.example")).is_some());
    assert!(p.forbids(&fetch("two.example")).is_some());
}

#[test]
fn chains_run_from_the_resource_to_the_root() {
    assert_eq!(path_chain(Path::new("/a/b")), ["/a/b", "/a", "/"]);
    assert_eq!(
        host_chain("API.GitHub.com."),
        ["api.github.com", "github.com", "com"]
    );
}

mod export {
    use ahma_common::config::{AhmaSettings, PersistentScope, ScopeAccess};
    use ahma_policy::export::{export, validate_text, verify};
    use std::path::PathBuf;

    /// The export is valid Cedar against ahma's schema, permits included, so
    /// a reviewer can run Cedar's own tooling on it (SPEC R-POLICY.6).
    #[test]
    fn the_export_is_valid_cedar_for_ahmas_schema() {
        let mut settings = AhmaSettings::default();
        settings.web.never_allow =
            vec!["*.pastebin.com".into(), "https://evil.example:8443".into()];
        settings.web.always_allow = vec!["api.github.com".into(), "*".into()];
        settings.network.allow = vec!["index.crates.io".into()];
        settings.sandbox.persistent_scopes = vec![
            PersistentScope {
                path: PathBuf::from("/opt/cache"),
                access: ScopeAccess::Ro,
                workspace: Some(PathBuf::from("/home/me/proj")),
                granted_by: None,
                granted_at: None,
                note: None,
                expires_at: None,
            },
            PersistentScope {
                path: PathBuf::from("/srv/data"),
                access: ScopeAccess::Rw,
                workspace: None,
                granted_by: None,
                granted_at: None,
                note: None,
                expires_at: None,
            },
        ];
        let text = export(&settings, Some(std::path::Path::new("/home/me")));
        validate_text(&text).unwrap_or_else(|e| panic!("export must validate: {e}\n{text}"));
        assert!(
            text.contains(r#"resource in Ahma::Path::"/home/me/.aws""#),
            "{text}"
        );
        assert!(
            text.contains(r#"principal.workspace == "/home/me/proj""#),
            "{text}"
        );
        assert!(
            text.contains(r#"resource in Ahma::Host::"pastebin.com""#),
            "{text}"
        );
        assert!(text.contains("scheme and port are not modelled"), "{text}");
    }

    /// The export contains permits, so it can never be loaded as team policy.
    #[test]
    fn the_export_cannot_be_loaded_back_as_team_policy() {
        let mut settings = AhmaSettings::default();
        settings.network.allow = vec!["example.com".into()];
        let text = export(&settings, None);
        let err =
            ahma_policy::TeamPolicy::from_sources(&[(PathBuf::from("x.cedar"), text)]).unwrap_err();
        assert!(
            matches!(err, ahma_policy::LoadError::Permit { .. }),
            "{err}"
        );
    }

    #[test]
    fn verify_holds_for_a_fresh_home() {
        let home = tempfile::tempdir().unwrap();
        let home = dunce::canonicalize(home.path()).unwrap();
        let checks = verify(Some(&home));
        assert!(checks.len() >= 5, "{checks:?}");
        for c in &checks {
            assert!(c.held, "invariant failed: {}", c.claim);
        }
    }
}
