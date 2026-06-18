# Security Advisories — Accepted / Tracked Risks

This register records third-party advisories (e.g. Dependabot / RUSTSEC alerts) that
are **known and consciously accepted** because no clean upstream fix is available yet.
Each entry documents the analysis, why the current risk is tolerable, and the exact
condition that will let us close it. Dismissing an alert without a record here is not
allowed — the rationale must live in the repo, not only in the GitHub UI.

---

## `tough` < 0.22.0 — TUF delegation flaws (2 × high)

| | |
|---|---|
| **Alerts** | GHSA — "Missing Delegated Metadata Validation"; "Delegated Roles have a Signature Threshold Bypass" |
| **Crate** | `tough` (locked `0.21.0`) |
| **Fixed in** | `tough` `0.22.0` |
| **Status** | **Accepted (tolerable risk)** — no clean upstream fix available |
| **First flagged** | 2026-06-18 |

### Dependency path

```
ahma_mcp
  └─ sigstore-verification 0.2.8   (latest published)
       └─ sigstore 0.13.0          (req: sigstore ^0.13)
            └─ tough 0.21.0        (req: tough ^0.21)
```

### Why we cannot simply bump

`tough 0.22.0` is only pulled in by `sigstore 0.14.0` (`tough ^0.22`). Reaching
`sigstore 0.14` requires `sigstore-verification` to depend on `sigstore ^0.14`, but
**no published `sigstore-verification` does** — `0.2.8` (latest) still pins
`sigstore ^0.13`, which pins `tough ^0.21`. A `cargo update` / `--precise` bump is
therefore rejected by the resolver, and a `[patch.crates-io]` cannot satisfy the
`^0.21` requirement with a `0.22.x` crate.

The only way to force `tough 0.22` today is to vendor/fork `sigstore-verification`
onto `sigstore 0.14` and adapt to the 0.13→0.14 API changes — adding maintained
third-party code and an upgrade-risk surface. We judged that worse than the residual
risk below.

### Why the residual risk is tolerable

The vulnerable code (`tough`'s TUF delegated-metadata / signature-threshold handling)
is reachable **only** through the self-update attestation path:

- `ahma_mcp::update::verify::verify_artifact` → `sigstore_verification::verify_github_attestation`
- invoked solely by the `ahma update` and `ahma verify` subcommands.

It is **not** part of the MCP server, the sandbox, or any normal runtime path. To
exploit it an attacker would have to subvert the **Sigstore public-good TUF CDN**
(`tuf-repo-cdn.sigstore.dev`) — its delegated role metadata — at the moment a user
runs an attested update, in order to defeat provenance verification. That is a narrow,
high-capability attack against an occasional, user-initiated, opt-in operation.

Mitigations already in place: release artifacts are additionally checksum-verified
(`update::install::verify_file_checksum`), and `ahma update` only writes within the
sandbox.

### Closing condition

Close (and bump) as soon as **`sigstore-verification` publishes a release depending on
`sigstore ^0.14`** (which brings `tough ^0.22`). Action then:

```bash
cargo update -p sigstore-verification   # pull the sigstore-0.14 release
cargo tree -i tough                      # confirm tough >= 0.22.0
```

Then drop this entry. Upstream to watch: <https://crates.io/crates/sigstore-verification>.
