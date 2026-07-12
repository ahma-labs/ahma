//! The claims that let terminal hooks be enabled by default (SPEC R-PERM.6).
//!
//! Hooks were disabled globally for a long time, and the stated reason — that a
//! sandbox exception might be "classified incorrectly" — was never quite the real
//! one. The real problem was that when the sandbox blocked something the user
//! genuinely wanted, **there was nowhere to ask them**. A hooked command's denial
//! reached no surface at all, so the only two outcomes were "blocked, with no way
//! forward" and "let it through". Hooks looked like they were getting in the way
//! because, lacking a question, they were.
//!
//! Turning them back on rests on three claims. This file holds them:
//!
//!   1. **A grant applies on the very next command.** Each hooked command spawns a
//!      fresh process that re-reads the ledger, so there is no server to restart.
//!      This is the hooks path's genuine advantage over the MCP path (R5.1
//!      lock-once), and it is what makes a denial *recoverable in the moment*
//!      rather than "fix it and come back later".
//!   2. **The fail-closed message is actionable**, in the terminal the user is
//!      already looking at — never a bare `Operation not permitted` buried in a
//!      build log.
//!   3. **Readiness is per client**, and a client that is skipped is skipped *with
//!      a stated reason*. An unexplained default is how the old blanket "not ready"
//!      ended up looking arbitrary.

use std::path::Path;

use ahma_common::config::{AhmaSettings, ScopeAccess};
use ahma_mcp::sandbox::grant_channel::runtime_denial_remediation_cli;
use tempfile::TempDir;

/// Claim 1: a grant made now is in effect for the next command, with no restart.
///
/// The mechanism is that a hooked command re-reads `~/.ahma/settings.toml` on
/// every invocation. This test drives that mechanism directly: grant, then reload
/// the settings exactly as the next `ahma hooks run-shell` process would, and
/// assert the scope is there.
#[test]
fn a_grant_is_in_effect_for_the_next_hooked_command_with_no_restart() {
    let home = TempDir::new().unwrap();
    let settings_file = home.path().join(".ahma").join("settings.toml");
    let cache = home.path().join("ext-cache");
    std::fs::create_dir_all(&cache).unwrap();

    // Before: the next command would see no grant for this path, so the kernel
    // blocks it and the user is asked.
    let before = AhmaSettings::load_from(&settings_file);
    assert!(
        before.sandbox.find_scope(&cache).is_none(),
        "precondition: nothing granted yet"
    );

    // The user approves the grant (at whichever rung of the ladder answered).
    ahma_common::scope_grant::persist_grant(
        &settings_file,
        &cache,
        ScopeAccess::Rw,
        Some("hook".into()),
        Some("2026-07-12".into()),
        None,
    )
    .expect("an approved grant persists");

    // After: the *next* hooked command re-reads the ledger from scratch — this is
    // the same load the fresh `ahma hooks run-shell` process performs — and sees
    // the grant. No server restart, no "try again after reconnecting your IDE".
    let after = AhmaSettings::load_from_result(&settings_file).expect("the ledger parses");
    let granted = after
        .sandbox
        .find_scope(&cache)
        .expect("the next command sees the grant");
    assert_eq!(granted.access, ScopeAccess::Rw);
    assert_eq!(
        granted.granted_by.as_deref(),
        Some("hook"),
        "provenance records which surface answered"
    );
}

/// Claim 2: the message a denied hooked command leaves behind is one the user can
/// act on — it names the path, the command to run, and that it applies next time.
///
/// The failure this guards against is concrete and historical: a build-cache write
/// denied deep inside a dependency's build script, surfacing as nothing but
/// `Operation not permitted (os error 1)` a hundred lines into a build log.
#[test]
fn a_denied_hooked_command_leaves_an_actionable_message() {
    let denied = Path::new("/opt/ext/sccache/0/object.o");
    let msg = runtime_denial_remediation_cli(denied, ScopeAccess::Rw);

    assert!(
        msg.contains("ahma sandbox grant"),
        "it names the command that fixes it: {msg}"
    );
    assert!(
        msg.contains("/opt/ext/sccache/0"),
        "…for the *parent directory*, so one grant covers the whole cache rather \
         than re-prompting per file: {msg}"
    );
    assert!(
        msg.contains("re-run"),
        "…and says what to do afterwards: {msg}"
    );
    // The point of the whole exercise: it explains the block instead of just being one.
    assert!(
        msg.len() > 80,
        "an actionable message is not a one-word errno: {msg}"
    );
}

/// Claim 3: readiness is per client, and a skipped client says why.
#[test]
fn hook_readiness_is_per_client_and_every_omission_is_explained() {
    let readiness = ahma_mcp::hooks::hooks_readiness();
    assert!(!readiness.is_empty(), "clients are enumerated");

    let ready: Vec<&str> = readiness
        .iter()
        .filter(|(_, ready, _)| *ready)
        .map(|(label, _, _)| *label)
        .collect();
    assert!(
        !ready.is_empty(),
        "at least one client must be proven, or the ladder bought us nothing"
    );

    for (label, ready, reason) in &readiness {
        if *ready {
            assert!(reason.is_none(), "{label} is ready, so it needs no excuse");
        } else {
            // This is the part that matters. A client omitted *without* a reason is
            // indistinguishable from a client forgotten, and that is exactly how the
            // old blanket "hooks are not ready" came across.
            let reason = reason.expect("an unexplained omission is not acceptable");
            assert!(
                reason.contains("--hooks") || reason.contains("explicitly"),
                "{label}: the reason must tell the user how to opt in anyway: {reason}"
            );
        }
    }

    assert!(
        ahma_mcp::hooks::any_client_ready_for_hooks(),
        "the setup default is gated on this"
    );
}
