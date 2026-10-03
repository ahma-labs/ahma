# ahma_harness_tools Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: no workspace crate
* **Used by**: `ahma_mcp` (built-in file tools, `fetch_webpage`)

## 1. User Story / Problem Statement

*As the ahma MCP harness, I want scope-validated file I/O, search, and outbound web fetch, so that the built-in tools enforce the same confinement boundaries as the kernel sandbox without each tool reimplementing path validation — and so that outbound requests cannot reach hosts the filesystem sandbox says nothing about.*

## 2. Acceptance Criteria

- **Scope-Validated Filesystem API**: `read_file`, `write_file`, `replace_in_file`, `multi_edit`, `apply_patch`, `list_dir`, `file_search` and `grep_search` each accept a `scopes: &[PathBuf]` allowlist. Any path that canonicalizes outside every listed scope MUST be rejected with an error.
- **Symlink Resolution**: Validation canonicalizes before comparing, so a symlink pointing out of scope is rejected rather than followed.
- **Safe Edits** (root R26: R26.2, R26.3): an edit's `old_str` must match exactly once unless `replace_all` is set; `multi_edit` and `apply_patch` are all-or-nothing (every edit or hunk applies, or the file is untouched); every write is atomic (`atomic_write`: temp file + rename).
- **Search**: Glob-pattern file discovery and plain-text or regex line search, both confined to the same scope allowlist and `.gitignore`-aware.
- **Bounded Output** (root R26.4): `read_file` returns at most `DEFAULT_READ_LINES` (2000) lines of at most `MAX_LINE_CHARS` (2000) characters; `grep_search` at most 200 results by default (`max_results`); `file_search` at most `FILE_SEARCH_MAX` (1000) paths; `fetch_webpage` at most `MAX_FETCH_CHARS` (50 000) characters.
- **SSRF Egress Guard** (root R-WEB.3.2, R-WEB.3.4): Outbound HTTP made by ahma's own tools is blocked **at connection time on the resolved IP**, not on the hostname string, via a custom `reqwest` DNS resolver in `egress_guard`. This blocks cloud-metadata endpoints (`169.254.169.254`), loopback admin services, and RFC-1918 hosts.
- **DNS-Rebinding Resistance**: Because the resolver runs for the initial request *and every redirect hop*, a domain that flips its DNS to a private address after approval is still blocked when the socket is opened. IP literals bypass DNS and MUST be rejected up front by `check_url` and, for redirect targets, by the redirect policy.
- **Cross-Domain Redirect Guard** (root R-WEB.8): a fetch given a `RedirectDomainGuard` turns reqwest's automatic redirects off and follows each 3xx itself, at most `MAX_REDIRECTS` hops, re-running `check_url` (scheme, private IP literal) on every hop before connecting; the guarded resolver re-checks hostnames at connect time. Each hop is decided by `decide_redirect` against the *original* host: the same host (case-insensitive, any scheme or port) is always followed. Another host follows the caller's `[web] on_redirect_to_new_domain`, with the caller's live `[web]` verdict for the target URL (session grants and denies read at the time of the hop): `policy` (default) follows iff the verdict is `Allow`, otherwise fails naming `ahma web allow <host>`, and never prompts; `block` never follows and fails naming the target and the setting; `prompt` follows on `Allow`, refuses on `Deny`, and on an unknown host awaits the caller's approver (the same three-tier flow as a fresh request), following iff it answers yes. A host approved this way is not asked about again within the same chain. Every refusal is an `EgressBlocked`, so it is never retried as a network failure.
- **Bounded Redirects**: At most `MAX_REDIRECTS` (10) hops before the request fails.

## 3. Non-Functional Requirements

- **Defence in Depth**: Scope validation here duplicates the kernel sandbox deliberately — it is the in-process guard for callers that have not yet crossed a kernel boundary, not a replacement for it.
- **Configurable Strictness**: `block_private` is a parameter, not a constant, so a legitimate dev workflow or a test hitting a loopback mock can opt out (the `block_private_ranges = false` case, root R-WEB.3.3). Tool code MUST always use the strict (`true`) default.

## 4. Out of Scope

- Kernel-level sandbox enforcement (Landlock/Seatbelt/Job Objects) — see `ahma_mcp::sandbox` and root SPEC R6.
- Tool definition parsing and MCP dispatch (`ahma_mcp`).
