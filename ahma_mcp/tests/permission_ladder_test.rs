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
//!   * an approval is **persisted** (so the next start, or the next hooked
//!     command, actually works), and
//!   * the **live sandbox is never widened** (R5.1 lock-once) — the grant is a
//!     promise about the future, not a hole in the present.

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ahma_common::config::{AhmaSettings, ScopeAccess};
use ahma_common::scope_grant::{GrantCoordinator, GrantDecision, ScopeGrantRequest};
use ahma_mcp::sandbox::{
    ElicitOutcome, ElicitationSurface, PermissionBroker, Sandbox, SandboxMode, ScopeGrantNotifier,
    grant_channel::{notify_pre_exec, notify_stderr_denial},
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
async fn an_approval_at_the_harness_is_persisted_without_widening_the_live_sandbox() {
    let home = TempDir::new().unwrap();
    // SAFETY: single-test binary; set before any settings access.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let workspace = TempDir::new().unwrap();
    let sandbox = test_sandbox(workspace.path());
    let scopes_before: Vec<PathBuf> = sandbox.scopes().to_vec();

    let harness = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::GrantRw)]);
    let broker = Arc::new(PermissionBroker::new(
        Arc::new(GrantCoordinator::new()),
        None, // no TUI attached: the harness is the only surface above fail-closed
    ));
    broker.set_elicitation_surface(harness.clone());
    let notifier: Arc<dyn ScopeGrantNotifier> = broker.clone();

    // A real kernel denial, as it appears in a sandboxed command's stderr.
    let denied = "error: failed to create directory `/opt/ext/sccache/0`: Read-only file system";
    notify_stderr_denial(&sandbox, Some(&notifier), denied, "", "sccache").await;

    assert_eq!(
        harness.asks.load(Ordering::SeqCst),
        1,
        "the denial reached the ladder and the harness was asked"
    );

    // The approval is in the ledger, ready for the next start / the next hooked command.
    let settings = AhmaSettings::load_from_result(&ledger(home.path()))
        .expect("the ledger was written and parses");
    let granted = settings
        .sandbox
        .find_scope(Path::new("/opt/ext/sccache/0"))
        .expect("the approved scope is persisted");
    assert_eq!(granted.access, ScopeAccess::Rw);

    // …and the live sandbox is untouched. This is the invariant that makes the
    // whole flow safe to automate: approving a grant can never punch a hole in the
    // session that is running right now (R5.1 lock-once).
    assert_eq!(
        sandbox.scopes().to_vec(),
        scopes_before,
        "an approved grant must NEVER widen the live sandbox scope"
    );

    // And it is auditable.
    let audit = home.path().join(".ahma").join("permissions-audit.jsonl");
    let text = std::fs::read_to_string(&audit).expect("the grant is audited");
    assert!(
        text.contains("/opt/ext/sccache/0"),
        "audit names the subject: {text}"
    );
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
    notify_pre_exec(Some(&notifier), &err, "cargo_build").await;
    notify_pre_exec(Some(&notifier), &err, "cargo_build").await;
    notify_pre_exec(Some(&notifier), &err, "cargo_build").await;

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
