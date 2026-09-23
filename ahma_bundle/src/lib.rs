//! # Bundle Supply-Chain Audit, and a Content Checksum
//!
//! A bundle is a directory of MTDF `*.json` tool definitions. Loading one means
//! letting somebody else define the commands ahma will run, so the interesting
//! question is what the tools *do* — which is what [`audit_bundle`] answers, by
//! scanning for embedded secrets, prompt-injection payloads in `description` /
//! `hints`, path arguments missing `format: "path"`, and exfiltration-shaped
//! command patterns.
//!
//! ## What is not here
//!
//! There is no signature, no key ring, no `--allow-unsigned` flag, and no
//! load-time gate: nothing in ahma refuses to load a bundle on trust grounds.
//! [`checksum::BundleChecksummer`] / [`BundleVerifier`] keep a SHA-256 content
//! manifest that detects **corruption** and cannot detect tampering, because the
//! manifest is unsigned and travels inside the bundle it describes. Tamper
//! evidence needs a detached signature verified against a key the bundle cannot
//! supply; that is not implemented (`ahma_bundle/SPEC.md`).

pub mod checksum;

pub use checksum::{
    BundleAuditResult, BundleAuditSeverity, BundleChecksummer, BundleVerifier, audit_bundle,
};
