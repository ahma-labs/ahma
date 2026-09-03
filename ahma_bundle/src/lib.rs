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
//! This header used to describe a signing model: an ed25519 signature over a
//! canonical manifest, verification against a trusted key ring in
//! `~/.ahma/keys/trusted/`, and third-party bundles requiring `--allow-unsigned`
//! to load. **None of that exists.** There is no signature, no key ring, no
//! `--allow-unsigned` flag, and no load-time gate: nothing in ahma refuses to
//! load a bundle on trust grounds. [`index::BundleIndex`] parses an index format
//! that no code path consults.
//!
//! What does exist is [`checksum::BundleChecksummer`] / [`BundleVerifier`], a
//! SHA-256 content manifest that detects **corruption** — and cannot detect
//! tampering, because the manifest is unsigned and travels inside the bundle it
//! describes. See that module's header for the full statement.
//!
//! The design is recorded in SPEC.md §11 as the v0.8 signed bundle index and is
//! worth building. Describing it in the present tense while it did not exist was
//! the actual hazard: a reader auditing ahma's supply-chain story found a
//! paragraph saying signatures were checked against a key ring, and no reason to
//! look further.

pub mod checksum;
pub mod index;

pub use checksum::{
    BundleAuditResult, BundleAuditSeverity, BundleChecksummer, BundleVerifier, audit_bundle,
};
pub use index::BundleIndex;
