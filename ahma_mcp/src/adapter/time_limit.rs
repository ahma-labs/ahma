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

/// The alert sent once a running command has used 80% of its limit.
pub(crate) fn near_limit(limit_secs: u64) -> String {
    let left = limit_secs - limit_secs * 8 / 10;
    format!(
        "This command has used 80% of its {limit_secs}-second limit and will be stopped in \
         about {left} seconds if it is still running. {}",
        how_to_raise(limit_secs)
    )
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
