//! # Bundle Signing and Supply-Chain Auditor
//!
//! Closes the "plugin marketplace contains malware" risk by inverting the
//! default: third-party MTDF tool bundles require explicit `--allow-unsigned`
//! to load.  First-party bundles shipped with ahma are always trusted.
//!
//! ## Signing model
//!
//! A bundle is a directory or `.tar.gz` archive of `.ahma/*.json` files.
//! The bundle manifest (`bundle.json`) records:
//!
//! - `name`, `version`, `author`
//! - SHA-256 digest of every included JSON file
//! - An ed25519 signature over the canonical manifest (JSON-deterministic)
//!
//! Verification checks the signature against the trusted key ring
//! (`~/.ahma/keys/trusted/`).
//!
//! ## Supply-chain audit
//!
//! `ahma bundle audit <path>` scans a bundle directory for:
//! - Known-suspicious MTDF patterns (e.g. `command: "curl"` + `subcommand` with
//!   `url` args → potential data exfiltration).
//! - Embedded secrets (API key patterns, AWS credential patterns).
//! - Prompt-injection payloads in `description` or `hints` fields.
//! - Missing `format: "path"` on path arguments (sandbox escape vector).

pub mod index;
pub mod signing;

pub use index::BundleIndex;
pub use signing::{BundleAuditResult, BundleAuditSeverity, BundleSigner, BundleVerifier};
