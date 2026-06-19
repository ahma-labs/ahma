---
name: ahmadev
version: 0.1.0
author: Paul Houghton
description: >
   Repo-local development skill for the ahma workspace. NOT distributed.
   USE THIS SKILL for safe dependency updates (/ahmadev update),
   version bumping (/ahmadev bump), and help (/ahmadev help).
   Trigger phrases: "ahmadev", "ahmadev update", "ahmadev help",
   "safe dep update", "ahmadev bump", "bump version", "version bump",
   "update rust dependencies safely", "safe dependency upgrade",
   "cargo safe update", "update dependencies", "bump deps",
   "upgrade workspace deps".
user-invocable: true
scope: repo
---

<!-- REPO-LOCAL SKILL — not packaged or distributed with ahma releases.       -->
<!-- For general ahma usage (sandboxed tools, livelog, run_terminal_command,   -->
<!-- etc.) see: skills/ahma/SKILL.md or invoke /ahma help.                     -->

# ahmadev Skill — Development Task Guide

This skill covers development workflows inside the `ahma` workspace.
It is **not** part of the distributed ahma skill bundle.

---

## User-Invocable Subcommands

| Command | Purpose |
|---------|---------|
| `/ahmadev help` | List all available subcommands and their usage |
| `/ahmadev update` | Upgrade workspace deps that are ≥14 days old and advisory-clean |
| `/ahmadev bump <X.Y.Z>` | Bump ahma version in Cargo.toml |

---

## `/ahmadev help` — List Subcommands

When the user types `/ahmadev help`, respond with:

```
/ahmadev help      — Show this help list
/ahmadev bump      — Bump ahma version in Cargo.toml (workspace.package.version)
/ahmadev update    — Upgrade workspace dependencies (safe: ≥14d old, no known advisories)
```

Reference `/ahma help` for general ahma tooling (sandbox, livelog, run_terminal_command,
simplify, ahma update, etc.).

---

## `/ahmadev bump [X.Y.Z]` — Bump ahma Version

### What it does

Bumps the version of the `ahma` workspace. This updates the version in `Cargo.toml` (`[workspace.package].version`) and runs `cargo xtask bump-version <X.Y.Z>` to propagate the new version across all other version-bearing files (such as installation scripts, skill files, and locks), making updates and signing run smoothly.

### Why a bump is required to ship

**The version number is the release trigger.** Every push to `main` builds and attests release binaries, but the CI publish step (`job-publish-release` in `.github/workflows/build.yml`) creates a GitHub Release *only when the tag `v<version>` does not already exist*. If you push to `main` without bumping, the build runs but no new release is published — so `ahma update` and the install scripts keep serving the **previous** release artifact, and your merged changes never reach users.

Practical rule: **any push to `main` with user-facing changes needs a version bump in the same push.** Batching a session's merges and bumping once at the end is fine; just don't leave `main` with shipped changes under an already-released version.

### Default: bump the patch version

**When the user says "bump" or gives no explicit version, always increment the patch component** (`Z` in `X.Y.Z`), keeping the major and minor components unchanged.
Example: `0.11.13` → `0.11.14`, **not** `0.12.0`.

Only deviate from this rule when the user explicitly specifies a different version string.

### Usage examples

```bash
# Unqualified "bump" or "bump to next version": increment patch
/ahmadev bump          → reads current version, adds 1 to Z (e.g. 0.11.13 → 0.11.14)

# Explicit version override
/ahmadev bump 0.11.15
/ahmadev bump 1.0.0
```

### Workflow (how to invoke as an agent)

1. **Determine target version**:
   - If the user provided `X.Y.Z` explicitly, use it as-is.
   - Otherwise ("next version", no argument, etc.) read the current `[workspace.package].version` from `Cargo.toml`, and increment the patch component `Z` by 1.
2. **Validate format** (expect `X.Y.Z`, numeric semver core).
3. **Run the xtask command**: Do NOT manually edit `Cargo.toml` or any other files. Run the xtask bump command to update `Cargo.toml` and synchronize all version-bearing files in a single atomic step:
   ```bash
   cargo xtask bump-version <X.Y.Z>
   ```
   *Note: This command updates `Cargo.toml`, `skills/ahma/SKILL.md`, `scripts/install.sh`, and `scripts/install.ps1` automatically. Running this command first prevents build panics/errors caused by version mismatches between files.*
4. **Review git diff** to confirm version-bearing files are correctly modified:
   ```bash
   git diff
   ```

> **Why no quality pipeline?** `/ahmadev bump` intentionally skips `cargo fmt`, `cargo nextest run`, and `cargo clippy` because the xtask command only edits version-bearing strings and ensures they are internally consistent. Running the full test suite here would be a poor cost/benefit trade-off — do that in the natural course of testing your other work.

