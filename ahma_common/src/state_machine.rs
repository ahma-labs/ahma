//! Workspace state-machine convention.
//!
//! Ahma models every non-trivial lifecycle as an **explicit, hand-written state
//! machine** rather than reaching for a third-party FSM crate. See the "State
//! Machines" section of `SPEC.md` for the rationale; in short, our states carry
//! data (`Active { scopes }`, `Completed { output }`), must be shared across
//! async tasks behind an `Arc`, and must be observable without polling
//! (R18/R20) — requirements that the popular FSM crates (`rust-fsm`'s unit-only
//! states, `statig`'s event-dispatch framework, compile-time `typestate`) do
//! not satisfy without fighting the architecture.
//!
//! This module supplies the three shared building blocks every machine reuses:
//!
//! - [`FsmState`] — a marker trait giving each state enum a uniform vocabulary
//!   (`name()` for logs/metrics, `is_terminal()` for guards).
//! - [`InvalidTransition`] — the typed error a guarded transition returns when
//!   rejected.
//! - [`Observable`] — a `tokio::sync::watch`-backed container for a state that
//!   any number of tasks read or `await` without polling. It is the generalized
//!   form of [`crate::sandbox_state::SandboxStateMachine`].
//!
//! For purely local state that is *not* observed across tasks, use the simpler
//! [`StateMachine`] (a `Mutex` plus a transition closure).

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use tokio::sync::watch;

/// A value used as the state of a finite state machine.
///
/// Implement this for every enum that models an explicit lifecycle so that
/// logging, metrics, and tests share one vocabulary across the workspace. The
/// trait is deliberately tiny: it describes a state, it does not drive
/// transitions. Transitions live in named, guarded methods on the owning
/// machine (see [`Observable`] and `SandboxStateMachine`), because only those
/// methods know the legal predecessor states and the data each carries.
pub trait FsmState: Clone + std::fmt::Debug {
    /// A stable, human-readable name for the current variant.
    ///
    /// Used in logs, metrics, and [`InvalidTransition`] messages. Keep it equal
    /// to the variant identifier (e.g. `"Configuring"`).
    fn name(&self) -> &'static str;

    /// True when the machine has reached a terminal state and no further
    /// transition is permitted (e.g. `Failed`, `Terminated`, `Done`).
    fn is_terminal(&self) -> bool {
        false
    }
}

/// Error returned by a guarded transition that was rejected because the machine
/// was not in a legal predecessor state.
///
/// Carrying both the originating state and the attempted action makes rejected
/// transitions self-describing in logs and test assertions, replacing the
/// ad-hoc `&'static str` errors the earliest machines used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTransition {
    /// [`FsmState::name`] of the state the machine was in when the transition
    /// was attempted.
    pub from: &'static str,
    /// Name of the transition that was attempted (e.g. `"to_active"`).
    pub action: &'static str,
}

impl InvalidTransition {
    /// Construct an error describing a rejected `action` attempted from `from`.
    pub fn new(from: impl FsmState, action: &'static str) -> Self {
        Self {
            from: from.name(),
            action,
        }
    }
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid state transition: cannot `{}` from `{}`",
            self.action, self.from
        )
    }
}

impl std::error::Error for InvalidTransition {}

/// An observable state machine: a single source of truth for a state value that
/// any number of tasks can read or `await` without polling.
///
/// This is the generalized engine behind
/// [`crate::sandbox_state::SandboxStateMachine`]. It is built on
/// `tokio::sync::watch`, satisfying R18 (No-Wait State Transitions) and R20
/// (Single Source of Truth): every change is delivered to all subscribers
/// immediately, and there is exactly one authoritative copy of the state.
///
/// Concrete machines wrap an `Observable<MyState>` privately and expose named,
/// guarded transition methods (`to_active`, `to_failed`, …) implemented with
/// [`modify`](Self::modify). Callers never mutate the state directly.
///
/// # Example
///
/// ```rust
/// use ahma_common::state_machine::{FsmState, InvalidTransition, Observable};
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum Light { Red, Green }
///
/// impl FsmState for Light {
///     fn name(&self) -> &'static str {
///         match self { Light::Red => "Red", Light::Green => "Green" }
///     }
/// }
///
/// struct Signal(Observable<Light>);
/// impl Signal {
///     fn go(&self) -> Result<(), InvalidTransition> {
///         self.0.modify(|s| match s {
///             Light::Red => { *s = Light::Green; (true, Ok(())) }
///             other => (false, Err(InvalidTransition::new(other.clone(), "go"))),
///         })
///     }
/// }
/// ```
#[derive(Clone)]
pub struct Observable<S> {
    sender: Arc<watch::Sender<S>>,
    // Hold a receiver so the channel never closes while the machine is alive,
    // even if every external subscriber has been dropped.
    _keepalive: watch::Receiver<S>,
}

