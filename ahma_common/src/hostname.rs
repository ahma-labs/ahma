//! Machine hostname resolution shared by every surface that names this host.

/// Best-effort name for this machine: `HOSTNAME` env var (Unix), then
/// `COMPUTERNAME` env var (Windows), then `"ahma-worker"`.
///
/// This fallback chain must stay single-sourced so that any surface naming this host
/// derives the machine's identity from it identically. Do not reimplement the chain at call sites.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "ahma-worker".to_owned())
}