### Failure recovery

If the change is incorrect or compilation fails, revert files and retry:
```bash
git checkout -- Cargo.toml Cargo.lock scripts/install.sh scripts/install.ps1 skills/ahma/SKILL.md
```

### Notes

- Do not manually edit `Cargo.toml` for version bumping. Always use `cargo xtask bump-version <X.Y.Z>` as it ensures script signature consistency and smooth updates.
- This command coordinates the full release version synchronization across the repository.

---

## `/ahmadev update [options]` — Safe Dependency Upgrade

### What it does

Upgrades workspace Rust dependencies (Cargo.toml + Cargo.lock) using two safety filters:

1. **Age filter** — a candidate version must have been published on crates.io for at least
   **14 days** (configurable). This avoids the window where most upstream supply-chain
   attacks and accidental breaking changes are discovered.

2. **Advisory filter** — the candidate must not be flagged by `cargo deny check advisories`,
   which checks the [RustSec Advisory Database](https://rustsec.org/). The workspace
   already has `deny.toml` configured.

Pre-release versions (e.g. `1.0.0-alpha.1`) and yanked crates are always skipped.

### Usage examples

```
/ahmadev update                          # Run with defaults (≥14d, advisory-clean)
/ahmadev update --dry-run                # Preview plan, no file changes
/ahmadev update --min-age-days 21        # Stricter: require 21 days minimum age
/ahmadev update --include serde,tokio    # Only consider specific crates
/ahmadev update --exclude ring           # Skip specific crates
/ahmadev update --dry-run --min-age-days 0  # Show ALL available upgrades (no age gate)
```

### Prerequisites

```bash
cargo install cargo-edit    # provides `cargo upgrade`
cargo install cargo-deny    # provides `cargo deny`   (already used in CI)
```

Check current installs:
```bash
cargo upgrade --version
cargo deny --version
```

### Workflow (how to invoke as an agent)

**Recommended two-step flow:**

1. **Preview** — run a dry-run first to review the plan:

   ```
   run_terminal_command("cargo xtask safe-update --dry-run", working_directory=".")
   ```

2. **Apply** — if the plan looks good, apply:

   ```
   run_terminal_command("cargo xtask safe-update", working_directory=".")
   ```

3. **Verify** — run the full test suite per the project's [Definition of Done](../../AGENTS.md):

   ```
   run_terminal_command("cargo nextest run", working_directory=".")
   ```

4. **Review diff** before committing:

   ```
   run_terminal_command("git diff Cargo.toml Cargo.lock", working_directory=".")
   ```

> Use `run_terminal_command` (ahma MCP tool) rather than the native terminal so the
> command runs inside the kernel sandbox and returns an `operation_id` for async tracking.

### What `cargo xtask safe-update` does internally

```
cargo upgrade --dry-run           ← discover available bumps (cargo-edit)
crates.io API /crates/<n>/<v>     ← fetch publish date per candidate
cargo deny check advisories       ← collect RustSec advisory hits
```

Then for each passing candidate:
```
cargo upgrade -p <name>@<ver>     ← write new requirement to Cargo.toml
cargo update -p <name> --precise <ver>  ← pin version in Cargo.lock
```

Finally:
```
cargo check --workspace           ← verify workspace still compiles
```

### Summary table output

The command always prints a summary table:

```
crate                          old          new          age(d)  status
--------------------------------------------------------------------------------
serde                          1.0.210      1.0.215          22  upgrade
tokio                          1.40.0       1.41.0            3  skipped:too-new (3d)
ring                           0.17.8       0.17.9           30  skipped:advisory
```

Possible status values:

| Status | Meaning |
|--------|---------|
| `upgrade` | Applied (or would be applied in `--dry-run`) |
| `skipped:too-new (Nd)` | Release is < min-age-days old |
| `skipped:advisory` | RustSec advisory found for this version |
| `skipped:pre-release` | Version has a `-alpha`/`-beta` pre-release tag |
| `skipped:age-fetch-failed` | Could not reach crates.io API for this crate |

### Failure recovery

If `cargo check --workspace` fails after upgrades, revert with:

```bash
git checkout -- Cargo.toml Cargo.lock
```

Then re-run with `--exclude <problem-crate>` to skip the offending crate.

### Full flags reference

```
cargo xtask safe-update [options]

Options:
  --dry-run              Print the plan; do not modify any files
  --min-age-days <N>     Minimum days since crates.io publish (default: 14)
  --include <a,b,...>    Only consider these crates (comma-separated)
  --exclude <a,b,...>    Skip these crates (comma-separated)
  -h, --help             Show xtask help for this subcommand
```
