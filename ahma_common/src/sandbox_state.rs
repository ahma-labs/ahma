//! Unified sandbox state machine for coordinating sandbox lifecycle.
//!
//! This module provides a single source of truth for sandbox state that can be
//! observed by multiple components without polling. Uses `tokio::sync::watch`
//! for immediate notification on state changes (per R18: No-Wait State Transitions).
//!
//! # Example
//!
//! ```rust,ignore
//! use ahma_common::sandbox_state::{SandboxStateMachine, SandboxState};
//! use std::path::PathBuf;
//!
//! let sm = SandboxStateMachine::new();
//!
//! // Transition through states
//! sm.transition_to_configuring(vec![PathBuf::from("/home/user/project")]).unwrap();
//! sm.transition_to_active().unwrap();
//!
//! // Wait for state (no polling - uses watch channel)
//! let is_ready = sm.wait_for_active().await;
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

/// Sandbox lifecycle states - single source of truth per R20.
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxState {
    /// Waiting for client to provide workspace roots via roots/list
    AwaitingRoots,

    /// Sandbox configuration in progress (scopes received, being applied)
    Configuring { scopes: Vec<PathBuf> },

    /// Sandbox is fully configured and kernel-level enforcement is active
    Active { scopes: Vec<PathBuf> },

    /// Sandbox configuration failed
    Failed { error: String },

    /// Session terminated
    Terminated,
}

impl SandboxState {
    /// Returns true if sandbox is ready for tool execution
    pub fn is_active(&self) -> bool {
        matches!(self, SandboxState::Active { .. })
    }

    /// Returns true if sandbox is in a terminal state (failed or terminated)
    pub fn is_terminal(&self) -> bool {
        matches!(self, SandboxState::Failed { .. } | SandboxState::Terminated)
    }

    /// Returns the scopes if in Active state
    pub fn scopes(&self) -> Option<&[PathBuf]> {
        match self {
            SandboxState::Active { scopes } | SandboxState::Configuring { scopes } => Some(scopes),
            _ => None,
        }
    }
}

/// Observable sandbox state machine using watch channels for immediate notification.
///
/// This implements R18 (No-Wait State Transitions) and R20 (Single Source of Truth).
/// All state changes are immediately visible to all subscribers without polling.
#[derive(Clone)]
pub struct SandboxStateMachine {
    sender: Arc<watch::Sender<SandboxState>>,
    // Keep a receiver to ensure the channel stays alive
    _receiver: watch::Receiver<SandboxState>,
}

