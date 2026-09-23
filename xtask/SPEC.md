# xtask Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0 (`publish = false`)
* **Depends on**: no workspace crate
* **Used by**: developers and release automation (`cargo xtask …`)

## 1. User Story / Problem Statement

*As a maintainer cutting a release, I want one command that updates the version everywhere it is recorded, so that a release cannot ship with the Cargo version bumped but the install scripts or skill manifest left pointing at the previous release.*

## 2. Acceptance Criteria

- **`bump-version X.Y.Z`**: Updates the workspace version across every version-bearing file in one step — `Cargo.toml`, `Cargo.lock`, `skills/ahma/SKILL.md`, `scripts/install.sh`, and `scripts/install.ps1`.
- **`Cargo.lock` Is Included**: Every workspace member records its version in the lockfile and all CI builds use `--locked`, so a bump that skips the lockfile produces a commit that fails to build under the gate.
- **`bump-android-version`**: Increments the Android Play `versionCode` and syncs `versionName` from the Cargo version.
- **`clean-stale`**: prunes stale `target/` state — incremental sessions older than the cutoff, coverage counters, `target/tmp/`; `--aggressive` also prunes `deps/`, `build/`, `.fingerprint/`; `--max-size-gb N` evicts oldest-first until `target/` fits. Reports `target/`'s size before and after (AGENTS.md, Target Directory).
- **`safe-update`**: Upgrades workspace dependencies that are at least 14 days old and carry no known advisories, so a freshly published (and potentially compromised or broken) release is not adopted immediately.
- **Version Is The Release Trigger** (enforced by CI, not by this crate): a push to `main` publishes GitHub Release `v<X.Y.Z>` only when it bumps to a version whose tag does not yet exist. That is why the bump must be complete in one commit.

## 3. Non-Functional Requirements

- **Idempotence**: Re-running a bump at the current version is not an error: it repairs any version-bearing file still at an older version, `Cargo.lock` included, and otherwise changes nothing.
- **Atomicity Of Intent**: All version-bearing files are updated together in a single invocation; partial updates are the failure mode this crate exists to prevent.
- **Dev-Only**: Not published to crates.io and not in workspace `default-members`.

## 4. Out of Scope

- Building, signing, attesting, or uploading release artifacts — those are CI workflow responsibilities gated on the full cross-platform matrix.
- Creating git tags. The tag is created by the publish job, never locally; a released version is never re-tagged or rewritten.
