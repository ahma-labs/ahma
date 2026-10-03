# ahma_update Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_mcp::update` (`ahma update`, `ahma verify`), which re-exports it and adds
  the steps that need the MCP engine (stopping a stale bridge, re-checking client configs)

## 1. User Story / Problem Statement

*As an ahma user, I want `ahma update` to install only a binary that provably came from this
project's CI, and to never leave me with a half-installed or unrunnable binary.*

## 2. Acceptance Criteria

- `ahma update` with no argument installs the latest GitHub release; `ahma update <ref>`
  builds a branch, tag or commit with `cargo install` (`classify_ref` decides which).
- **Provenance** (`verify`): a release artifact installs only if one of its GitHub
  attestations passes every check — Fulcio certificate chain from the Sigstore trust root,
  embedded SCT, identity policy (GitHub Actions OIDC issuer, repository `ahma-labs/ahma`,
  signer URI under `https://github.com/ahma-labs/ahma/`), DSSE signature, in-toto subject
  matching the artifact's sha256, and a Rekor entry for this envelope logged while the
  certificate was valid. `ahma verify --self` runs the same checks on the running binary.
- `--insecure-skip-verify` skips provenance and is CLI-flag-only (R-CFG2.3).
- **Install** follows root SPEC R-SIGN.2: the new binary is written beside the target and
  renamed over it, never modified in place. On macOS it is then re-signed locally (R-SIGN.1),
  because an in-place rewrite of an ad-hoc-signed binary gets `SIGKILL (Code Signature
  Invalid)`.
- After install, `ahma setup` runs by re-executing the **new** binary, with `kill_on_drop`.
- `--prefer-musl` (Linux) selects the musl build; `--install-dir` sets the install directory.

## 3. Non-Functional Requirements

- No MCP-engine dependency: this crate can be tested without starting a server.
- Every GitHub request (release lookup, download, `SHA256SUMS`, attestations) goes through
  `github::get`: bounded connect and read timeouts, retries per root SPEC R-HTTP, and a final
  failure that leads with "Couldn't reach GitHub." or "GitHub … " before the technical detail.

## 4. Out of Scope

- Developer-ID signing and notarization of release binaries (root R-SIGN.1; open, root SPEC
  §11).
