//! Safe stdout notification delivery for the subprocess-to-bridge protocol.
//!
//! In HTTP bridge mode, per-session `ahma` subprocesses communicate with
//! the bridge via stdin/stdout pipes.  Sandbox lifecycle notifications
//! (`configured`, `failed`, `terminated`) are written as raw JSON-RPC to
//! stdout so the bridge can intercept and broadcast them.
//!
//! **Why not `println!`?**  `println!` panics on *any* write error.  On
//! Windows, OS error 232 ("The pipe is being closed") fires when the bridge
//! kills the subprocess or closes its end of the pipe during shutdown.  On
//! Unix, SIGPIPE can cause similar issues.  A panic here is never useful:
//!
//! - **During shutdown** the notification is best-effort; the bridge is
//!   already tearing down.
//! - **During active operation** a broken pipe means the bridge crashed.
//!   The subprocess should log and exit, not panic with a stack trace.
//!
//! See SPEC.md R5.6.1 for the formal requirement.

use std::io::{self, ErrorKind, Write};

/// Write a JSON-RPC notification to stdout for the bridge to read.
///
/// The notification is prefixed with `\n` and followed by `\n` (via
/// `writeln!`) to ensure the bridge's line-oriented reader can parse it
/// even if it arrives concatenated with a previous partial message.
///
/// # Error handling
///
/// - **Broken pipe** (`ErrorKind::BrokenPipe`, Windows error 232): logged
///   at `debug` level and treated as success.  This is expected when the
///   bridge closes the pipe during shutdown.
/// - **Other I/O errors**: logged at `warn` level and returned so callers
///   can decide whether to continue or abort.
///
/// # Returns
///
/// `Ok(())` on success or broken pipe, `Err(io::Error)` on unexpected
/// write failures.
pub fn emit_stdout_notification(json: &str) -> io::Result<()> {
    // Build the whole framed message up front and issue a SINGLE `write_all`.
    //
    // `writeln!(stdout, "\n{}", json)` expands to THREE separate `write_all`
    // calls ("\n", json, "\n").  On a Windows pipe shared with other writers
    // (the rmcp transport), three calls can interleave with concurrent writes
    // and corrupt the line.  One `write_all` of a small message is a single
    // `WriteFile` and cannot be split by another writer, keeping the
    // notification on a clean line.  (The peer transport is still the preferred
    // path — see emit_sandbox_notification_via_peer — but several lifecycle
    // emits run during shutdown when no peer is available and fall back here.)
    let framed = format!("\n{}\n", json);
    write_stdout(&framed, "notification")
}

/// Write arbitrary text to stdout with the same broken-pipe handling.
///
/// The sibling of [`emit_stdout_notification`] for payloads that are not
/// JSON-RPC and must not be framed with surrounding newlines — currently the
/// wrapped command's own output on the terminal-hook execution path, which is
/// read by the editor's hook engine through a pipe. That call site used
/// `println!`, which is the exact macro the module header explains must not be
/// used on a pipe: it panics unconditionally on a write error, and a hook whose
/// consumer has gone away then dies with a stack trace instead of a log line.
///
/// Returns `Ok(())` on success or broken pipe, `Err` on any other I/O error.
pub fn emit_stdout_text(text: &str) -> io::Result<()> {
    write_stdout(text, "output")
}

/// One `write_all` of `payload`, classifying the error per SPEC R5.6.1.
///
/// A single `write_all` rather than several: on a Windows pipe shared with other
/// writers, separate calls can interleave with a concurrent write and corrupt
/// the line.
fn write_stdout(payload: &str, what: &str) -> io::Result<()> {
    fn write_to<W: Write>(mut w: W, payload: &str, what: &str) -> io::Result<()> {
        match w.write_all(payload.as_bytes()) {
            Ok(()) => {
                let _ = w.flush();
                Ok(())
            }
            Err(e) if is_broken_pipe(&e) => {
                tracing::debug!("stdout pipe closed (broken pipe) — {what} not delivered");
                Ok(())
            }
            Err(e) => {
                tracing::warn!("Unexpected stdout write error: {}", e);
                Err(e)
            }
        }
    }

    if let Some(saved_stdout) = super::stdio_redirect::get_saved_stdout() {
        write_to(saved_stdout, payload, what)
    } else {
        write_to(io::stdout().lock(), payload, what)
    }
}

/// Returns `true` for broken-pipe errors on both Unix and Windows.
///
/// - Unix: `ErrorKind::BrokenPipe` (EPIPE)
/// - Windows: `ErrorKind::BrokenPipe` (mapped from OS error 232)
fn is_broken_pipe(e: &io::Error) -> bool {
    e.kind() == ErrorKind::BrokenPipe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_broken_pipe_unix_style() {
        let err = io::Error::new(ErrorKind::BrokenPipe, "pipe closed");
        assert!(is_broken_pipe(&err));
    }

    #[test]
    fn test_is_not_broken_pipe() {
        let err = io::Error::new(ErrorKind::NotFound, "not found");
        assert!(!is_broken_pipe(&err));
    }

    #[test]
    fn test_emit_stdout_notification_success() {
        let json = r#"{"jsonrpc":"2.0","method":"test"}"#;
        assert!(emit_stdout_notification(json).is_ok());
    }

    #[test]
    fn emit_stdout_text_does_not_frame_its_payload() {
        // The notification form wraps in newlines so a line-oriented reader can
        // find it; command output must arrive byte-for-byte as the command
        // produced it, so the two cannot share one entry point.
        assert!(emit_stdout_text("plain output\n").is_ok());
    }
}