impl<S: std::fmt::Debug> std::fmt::Debug for Observable<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observable")
            .field("state", &*self.sender.borrow())
            .finish()
    }
}

impl<S: Clone + Send + Sync + 'static> Observable<S> {
    /// Create a machine in `initial`.
    pub fn new(initial: S) -> Self {
        let (sender, receiver) = watch::channel(initial);
        Self {
            sender: Arc::new(sender),
            _keepalive: receiver,
        }
    }

    /// Read the current state without blocking.
    pub fn current(&self) -> S {
        self.sender.borrow().clone()
    }

    /// Inspect the current state under a borrow, without cloning it.
    ///
    /// Prefer this over [`current`](Self::current) in hot paths (e.g. request
    /// gating) where only a small projection of the state is needed. Keep `f`
    /// short: it runs while the channel's read lock is held.
    pub fn read<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.sender.borrow())
    }

    /// Subscribe to state changes. The returned receiver is notified the moment
    /// the state changes — no polling required.
    pub fn subscribe(&self) -> watch::Receiver<S> {
        self.sender.subscribe()
    }

    /// Apply a guarded transition.
    ///
    /// `f` inspects (and may mutate) the state in place and returns
    /// `(changed, output)`. Subscribers are notified **iff** `changed` is
    /// `true`, so a rejected transition that leaves the state untouched is also
    /// silent. The `output` (typically `Result<(), InvalidTransition>` or an
    /// action enum to run outside the lock) is returned to the caller.
    ///
    /// The closure runs under the channel's internal lock; keep it short and
    /// non-blocking.
    pub fn modify<R>(&self, f: impl FnOnce(&mut S) -> (bool, R)) -> R {
        let mut output = None;
        self.sender.send_if_modified(|state| {
            let (changed, r) = f(state);
            output = Some(r);
            changed
        });
        // `send_if_modified` always invokes the closure exactly once.
        output.expect("transition closure always runs")
    }

    /// Await until `pred` returns `Some`, then yield that value.
    ///
    /// This is event-driven, not polling: it parks on the watch channel between
    /// changes. Returns `Err` if the machine was dropped before `pred` matched.
    pub async fn wait_until<T>(
        &self,
        mut pred: impl FnMut(&S) -> Option<T>,
    ) -> Result<T, MachineDropped> {
        let mut rx = self.sender.subscribe();
        loop {
            // Scope the borrow so it is released before the await point.
            if let Some(t) = pred(&rx.borrow()) {
                return Ok(t);
            }
            if rx.changed().await.is_err() {
                return Err(MachineDropped);
            }
        }
    }
}

/// Returned by [`Observable::wait_until`] when the machine is dropped before the
/// awaited predicate is satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineDropped;

impl std::fmt::Display for MachineDropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("state machine was dropped before the awaited state was reached")
    }
}

impl std::error::Error for MachineDropped {}

/// A generic state machine wrapper ensuring thread-safe state transitions.
///
/// This struct wraps a state `S` in a `Mutex` and provides a `transition` method
/// to perform atomic state updates and return an action/result. Use it for
/// **local** state that is not observed across tasks; when other tasks must
/// react to changes without polling, use [`Observable`] instead.
///
/// # Example
///
/// ```rust
/// use ahma_common::state_machine::StateMachine;
///
/// enum State {
///     Idle,
///     Running,
/// }
///
/// let machine = StateMachine::new(State::Idle);
///
/// let action = machine.transition(|state| {
///     match state {
///         State::Idle => {
///             *state = State::Running;
///             "Started"
///         }
///         State::Running => "Already running",
///     }
/// });
/// ```
#[derive(Debug)]
pub struct StateMachine<S> {
    state: Mutex<S>,
}

