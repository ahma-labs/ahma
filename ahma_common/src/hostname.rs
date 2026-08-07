//! Machine hostname resolution shared by every surface that names this host.

/// Best-effort name for this machine: `HOSTNAME` env var (Unix), then
/// `COMPUTERNAME` env var (Windows), then `"ahma-worker"`.
///
/// This fallback chain must stay single-sourced: the mDNS instance name
/// (`ahma_cluster::discovery`), the TLS certificate SAN (`ahma_cluster::tls`),
/// and the CLI's default worker id (`ahma cluster announce`) all derive the
/// machine's identity from it, and they must agree — a peer that advertises
/// one name over mDNS but presents a certificate for another fails mTLS
/// verification. Do not reimplement the chain at call sites.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "ahma-worker".to_owned())
}
