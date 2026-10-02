//! End-to-end coverage of the question ladder (SPEC R-PERM.3) through the *real*
//! denial path: a sandboxed command trips the kernel, the adapter's notifier is
//! the [`PermissionBroker`], and the answer lands in the ledger.
//!
//! The broker's unit tests pin the ladder's *logic* (which rung, when, and why).
//! What they cannot show is that the ladder is actually wired to the thing that
//! detects denials — a ladder nobody climbs is just a data structure. These tests
//! drive `notify_stderr_denial` / `notify_pre_exec`, the same helpers the adapter
//! calls, and assert on the two outcomes that matter:
//!
//!   * an `always` approval is **persisted**, bound to the asking workspace (so
//!     the next start, or the next hooked command, actually works), and
//!   * it is **applied to the live session** beside the committed workspace
//!     scope, which itself never changes (R5.1 lock-once, R5.4.6).

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ahma_common::config::{AhmaSettings, ScopeAccess};
use ahma_common::scope_grant::{GrantCoordinator, GrantDecision, ScopeGrantRequest};
use ahma_mcp::sandbox::{
    ElicitOutcome, ElicitationSurface, PermissionBroker, Sandbox, SandboxMode, ScopeGrantNotifier,
    grant_channel::notify_pre_exec,
};
use async_trait::async_trait;
use tempfile::TempDir;

/// A harness that answers with a canned outcome and counts the asks.
#[derive(Debug)]
struct ScriptedHarness {
    outcomes: Mutex<Vec<ElicitOutcome>>,
    asks: AtomicUsize,
}

impl ScriptedHarness {
    fn new(outcomes: Vec<ElicitOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            asks: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ElicitationSurface for ScriptedHarness {
    async fn ask(&self, _req: &ScopeGrantRequest) -> ElicitOutcome {
        self.asks.fetch_add(1, Ordering::SeqCst);
        let mut q = self.outcomes.lock();
        if q.is_empty() {
            ElicitOutcome::Unavailable
        } else {
            q.remove(0)
        }
    }
}

fn test_sandbox(scope: &Path) -> Sandbox {
    Sandbox::new(
        vec![scope.to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap()
}

fn ledger(home: &Path) -> PathBuf {
    home.join(".ahma").join("settings.toml")
}

#[tokio::test]
async fn an_approval_at_the_harness_is_persisted_and_applied_beside_the_locked_scope() {
    let home = TempDir::new().unwrap();
    // SAFETY: single-test binary; set before any settings access.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let workspace = TempDir::new().unwrap();
    let sandbox = Arc::new(test_sandbox(workspace.path()));
    let primary_before = sandbox.scopes().first().cloned();

    let harness = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::GrantRw)]);
    let broker = Arc::new(PermissionBroker::new(
        Arc::new(GrantCoordinator::new()),
        None, // no TUI attached: the harness is the only surface above fail-closed
    ));
    broker.set_elicitation_surface(harness.clone());
    broker.set_sandbox(sandbox.clone());
    let notifier: Arc<dyn ScopeGrantNotifier> = broker.clone();

    // A real directory outside the workspace, refused up front by path
    // validation — the exact path, the same helper the adapter calls. A real
    // temporary directory rather than a literal like `/opt/…`, which is not an
    // absolute path on Windows and so names a different place there.
    let outside = TempDir::new().unwrap();
    let cache = outside.path().join("sccache");
    std::fs::create_dir_all(&cache).unwrap();
    let cache = dunce::canonicalize(&cache).unwrap();
    let err: anyhow::Error = ahma_mcp::sandbox::SandboxError::PathOutsideSandbox {
        path: cache.clone(),
        scopes: vec![workspace.path().to_path_buf()],
    }
    .into();
    notify_pre_exec(&sandbox, Some(&notifier), &err, "sccache").await;

    assert_eq!(
        harness.asks.load(Ordering::SeqCst),
        1,
        "the denial reached the ladder and the harness was asked"
    );

    // The approval is in the ledger, bound to the workspace that asked.
    let settings = AhmaSettings::load_from_result(&ledger(home.path()))
        .expect("the ledger was written and parses");
    let granted = settings
        .sandbox
        .find_scope(&cache)
        .expect("the approved scope is persisted");
    assert_eq!(granted.access, ScopeAccess::Rw);
    assert_eq!(
        granted.workspace.as_deref(),
        primary_before.as_deref(),
        "an always grant is bound to the asking workspace (R5.4.11)"
    );

    // Applied live beside the workspace scope, which itself never moves.
    assert!(
        sandbox.is_path_in_scope(&cache),
        "a human-approved grant applies to this session now (R5.4.6)"
    );
    assert_eq!(
        sandbox.scopes().first().cloned(),
        primary_before,
        "the committed workspace scope is never replaced (R5.1)"
    );

    // And it is auditable.
    let audit = home.path().join(".ahma").join("permissions-audit.jsonl");
    let text = std::fs::read_to_string(&audit).expect("the grant is audited");
    assert!(text.contains("sccache"), "audit names the subject: {text}");
    assert!(text.contains("grant"), "audit names the action: {text}");
}

#[tokio::test]
async fn a_declined_question_persists_nothing_and_is_not_asked_again() {
    let home = TempDir::new().unwrap();
    // SAFETY: single-test binary.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let workspace = TempDir::new().unwrap();

    let harness = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::Deny)]);
    let broker = Arc::new(PermissionBroker::new(
        Arc::new(GrantCoordinator::new()),
        None,
    ));
    broker.set_elicitation_surface(harness.clone());
    let notifier: Arc<dyn ScopeGrantNotifier> = broker.clone();

    let err: anyhow::Error = ahma_mcp::sandbox::SandboxError::PathOutsideSandbox {
        path: PathBuf::from("/opt/ext/cache"),
        scopes: vec![workspace.path().to_path_buf()],
    }
    .into();

    // The kernel will trip on this path again and again; the human is asked once.
    let sandbox = test_sandbox(workspace.path());
    notify_pre_exec(&sandbox, Some(&notifier), &err, "cargo_build").await;
    notify_pre_exec(&sandbox, Some(&notifier), &err, "cargo_build").await;
    notify_pre_exec(&sandbox, Some(&notifier), &err, "cargo_build").await;

    assert_eq!(
        harness.asks.load(Ordering::SeqCst),
        1,
        "a declined path must not re-prompt — being nagged for saying no is exactly \
         the hassle this model exists to prevent (R-PERM.4)"
    );

    let settings = AhmaSettings::load_from(&ledger(home.path()));
    assert!(
        settings.sandbox.persistent_scopes.is_empty(),
        "a decline must persist nothing at all"
    );
}