impl<S> StateMachine<S> {
    /// Creates a new `StateMachine` in the given initial state.
    pub fn new(initial_state: S) -> Self {
        Self {
            state: Mutex::new(initial_state),
        }
    }

    /// Access the underlying state directly via a MutexGuard.
    ///
    /// Use this for simple reads or checks that don't require complex transitions.
    /// For transitions, prefer `transition`.
    pub fn lock(&self) -> MutexGuard<'_, S> {
        self.state.lock()
    }

    /// Perform an atomic transition on the state.
    ///
    /// The closure `f` is called with a mutable reference to the current state.
    /// The lock is held for the duration of the closure.
    ///
    /// Returns the result of the closure.
    pub fn transition<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.state.lock();
        f(&mut *state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_concurrent_transitions() {
        let machine = Arc::new(StateMachine::new(0));
        let mut handles = vec![];

        for _ in 0..10 {
            let machine = machine.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    machine.transition(|state| {
                        *state += 1;
                    });
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(*machine.lock(), 1000);
    }

    #[test]
    fn test_complex_transition_logic() {
        enum State {
            A,
            B,
        }

        let machine = StateMachine::new(State::A);

        let result = machine.transition(|state| match state {
            State::A => {
                *state = State::B;
                "moved to B"
            }
            _ => "error",
        });

        assert_eq!(result, "moved to B");
        match *machine.lock() {
            State::B => (),
            _ => panic!("Wrong state"),
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Light {
        Red,
        Green,
        Broken,
    }

    impl FsmState for Light {
        fn name(&self) -> &'static str {
            match self {
                Light::Red => "Red",
                Light::Green => "Green",
                Light::Broken => "Broken",
            }
        }
        fn is_terminal(&self) -> bool {
            matches!(self, Light::Broken)
        }
    }

    fn go(obs: &Observable<Light>) -> Result<(), InvalidTransition> {
        obs.modify(|s| match s {
            Light::Red => {
                *s = Light::Green;
                (true, Ok(()))
            }
            other => (false, Err(InvalidTransition::new(other.clone(), "go"))),
        })
    }

    #[test]
    fn observable_guarded_transition_accepts_and_rejects() {
        let obs = Observable::new(Light::Red);
        assert_eq!(obs.current(), Light::Red);

        assert!(go(&obs).is_ok());
        assert_eq!(obs.current(), Light::Green);

        // Rejected: not in a legal predecessor state; state is left untouched.
        let err = go(&obs).unwrap_err();
        assert_eq!(err.from, "Green");
        assert_eq!(err.action, "go");
        assert_eq!(obs.current(), Light::Green);
    }

    #[test]
    fn observable_only_notifies_on_actual_change() {
        let obs = Observable::new(Light::Green);
        let rx = obs.subscribe();
        // A rejected transition must not mark the channel changed.
        let _ = go(&obs);
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn fsm_state_terminal_and_name() {
        assert!(!Light::Red.is_terminal());
        assert!(Light::Broken.is_terminal());
        assert_eq!(Light::Green.name(), "Green");
    }

    #[test]
    fn invalid_transition_display() {
        let e = InvalidTransition::new(Light::Red, "go");
        assert_eq!(
            e.to_string(),
            "invalid state transition: cannot `go` from `Red`"
        );
    }

    #[tokio::test]
    async fn observable_wait_until_is_event_driven() {
        let obs = Observable::new(Light::Red);
        let obs2 = obs.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = go(&obs2);
        });
        let reached = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            obs.wait_until(|s| matches!(s, Light::Green).then_some(())),
        )
        .await;
        assert!(reached.is_ok(), "should observe Green without polling");
    }

    #[tokio::test]
    async fn observable_wait_until_returns_immediately_if_already_satisfied() {
        let obs = Observable::new(Light::Green);
        let r = obs
            .wait_until(|s| matches!(s, Light::Green).then_some(()))
            .await;
        assert!(r.is_ok());
    }
}