impl SandboxStateMachine {
    /// Create a new state machine in AwaitingRoots state
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(SandboxState::AwaitingRoots);
        Self {
            sender: Arc::new(sender),
            _receiver: receiver,
        }
    }

    /// Create a new state machine that starts in Active state (for non-deferred sandbox)
    pub fn new_active(scopes: Vec<PathBuf>) -> Self {
        let (sender, receiver) = watch::channel(SandboxState::Active { scopes });
        Self {
            sender: Arc::new(sender),
            _receiver: receiver,
        }
    }

    /// Get the current state without blocking
    pub fn current(&self) -> SandboxState {
        self.sender.borrow().clone()
    }

    /// Subscribe to state changes - returns a receiver that will be notified
    /// immediately when state changes (no polling required)
    pub fn subscribe(&self) -> watch::Receiver<SandboxState> {
        self.sender.subscribe()
    }

    /// Transition from AwaitingRoots to Configuring
    pub fn transition_to_configuring(&self, scopes: Vec<PathBuf>) -> Result<(), &'static str> {
        self.sender.send_if_modified(|state| {
            if matches!(state, SandboxState::AwaitingRoots) {
                *state = SandboxState::Configuring {
                    scopes: scopes.clone(),
                };
                true
            } else {
                false
            }
        });
        // Check if transition actually happened
        if matches!(&*self.sender.borrow(), SandboxState::Configuring { .. }) {
            Ok(())
        } else {
            Err("Can only transition to Configuring from AwaitingRoots")
        }
    }

    /// Transition from Configuring to Active
    pub fn transition_to_active(&self) -> Result<(), &'static str> {
        let mut transitioned = false;
        self.sender.send_if_modified(|state| {
            if let SandboxState::Configuring { scopes } = state {
                let s = std::mem::take(scopes);
                *state = SandboxState::Active { scopes: s };
                transitioned = true;
                true
            } else {
                false
            }
        });
        if transitioned {
            Ok(())
        } else {
            Err("Can only transition to Active from Configuring")
        }
    }

    /// Transition to Active from any non-terminal state.
    ///
    /// Unlike [`transition_to_active`](Self::transition_to_active), which only
    /// fires from `Configuring`, this advances to `Active` from either
    /// `AwaitingRoots` or `Configuring`. It exists because the *authoritative*
    /// signal that the sandbox is locked is the subprocess's
    /// `notifications/sandbox/configured` notification: when the HTTP bridge
    /// observes it, the subprocess has already applied and enforced its scopes,
    /// regardless of whether the bridge itself observed a `roots/list` lock
    /// first.
    ///
    /// Requiring `Configuring` first created a race: when SSE connects, the
    /// bridge fires `roots/list_changed` to the subprocess (which may emit
    /// `configured` immediately) on one task while `auto_lock_if_default_scope`
    /// transitions `AwaitingRoots -> Configuring` on another. If `configured`
    /// won that race, [`transition_to_active`](Self::transition_to_active) was a
    /// no-op and the session stuck forever — the TUI showed `[LOCKED]` (it saw
    /// the SSE-forwarded notification) while every `tools/call` returned HTTP
    /// 409 / JSON-RPC -32001.
    ///
    /// When transitioning from `Configuring`, the existing scopes are preserved
    /// if `scopes` is empty (the notification may omit them); otherwise the
    /// provided `scopes` win. Terminal states (`Failed`, `Terminated`) are
    /// preserved and yield an error. Already-`Active` is a no-op success
    /// (idempotent).
    pub fn transition_to_active_with_scopes(
        &self,
        scopes: Vec<PathBuf>,
    ) -> Result<(), &'static str> {
        self.sender.send_if_modified(|state| match state {
            SandboxState::AwaitingRoots => {
                *state = SandboxState::Active {
                    scopes: scopes.clone(),
                };
                true
            }
            SandboxState::Configuring {
                scopes: existing_scopes,
            } => {
                let resolved = if scopes.is_empty() {
                    std::mem::take(existing_scopes)
                } else {
                    scopes.clone()
                };
                *state = SandboxState::Active { scopes: resolved };
                true
            }
            // Already Active (idempotent) or terminal: do not modify.
            SandboxState::Active { .. }
            | SandboxState::Failed { .. }
            | SandboxState::Terminated => false,
        });
        if self.sender.borrow().is_active() {
            Ok(())
        } else {
            Err("Cannot transition to Active from a terminal state")
        }
    }

    /// Transition to Failed from any non-terminal state
    pub fn transition_to_failed(&self, error: String) -> Result<(), &'static str> {
        let mut transitioned = false;
        self.sender.send_if_modified(|state| {
            if !state.is_terminal() {
                *state = SandboxState::Failed {
                    error: error.clone(),
                };
                transitioned = true;
                true
            } else {
                false
            }
        });
        if transitioned {
            Ok(())
        } else {
            Err("Cannot transition from terminal state")
        }
    }

    /// Transition to Terminated from any non-terminal state
    pub fn transition_to_terminated(&self) -> Result<(), &'static str> {
        let mut transitioned = false;
        self.sender.send_if_modified(|state| {
            if !state.is_terminal() {
                *state = SandboxState::Terminated;
                transitioned = true;
                true
            } else {
                false
            }
        });
        if transitioned {
            Ok(())
        } else {
            Err("Cannot transition from terminal state")
        }
    }

    /// Wait until sandbox is Active - NO POLLING, uses watch channel
    /// Returns the scopes if successful, or error message if failed/terminated
    pub async fn wait_for_active(&self) -> Result<Vec<PathBuf>, String> {
        let mut rx = self.sender.subscribe();
        loop {
            {
                let state = rx.borrow();
                match &*state {
                    SandboxState::Active { scopes } => return Ok(scopes.clone()),
                    SandboxState::Failed { error } => return Err(error.clone()),
                    SandboxState::Terminated => return Err("Session terminated".to_string()),
                    _ => {}
                }
            }
            // Wait for next state change - this is NOT polling, it's event-driven
            if rx.changed().await.is_err() {
                return Err("State machine dropped".to_string());
            }
        }
    }

    /// Check if currently in Active state
    pub fn is_active(&self) -> bool {
        self.sender.borrow().is_active()
    }
}

