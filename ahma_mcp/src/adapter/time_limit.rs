//! What a command is told about its time limit (SPEC R2.6.6).
//!
//! The limit (`[tools] timeout_secs`, 30 minutes by default) stops a command
//! that hangs. A healthy job close to it used to get no warning and, when
//! stopped, a message that named neither the setting nor how to change it.
//! Both messages now say where the limit lives: a repository's own
//! `.ahma/settings.toml` may raise it for that repository, and an MCP call
//! may pass `timeout_seconds` for itself.

/// The message for a command stopped at its limit.
pub(crate) fn limit_reached(limit_secs: u64) -> String {
    format!(
        "Operation timed out (exceeded timeout limit): {limit_secs} seconds. {}",
        how_to_raise(limit_secs)
    )
}

/// A synchronous command stopped at its time limit, with what it printed
/// first. `Display` is the MCP caller's message; a terminal hook renders its
/// own from the fields (SPEC R-SETUP.2.2), because the advice differs.
#[derive(Debug)]
pub struct LimitReached {
    /// The limit it was stopped at.
    pub limit_secs: u64,
    /// Its stdout then stderr up to the kill, possibly empty.
    pub output: String,
}

impl std::fmt::Display for LimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&limit_reached(self.limit_secs))?;
        if !self.output.trim().is_empty() {
            write!(f, "\n\nOutput before it was stopped:\n{}", self.output)?;
        }
        Ok(())
    }
}

impl std::error::Error for LimitReached {}

/// The alert sent once a running command has used 80% of its limit.
pub(crate) fn near_limit(limit_secs: u64) -> String {
    let left = limit_secs - limit_secs * 8 / 10;
    format!(
        "This command has used 80% of its {limit_secs}-second limit and will be stopped in \
         about {left} seconds if it is still running. {}",
        how_to_raise(limit_secs)
    )
}

/// The once-only near-limit warning (SPEC R2.6.6) of one running command,
/// shared by the piped, PTY and session loops so none of them can lose it.
pub(crate) struct NearLimit {
    at: tokio::time::Instant,
    limit_secs: u64,
    warned: bool,
}

impl NearLimit {
    /// Warn at 80% of `limit`, measured from now.
    pub(crate) fn starting_now(limit: std::time::Duration) -> Self {
        Self {
            at: tokio::time::Instant::now() + limit.mul_f64(0.8),
            limit_secs: limit.as_millis().div_ceil(1000) as u64,
            warned: false,
        }
    }

    /// When to warn.
    pub(crate) fn at(&self) -> tokio::time::Instant {
        self.at
    }

    /// Not warned yet: the `select!` arm's precondition.
    pub(crate) fn pending(&self) -> bool {
        !self.warned
    }

    /// The alert, and the warning is spent.
    pub(crate) fn alert(&mut self) -> String {
        self.warned = true;
        near_limit(self.limit_secs)
    }
}

/// Where the limit lives and how to raise it.
pub(crate) fn how_to_raise(limit_secs: u64) -> String {
    format!(
        "If the job is healthy, raise the limit for this repository with `[tools] timeout_secs = \
         {}` in its .ahma/settings.toml, or pass `timeout_seconds` for one MCP call; a job that \
         sits near the limit is often better split.",
        limit_secs * 2
    )
}
