//! Build script for ahma_common.
//!
//! Embeds a compile-time build identifier (`AHMA_BUILD_ID`) so that same-semver
//! dev rebuilds are detectable at runtime.  The version check in
//! `ahma_mcp::shell::modes::server` compares both semver **and** build-id;
//! when they differ, the stdio process treats the running bridge as stale and
//! triggers a restart rather than proxying into an incompatible peer.

fn main() {
    let build_id = build_id();
    println!("cargo:rustc-env=AHMA_BUILD_ID={build_id}");

    // Re-run when git HEAD or refs change (covers branch switches and new commits).
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
}

fn build_id() -> String {
    // Prefer the git short hash: stable across parallel builds of the same commit
    // and human-readable in log output.
    if let Some(output) = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    {
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !hash.is_empty() {
            return hash;
        }
    }

    // Fallback: build epoch-seconds.  This always changes on rebuild, which is
    // exactly what we need: two binaries built at different times will differ.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("t{secs}")
}
