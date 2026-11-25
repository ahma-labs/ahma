# Security Advisories — Accepted / Tracked Risks

This register records third-party advisories (e.g. Dependabot / RUSTSEC alerts) that
are **known and consciously accepted** because no clean upstream fix is available yet.
Each entry documents the analysis, why the current risk is tolerable, and the exact
condition that will let us close it. Dismissing an alert without a record here is not
allowed — the rationale must live in the repo, not only in the GitHub UI.

---

## `tough` < 0.22.0 — TUF delegation flaws (2 × high) — **RESOLVED in v0.19.8**

| | |
|---|---|
| **Alerts** | GHSA — "Missing Delegated Metadata Validation"; "Delegated Roles have a Signature Threshold Bypass" |
| **Crate** | `tough` (was locked `0.21.0`, now `0.22.0`) |
| **Fixed in** | `tough` `0.22.0` |
| **Status** | **Resolved** — the blocking dependency was removed rather than waited on |
| **First flagged** | 2026-06-18 |
| **Closed** | 2026-09-03 |

### Why it was stuck, and how it was unstuck

`tough 0.22.0` is only reachable through `sigstore 0.14`, and the old path went
`ahma_update → sigstore-verification 0.2.8 → sigstore ^0.13 → tough ^0.21`. No
published `sigstore-verification` depends on `sigstore ^0.14`, so neither
`cargo update --precise` nor `[patch.crates-io]` could satisfy the `^0.21`
requirement with a `0.22.x` crate — the register's original closing condition
("wait for a `sigstore-verification` release on `sigstore ^0.14`") never came.

The wrapper was dropped instead: `ahma verify` / `ahma update` now implement GitHub
Build Provenance verification directly on `sigstore` 0.14 (see
[release-signing.md](release-signing.md) and `ahma_update/src/verify.rs`), which
brings `tough 0.22.0` with it. Verify with:

```bash
cargo tree -i tough      # tough v0.22.0 └── sigstore v0.14.0
```

The same change removed **RUSTSEC-2026-0215** (`smallstr`). **RUSTSEC-2024-0370**
returned later through a path that is never compiled; see its entry below.

---

## RUSTSEC-2023-0071 — `rsa` (Marvin Attack) — **Accepted**

`sigstore`'s own `verify` feature enables `fulcio` → `oauth` → `openidconnect`,
which is the only reason `rsa` is in the graph. ahma signs nothing and never
performs an RSA private-key operation; `openidconnect` uses `rsa` for public-key
JWT signature verification only. The `deny.toml` ignore stays until `sigstore`
stops chaining those features, or `openidconnect` moves off `rsa`. Upstream to
watch: <https://github.com/sigstore/sigstore-rs>.

---

## RUSTSEC-2026-0387 — `crypto-bigint` 0.5 timing variability — **Accepted**

| | |
|---|---|
| **Crate** | `crypto-bigint` 0.5.5, via `sigstore` → `openidconnect` → `elliptic-curve` 0.13 |
| **Fixed in** | `>= 0.7.6`, a breaking API change `elliptic-curve` 0.13 and `openidconnect` 4.0 have not adopted |
| **Why it is tolerable** | ahma's use of `sigstore` is public-key verification of release attestations only (`ahma verify`, `ahma update`), never a private-key operation with a secret |
| **Closes when** | `sigstore` stops chaining `fulcio` → `oauth`, or `openidconnect` upgrades |

## RUSTSEC-2024-0370 — `proc-macro-error` unmaintained — **Accepted**

| | |
|---|---|
| **Crate** | `proc-macro-error`, via `tao`'s Linux windowing stack (`gtk` 0.18 → `glib-macros` / `gtk3-macros`) |
| **Why it is tolerable** | "Unmaintained", not a vulnerability, in a build-time proc macro. `ahma_gui` uses `tao` only on macOS and Windows, where it uses no gtk; no Linux build links `tao`. The lockfile lists it because Cargo locks every platform |
| **Closes when** | Never reached on a shipped target. Re-check with `cargo tree -i proc-macro-error --target <triple>` |

## RUSTSEC-2026-0543 — `chrono` unmaintained — **Accepted**

| | |
|---|---|
| **Crate** | `chrono`, via `rmcp` (non-optional), `oauth2` 5.0 (`ahma_http_mcp_client`) and `sigstore` 0.14 (`ahma_update`) |
| **Why it is tolerable** | "Unmaintained", not a vulnerability; no flaw is reported in the code ahma runs, which formats and compares timestamps. ahma's own code uses `jiff` (R-TIME.1) and turns on no `chrono` feature |
| **Closes when** | `cargo tree -i chrono` shows nothing. A vulnerability advisory for `chrono` would fail the check by itself, since this entry covers only this id |