impl Default for SandboxStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn test_scope() -> PathBuf {
        std::env::temp_dir().join("ahma_test_scope")
    }

    #[test]
    fn test_valid_transitions() {
        let scope = test_scope();
        let sm = SandboxStateMachine::new();
        assert!(matches!(sm.current(), SandboxState::AwaitingRoots));

        sm.transition_to_configuring(vec![scope.clone()]).unwrap();
        assert!(matches!(
            sm.current(),
            SandboxState::Configuring { scopes } if scopes == vec![scope.clone()]
        ));

        sm.transition_to_active().unwrap();
        assert!(matches!(
            sm.current(),
            SandboxState::Active { scopes } if scopes == vec![scope]
        ));
    }

    #[test]
    fn test_invalid_transition_awaiting_to_active() {
        let sm = SandboxStateMachine::new();
        assert!(sm.transition_to_active().is_err());
    }

    #[test]
    fn test_authoritative_active_from_awaiting_roots() {
        // The subprocess's notifications/sandbox/configured is authoritative:
        // the bridge must reach Active even if it never observed a roots lock
        // (it stayed in AwaitingRoots). This is the desync that made the TUI
        // show [LOCKED] while tools/call returned 409 forever.
        let scope = test_scope();
        let sm = SandboxStateMachine::new();
        assert!(matches!(sm.current(), SandboxState::AwaitingRoots));

        sm.transition_to_active_with_scopes(vec![scope.clone()])
            .unwrap();
        assert!(matches!(
            sm.current(),
            SandboxState::Active { scopes } if scopes == vec![scope]
        ));
    }

    #[test]
    fn test_authoritative_active_from_configuring_preserves_scopes() {
        // configured may carry no scope payload; when it arrives while the
        // bridge is mid-Configuring, the in-flight scopes must be preserved.
        let scope = test_scope();
        let sm = SandboxStateMachine::new();
        sm.transition_to_configuring(vec![scope.clone()]).unwrap();

        sm.transition_to_active_with_scopes(Vec::new()).unwrap();
        assert!(matches!(
            sm.current(),
            SandboxState::Active { scopes } if scopes == vec![scope]
        ));
    }

    #[test]
    fn test_authoritative_active_idempotent() {
        let scope = test_scope();
        let sm = SandboxStateMachine::new_active(vec![scope.clone()]);
        // A second configured notification (e.g. SSE replay) must not error.
        sm.transition_to_active_with_scopes(Vec::new()).unwrap();
        assert!(sm.is_active());
    }

    #[test]
    fn test_authoritative_active_rejected_from_terminal() {
        let sm = SandboxStateMachine::new();
        sm.transition_to_failed("boom".to_string()).unwrap();
        // A late configured must not resurrect a Failed session.
        assert!(
            sm.transition_to_active_with_scopes(vec![test_scope()])
                .is_err()
        );
        assert!(matches!(sm.current(), SandboxState::Failed { .. }));
    }

    #[test]
    fn test_authoritative_active_wins_race_against_late_configuring() {
        // Models the bridge race: configured (authoritative) reaches the state
        // machine BEFORE auto_lock's transition_to_configuring. Active must win
        // and the late Configuring attempt must be a no-op.
        let scope = test_scope();
        let sm = SandboxStateMachine::new();

        // configured arrives first while still AwaitingRoots.
        sm.transition_to_active_with_scopes(vec![scope.clone()])
            .unwrap();
        assert!(sm.is_active());

        // auto_lock's transition_to_configuring loses (only fires from
        // AwaitingRoots), so the session stays Active rather than getting stuck.
        assert!(sm.transition_to_configuring(vec![scope.clone()]).is_err());
        assert!(matches!(
            sm.current(),
            SandboxState::Active { scopes } if scopes == vec![scope]
        ));
    }

    #[test]
    fn test_failed_transition() {
        let sm = SandboxStateMachine::new();
        sm.transition_to_configuring(vec![test_scope()]).unwrap();
        sm.transition_to_failed("Test error".to_string()).unwrap();

        assert!(matches!(
            sm.current(),
            SandboxState::Failed { error } if error == "Test error"
        ));

        assert!(sm.transition_to_terminated().is_err());
    }

    #[tokio::test]
    async fn test_wait_for_active_immediate() {
        let scope = std::env::temp_dir().join("ahma_home_user");
        let sm = SandboxStateMachine::new_active(vec![scope.clone()]);
        let result = sm.wait_for_active().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![scope]);
    }

    #[tokio::test]
    async fn test_wait_for_active_delayed() {
        let scope = std::env::temp_dir().join("ahma_project");
        let expected = scope.clone();
        let sm = SandboxStateMachine::new();
        let sm_clone = sm.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sm_clone.transition_to_configuring(vec![scope]).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            sm_clone.transition_to_active().unwrap();
        });

        let result = timeout(Duration::from_secs(1), sm.wait_for_active()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), vec![expected]);
    }

    #[tokio::test]
    async fn test_wait_for_active_failed() {
        let sm = SandboxStateMachine::new();
        let sm_clone = sm.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sm_clone
                .transition_to_failed("Configuration error".to_string())
                .unwrap();
        });

        let result = timeout(Duration::from_secs(1), sm.wait_for_active()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn test_subscribe_receives_updates() {
        let sm = SandboxStateMachine::new();
        let mut rx = sm.subscribe();

        assert!(matches!(*rx.borrow(), SandboxState::AwaitingRoots));

        sm.transition_to_configuring(vec![test_scope()]).unwrap();

        assert!(rx.has_changed().unwrap());
        assert!(matches!(
            &*rx.borrow_and_update(),
            SandboxState::Configuring { .. }
        ));
    }
}
