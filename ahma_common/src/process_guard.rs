//! Spawn-depth backstop: a hard ceiling on how deeply `ahma` may spawn copies
//! of itself.
//!
//! `ahma serve` legitimately spawns a small, fixed chain of child processes:
//! the IDE-facing **frontend** spawns a detached **bridge**, which spawns one
//! **peer** per session (depth 0 → 1 → 2). A supervision bug that makes a child
//! mistake itself for a frontend turns that finite chain into an unbounded
//! frontend→bridge→peer→frontend→… recursion that exhausts the OS process table
//! (observed: thousands of idle `ahma serve` processes, `fork: Resource
//! temporarily unavailable`).
//!
//! This module is the circuit breaker. Each spawn site stamps the child's
//! environment with an incremented depth via [`child_spawn_depth`]; every
//! `ahma serve` invocation calls [`check_spawn_depth`] at startup and refuses to
//! run once the chain is implausibly deep. It is a backstop, not the primary
//! fix — the spawn sites must still set the correct child role — but it
//! guarantees that *any* such bug, present or future, self-limits instead of
//! taking down the machine.

/// Environment variable carrying a process's spawn depth.
///
/// Unset (or unparseable) means depth `0`: a root process launched by an
/// IDE/agent/CLI rather than by another `ahma`.
pub const SPAWN_DEPTH_ENV: &str = "AHMA_SPAWN_DEPTH";

/// Maximum tolerated spawn depth before a process refuses to start.
///
/// The deepest legitimate chain is frontend(0) → bridge(1) → peer(2). The
/// generous headroom (re-exec on version mismatch, HTTP-bridge variants, future
/// nesting) keeps normal operation well clear while still catching runaway
/// recursion, which historically grew dozens of levels deep.
pub const MAX_SPAWN_DEPTH: u32 = 8;

/// This process's spawn depth, read from [`SPAWN_DEPTH_ENV`] (default `0`).
pub fn current_spawn_depth() -> u32 {
    std::env::var(SPAWN_DEPTH_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// The value to stamp on a child's [`SPAWN_DEPTH_ENV`]: this process's depth + 1
/// (saturating). Spawn sites call this when building the child `Command`.
pub fn child_spawn_depth() -> String {
    current_spawn_depth().saturating_add(1).to_string()
}

/// `Err(message)` when this process is nested deeper than [`MAX_SPAWN_DEPTH`],
/// indicating a self-respawn loop. Callers should log the message and exit
/// non-zero so the chain stops growing instead of exhausting the process table.
pub fn check_spawn_depth() -> Result<(), String> {
    let depth = current_spawn_depth();
    if depth > MAX_SPAWN_DEPTH {
        Err(format!(
            "ahma spawn depth {depth} exceeds maximum {MAX_SPAWN_DEPTH}; refusing to start to \
             break a self-respawn loop ({SPAWN_DEPTH_ENV}={depth}). This indicates ahma is \
             spawning itself recursively. Run `pkill -f 'ahma serve'` and restart your IDE; if it \
             recurs, report it with this message."
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The depth helpers are pure functions of the environment. Because tests in
    /// a process share one environment, this test mutates and restores
    /// [`SPAWN_DEPTH_ENV`] within a single test to stay deterministic.
    #[test]
    fn depth_roundtrip_and_ceiling() {
        let saved = std::env::var(SPAWN_DEPTH_ENV).ok();

        // Unset → depth 0, child depth 1, well under the ceiling.
        unsafe { std::env::remove_var(SPAWN_DEPTH_ENV) };
        assert_eq!(current_spawn_depth(), 0);
        assert_eq!(child_spawn_depth(), "1");
        assert!(check_spawn_depth().is_ok());

        // A legitimate mid-chain depth is accepted.
        unsafe { std::env::set_var(SPAWN_DEPTH_ENV, "2") };
        assert_eq!(current_spawn_depth(), 2);
        assert_eq!(child_spawn_depth(), "3");
        assert!(check_spawn_depth().is_ok());

        // At the ceiling is still allowed; one past it is refused.
        unsafe { std::env::set_var(SPAWN_DEPTH_ENV, MAX_SPAWN_DEPTH.to_string()) };
        assert!(check_spawn_depth().is_ok());
        unsafe { std::env::set_var(SPAWN_DEPTH_ENV, (MAX_SPAWN_DEPTH + 1).to_string()) };
        assert!(check_spawn_depth().is_err());

        // Garbage parses as depth 0 (fail-open: never wedge a real process over a
        // malformed env var).
        unsafe { std::env::set_var(SPAWN_DEPTH_ENV, "not-a-number") };
        assert_eq!(current_spawn_depth(), 0);
        assert!(check_spawn_depth().is_ok());

        match saved {
            Some(v) => unsafe { std::env::set_var(SPAWN_DEPTH_ENV, v) },
            None => unsafe { std::env::remove_var(SPAWN_DEPTH_ENV) },
        }
    }
}
