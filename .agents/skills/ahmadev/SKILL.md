---
name: ahmadev
version: 0.1.0
author: Paul Houghton
description: >
   Repo-local development skill for the ahma workspace. NOT distributed.
   USE THIS SKILL for safe dependency updates (/ahmadev update),
   version bumping (/ahmadev bump), installing a local build
   (/ahmadev install), and help (/ahmadev help).
   Trigger phrases: "ahmadev", "ahmadev update", "ahmadev help",
   "safe dep update", "ahmadev bump", "bump version", "version bump",
   "update rust dependencies safely", "safe dependency upgrade",
   "cargo safe update", "update dependencies", "bump deps",
   "upgrade workspace deps", "ahmadev install", "install local build",
   "build and install ahma", "install my changes", "get the fix on my machine",
   "local release build", "install without waiting for CI".
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
| `/ahmadev install` | Build the working tree in release mode and install it to `~/.local/bin/ahma` |

---

## `/ahmadev help` — List Subcommands

When the user types `/ahmadev help`, respond with:

```
/ahmadev help      — Show this help list
/ahmadev bump      — Bump ahma version in Cargo.toml (workspace.package.version)
/ahmadev update    — Upgrade workspace dependencies (safe: ≥14d old, no known advisories)
/ahmadev install   — Build the working tree (release) and install it to ~/.local/bin/ahma
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

> **Stale-binary note:** A bump changes `CARGO_PKG_VERSION`, which can make integration tests that spawn the `ahma` binary fail against a stale `target/debug/ahma` (e.g. the `/health` semver assertion in `ahma_http_bridge`). The test harness now self-heals: `build_binary_cached` (in `ahma_mcp::test_utils::cli`) rebuilds the binary when it is **stale** — older than the newest workspace source file — so `cargo nextest run` no longer requires a manual `cargo build -p ahma_bin` first. A binary that is already fresh is used as-is and never rebuilt, so a build made with specific flags (e.g. CI's `--no-default-features`) keeps its feature set. If you ever bypass the harness, build the binary yourself before spawning it.

### Failure recovery

If the change is incorrect or compilation fails, revert files and retry:
```bash
git checkout -- Cargo.toml Cargo.lock scripts/install.sh scripts/install.ps1 skills/ahma/SKILL.md
```

### Notes

- Do not manually edit `Cargo.toml` for version bumping. Always use `cargo xtask bump-version <X.Y.Z>` as it ensures script signature consistency and smooth updates.
- This command coordinates the full release version synchronization across the repository.

---

## `/ahmadev install` — Build & Install the Local Working Tree

### What it does

Compiles the **current working tree** in release mode and installs the resulting `ahma`
binary to `~/.local/bin/ahma`, replacing whatever is on your `PATH`. This is the
"get my fix on this machine **right now**, without waiting for CI to build and publish a
release" command. It is equivalent to:

```bash
cargo build --release -p ahma_bin \
  && cp target/release/ahma ~/.local/bin/ahma.new \
  && chmod +x ~/.local/bin/ahma.new \
  && mv -f ~/.local/bin/ahma.new ~/.local/bin/ahma   # atomic rename, NOT cp-over
```

> **Why `mv` (rename), never `cp` over the live file** — ahma is almost always already
> running as your MCP server (`ahma serve …`), so `~/.local/bin/ahma` is a binary that
> mapped processes are currently executing. `cp` overwrites it **in place (same inode)**;
> on macOS/arm64 modifying the pages of a running, code-signed Mach-O invalidates its
> signature and the kernel then `SIGKILL`s every new launch of that path (`Killed: 9` /
> exit 137) — even though `codesign -v` on the static bytes still passes. Installing to a
> temp name and doing an atomic `mv` gives the path a **fresh inode** with a clean
> signature and leaves the running processes on the old inode untouched. This is exactly
> why `scripts/install.sh` uses `mv`, not `cp`.

### When to use it vs. `/ahma update`

| Command | Source | Trust | Use when |
|---------|--------|-------|----------|
| `/ahma update` (or `ahma update`) | Latest **published GitHub Release** | Sigstore Build Provenance verified | You want the official, attested release |
| `/ahmadev install` | Your **local working tree** | None — unsigned local build | You want uncommitted/unmerged changes running immediately |

Because this installs an unsigned, locally-built binary, it deliberately **skips** the
Sigstore attestation check that `scripts/install.sh` performs. Only run it on a tree you
trust (your own checkout).

### Workflow (how to invoke as an agent)

1. **Confirm the install target** matches the official location (`~/.local/bin`, the same
   `INSTALL_DIR` used by `scripts/install.sh`) and is on `PATH`:

   ```
   run_terminal_command("command -v ahma; echo \"$HOME/.local/bin\"", working_directory=".")
   ```

2. **Build + install** in one step, using a temp name + atomic `mv` (see the warning above —
   never `cp` over the live binary). The release build can take a couple of minutes, so set
   a monitor and `await` the `operation_id` rather than blocking:

   ```
   run_terminal_command(
     "cargo build --release -p ahma_bin && cp target/release/ahma \"$HOME/.local/bin/ahma.new\" && chmod +x \"$HOME/.local/bin/ahma.new\" && mv -f \"$HOME/.local/bin/ahma.new\" \"$HOME/.local/bin/ahma\"",
     working_directory=".",
     monitor_level="error"
   )
   ```

   *Note: `~/.local/bin` is outside the workspace, so the install step (`cp`/`mv`) must run
   on the **native** terminal. If the sandboxed `run_terminal_command` blocks the
   out-of-scope write, fall back to running the same command in the native terminal — the
   kernel sandbox is doing its job by refusing a write outside the workspace.*

3. **Verify** the newly installed binary is the one now resolved on `PATH`:

   ```
   run_terminal_command("ahma --version", working_directory=".")
   ```

   Confirm the reported version/build matches your working tree (e.g. compare against
   `cargo run -p ahma_bin -- --version`, or just sanity-check the version string).

### Why build only `-p ahma_bin`?

`scripts/install.sh` installs a single binary named `ahma`, produced by the `ahma_bin`
crate. Building just that package (`-p ahma_bin`) yields `target/release/ahma` and is
faster than a full `cargo build --release` of the whole workspace. Use a full workspace
release build only if you specifically want everything compiled.

### Failure recovery

- **Build fails**: fix the compile error in your working tree and re-run. Nothing was
  installed, so the previously installed `ahma` is untouched.
- **Installed `ahma` is `Killed: 9` / exits 137**: you (or an old version of this command)
  `cp`-ed over the live binary in place, invalidating its code signature on macOS. Re-run
  `/ahmadev install` — the temp-name + atomic `mv` flow replaces the path with a fresh,
  validly-signed inode and fixes it.
- **Wrong binary on PATH afterward**: run `command -v ahma` / `which -a ahma` to see which
  copy wins, and ensure `~/.local/bin` precedes any other `ahma` location in `PATH`.
- **Restore the official release**: re-run `ahma update` (or `scripts/install.sh`) to pull
  the latest attested release binary back over your local build.

### Notes

- This is a **developer convenience** command; it does not commit, push, bump, or tag.
  To actually ship changes to other users you still need `/ahmadev bump` + push to `main`
  (see the bump section — the version number is the release trigger).
- No quality pipeline is run here. Run `cargo nextest run` / clippy / fmt as part of your
  normal Definition of Done before relying on the build.

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
