# ahma_harness_tools Crate Specification

* **Status**: Approved
* **Date**: 2026-07-27

## 1. User Story / Problem Statement

*As the ahma MCP harness, I want scope-validated file I/O, search, and outbound web fetch, so that the built-in tools enforce the same confinement boundaries as the kernel sandbox without each tool reimplementing path validation — and so that outbound requests cannot reach hosts the filesystem sandbox says nothing about.*

## 2. Acceptance Criteria

- **Scope-Validated Filesystem API**: `read_file`, `write_file`, `replace_in_file`, `list_dir`, `file_search` and `grep_search` each accept a `scopes: &[PathBuf]` allowlist. Any path that canonicalizes outside every listed scope MUST be rejected with an error.
- **Symlink Resolution**: Validation canonicalizes before comparing, so a symlink pointing out of scope is rejected rather than followed.
- **Search**: Glob-pattern file discovery and plain-text or regex line search, both confined to the same scope allowlist.
- **SSRF Egress Guard** (SPEC R-WEB.3.3): Outbound HTTP made by ahma's own tools is blocked **at connection time on the resolved IP**, not on the hostname string, via a custom `reqwest` DNS resolver. This blocks cloud-metadata endpoints (`169.254.169.254`), loopback admin services, and RFC-1918 hosts.
- **DNS-Rebinding Resistance**: Because the resolver runs for the initial request *and every redirect hop*, a domain that flips its DNS to a private address after approval is still blocked when the socket is opened. IP literals bypass DNS and MUST be rejected up front by `check_url` and, for redirect targets, by the redirect policy.
- **Cross-Domain Redirect Guard** (SPEC R-WEB.8): A redirect to a *different public domain* is followed only if the caller's live `[web]` policy would independently approve that host. The origin host is always permitted. Without this, an approved host returning `302 Location: https://evil.example/` would launder an unapproved domain through an approved one.
- **Bounded Redirects**: At most `MAX_REDIRECTS` (10) hops before the request fails.

## 3. Non-Functional Requirements

- **Defence in Depth**: Scope validation here duplicates the kernel sandbox deliberately — it is the in-process guard for callers that have not yet crossed a kernel boundary, not a replacement for it.
- **Configurable Strictness**: `block_private` is a parameter, not a constant, so a legitimate dev workflow or a test hitting a loopback mock can opt out. Tool code MUST always use the strict (`true`) default.

## 4. Out of Scope

- Kernel-level sandbox enforcement (Landlock/Seatbelt/Job Objects) — see `ahma_mcp::sandbox` and root SPEC R6.
- Tool definition parsing and MCP dispatch (`ahma_mcp`).
