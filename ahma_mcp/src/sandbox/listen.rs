//! Where a sandboxed command may listen for connections (SPEC R-LISTEN).
//!
//! Listening on loopback reaches nothing outside the machine, so it is free.
//! Listening on every interface (`0.0.0.0`, `::`) or a LAN address lets any
//! device on the network connect to what the command serves, so it is a
//! capability the human grants: `[network] listen = "any"`.
//!
//! macOS enforces it in the Seatbelt profile: TCP binds are denied except on
//! `localhost`. UDP is left alone, since DNS and QUIC clients bind every
//! address just to send. Linux Landlock filters binds by port only and cannot
//! tell loopback from every interface, and Windows has no filter at all, so
//! there it is disclosed as not enforced instead (R-PERM.5.1).
//!
//! Process-global like the GPU toggle: set once at startup from the resolved
//! settings, read by every profile generation.

use std::sync::atomic::{AtomicBool, Ordering};

static LISTEN_ANY: AtomicBool = AtomicBool::new(false);

/// Install the resolved `[network] listen` value (`true` for `"any"`).
pub fn set_listen_any(any: bool) {
    LISTEN_ANY.store(any, Ordering::Relaxed);
}

/// Whether sandboxed commands may listen on every interface.
pub fn listen_any_enabled() -> bool {
    LISTEN_ANY.load(Ordering::Relaxed)
}

/// The Seatbelt rules for listening under the current setting. Placed after
/// `(allow network*)`: last match wins, so TCP binds are denied and then
/// re-allowed on loopback. Empty when every interface is allowed.
pub fn seatbelt_listen_rules() -> String {
    if listen_any_enabled() {
        return String::new();
    }
    "(deny network-bind (local tcp \"*:*\"))\n\
     (allow network-bind (local tcp \"localhost:*\"))\n"
        .to_string()
}

/// Whether the kernel on this platform enforces R-LISTEN.2.
pub fn listen_enforced_here() -> bool {
    cfg!(target_os = "macos")
}

/// Explain a bind the sandbox refused, if the output shows one, as the
/// capability it is — not a directory to grant (SPEC R-LISTEN.3).
pub fn listen_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    if listen_any_enabled() || !listen_enforced_here() {
        return None;
    }
    let hit = stderr
        .lines()
        .chain(stdout.lines())
        .any(looks_like_listen_denial);
    if !hit {
        return None;
    }
    Some(
        "The sandbox refused to let this command listen on every network interface (SPEC \
         R-LISTEN): a sandboxed command may listen on localhost (127.0.0.1 or ::1) only, because \
         listening on 0.0.0.0 or a LAN address lets any device on the network connect to it. \
         This is a capability, not a path — no `sandbox_grant` can help. Either bind to \
         127.0.0.1 (most dev servers take `--host 127.0.0.1`), or tell the human that \
         `ahma network listen any` (or `[network] listen = \"any\"` in ~/.ahma/settings.toml) \
         allows it for every workspace (next command for terminal hooks, next server start for \
         an MCP session)."
            .to_string(),
    )
}

/// A refused bind, as runtimes word it: Go `bind: operation not permitted`,
/// Node `listen EPERM: operation not permitted 0.0.0.0:3000`, Python
/// `PermissionError: [Errno 1] Operation not permitted` on `bind`, Rust
/// `Operation not permitted (os error 1)` from a bind or listen call.
fn looks_like_listen_denial(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let refused = lower.contains("operation not permitted") || lower.contains("eperm");
    let binding = lower.contains("bind") || lower.contains("listen");
    refused && binding
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_binds_are_loopback_only_unless_any() {
        set_listen_any(false);
        let rules = seatbelt_listen_rules();
        assert!(
            rules.contains("(deny network-bind (local tcp \"*:*\"))"),
            "{rules}"
        );
        assert!(rules.contains("(allow network-bind (local tcp \"localhost:*\"))"));
        let deny = rules.find("deny network-bind").unwrap();
        let allow = rules.find("allow network-bind").unwrap();
        assert!(
            deny < allow,
            "last match wins: the loopback allow must come last"
        );
        assert!(!rules.contains("udp"), "UDP is left alone");
        set_listen_any(true);
        assert_eq!(seatbelt_listen_rules(), "");
        set_listen_any(false);
    }

    #[test]
    fn a_refused_bind_is_named_as_the_capability() {
        set_listen_any(false);
        for line in [
            "listen tcp 0.0.0.0:8080: bind: operation not permitted",
            "Error: listen EPERM: operation not permitted 0.0.0.0:3000",
            "PermissionError: [Errno 1] Operation not permitted (while calling bind)",
            "Error: failed to bind 0.0.0.0:8000: Operation not permitted (os error 1)",
        ] {
            let note = listen_denial_note(line, "");
            assert_eq!(note.is_some(), listen_enforced_here(), "{line}");
            if let Some(note) = note {
                assert!(note.contains("ahma network listen any"), "{note}");
                assert!(note.contains("127.0.0.1"), "{note}");
                assert!(note.contains("not a path"), "{note}");
            }
        }
        for line in [
            "cat: x: Operation not permitted",
            "bind mount failed: no such device",
            "Listening on http://127.0.0.1:8000",
        ] {
            assert!(listen_denial_note(line, "").is_none(), "{line}");
        }
        set_listen_any(true);
        assert!(listen_denial_note("bind: operation not permitted", "").is_none());
        set_listen_any(false);
    }
}
