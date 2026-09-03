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

The same change removed the transitive crates behind **RUSTSEC-2024-0370**
(`proc-macro-error`) and **RUSTSEC-2026-0215** (`smallstr`), whose `deny.toml`
ignores were deleted.

### Still open: RUSTSEC-2023-0071 (`rsa` Marvin Attack)

`sigstore`'s own `verify` feature enables `fulcio` → `oauth` → `openidconnect`,
which is the only reason `rsa` is in the graph. ahma signs nothing and never
performs an RSA private-key operation; `openidconnect` uses `rsa` for public-key
JWT signature verification only. The `deny.toml` ignore stays until `sigstore`
stops chaining those features, or `openidconnect` moves off `rsa`. Upstream to
watch: <https://github.com/sigstore/sigstore-rs>.
