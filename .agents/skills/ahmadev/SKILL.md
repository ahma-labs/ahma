---
name: ahmadev
version: 0.1.0
author: Paul Houghton
description: >
   Repo-local development skill for the ahma workspace. NOT distributed.
   USE THIS SKILL to drive a single feature from branch to squash-merged-on-main
   (/ahmadev land), cut a release (/ahmadev release), hunt a regression
   (/ahmadev bisect), update dependencies safely (/ahmadev update), bump the
   version (/ahmadev bump), install a local build (/ahmadev install), and help
   (/ahmadev help).
   Trigger phrases: "ahmadev", "ahmadev land", "ahmadev release",
   "ahmadev bisect", "ahmadev update", "ahmadev help", "land this feature",
   "squash merge to main", "open a PR and merge", "merge to main", "ship it",
   "cut a release", "release this", "publish a release", "bump and release",
   "find the regression", "git bisect", "which commit broke", "find what broke",
   "revert the bad commit", "safe dep update", "ahmadev bump", "bump version",
   "version bump", "update rust dependencies safely", "safe dependency upgrade",
   "cargo safe update", "update dependencies", "bump deps", "upgrade workspace deps",
   "ahmadev install", "install local build", "build and install ahma",
   "install my changes", "get the fix on my machine", "local release build",
   "install without waiting for CI".
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

Two commands carry the day-to-day loop; the rest are occasional specialists.

**Everyday (the loop):**

| Command | Purpose |
|---------|---------|
| `/ahmadev land` | Drive one feature branch → PR → squash-merge on `main` through the CI gate. **This is how a fix reaches `main`.** |
| `/ahmadev release` | Land any pending work, bump the version on `main`, and watch CI publish the GitHub Release. **This is how you ship to users.** |

**Occasional (specialists):**

| Command | Purpose |
|---------|---------|
| `/ahmadev help` | Show the process overview + subcommand list |
| `/ahmadev bisect` | Find the commit that introduced a regression via `git bisect run` (local, zero-CI) |
| `/ahmadev update` | Upgrade workspace deps that are ≥14 days old and advisory-clean |
| `/ahmadev install` | Build the working tree in release mode and install it to `~/.local/bin/ahma` (unsigned, for *this* machine — does not ship) |
| `/ahmadev bump [X.Y.Z]` | Bump the version across all version-bearing files. **Building block of `release` — you rarely call it directly** (see note below) |

The intended day-to-day loop is **many small single-feature branches, each squash-merged
onto `main`**: `land` is the workhorse, `release` is `land` + a version bump, `bisect` is the
surgical undo-finder when something slips through. See the **Workflow Model** section at the
bottom for how squash-merge, `git revert`, and `git bisect` fit together.

> **On overlap / "old commands":** the set is well-factored — there is **no dead command to
> delete**. The only overlap is that `bump` is a strict sub-step of `release` (`release` runs
> `bump` for you). It stays as a standalone command for the rare case of bumping without
> landing a feature, but in the everyday loop you should reach for `release`, not `bump`.
> `install` and `/ahma update` look similar but are not duplicates: `install` puts your
> *local unsigned* build on *your* machine; `/ahma update` pulls the *published, attested*
> release for *everyone*.

---

## `/ahmadev help` — Process Overview + Subcommands

When the user types `/ahmadev help`, respond with the process overview below, then the
subcommand list.

### The development loop (what to do, in order)

This repo is built for **many small single-feature changes, each landed fast on `main`**.
The whole loop is two commands:

```
   you fix something
        │
        ▼
  /ahmadev land  ──►  branch from origin/main ─► PR ─► full cross-platform CI ─► squash-merge to main
        │                                              (merges ONLY if "CI green" passes)
        ▼
  (repeat land for each small fix…)
        │
        ▼
  /ahmadev release ─► sync main ─► bump version on main ─► CI builds + publishes GitHub Release v<X.Y.Z>
```

**Q: I fixed something — how do I drive it to `main` if it passes PR CI?**
→ `/ahmadev land`. It branches from `origin/main`, makes small conventional commits, opens a
PR, and runs `gh pr merge --squash --auto`. The change squash-merges to `main` **by itself,
the moment the full cross-platform matrix passes** (the `CI green` aggregate check). You don't
hand-merge; you don't babysit. If CI is red it simply never merges. That's the "passes PR CI →
auto-lands" you're asking for.

**Q: What do I type to publish a new release?**
→ `/ahmadev release`. The **version number is the release trigger**: every push to `main`
builds release binaries, but the publish step creates `v<X.Y.Z>` *only if that tag doesn't
already exist yet*. So a release = landing a version bump on `main`. `release` syncs `main`,
asks you to confirm the version (default: patch bump), lands the bump through the same gate,
then watches CI publish the GitHub Release. (You almost never type `/ahmadev bump` directly —
`release` runs it for you.)

**Q: Is auto-merging to `main` even a good idea?**
→ Yes, *with this design* — because "auto" does **not** mean "merge blindly." `--auto` arms
the PR so GitHub merges it **only after the required `CI green` check passes** (the full
Linux/macOS/Windows/Android matrix + clippy + cargo-deny). So the safety is identical to a
human waiting and clicking merge — minus the waiting. It's a good idea precisely because:
- changes are **small and squashed** → one revertable commit each, a clean linear `main`;
- the gate is **real and full** → nothing lands that didn't pass every platform;
- undo is a **one-liner** → `git revert <sha>` (single parent), so the cost of a wrong land is low.

  It is **not** a fire-and-forget rubber stamp. CI can't catch design mistakes, security/
  invariant regressions, or breaking API changes. So `/ahmadev land` **pauses for human
  confirmation** when a change touches sandbox/security invariants (SPEC R5/R6) or release
  signing, alters CI or branch-protection itself, breaks a public API, or hits a
  cross-platform failure that isn't an obvious flake. For routine small fixes: let it
  auto-land. The philosophy is *push-forward-and-clean-up*, with `git revert` as the net.

### Subcommand list

```
/ahmadev help      — Show this overview + subcommand list
/ahmadev land      — Branch → PR → auto squash-merge on main when "CI green" passes  ← drive a fix to main
/ahmadev release   — Land pending work + bump version on main; CI publishes the GitHub Release  ← ship to users
/ahmadev bisect    — git bisect run a repro to find the commit that introduced a regression (local, free)
/ahmadev update    — Upgrade workspace dependencies (safe: ≥14d old, no known advisories)
/ahmadev install   — Build the working tree (release) and install to ~/.local/bin/ahma (this machine only; unsigned)
/ahmadev bump      — Bump version across version-bearing files (Cargo.toml, Cargo.lock, …) — building block of release
```

Reference `/ahma help` for general ahma tooling (sandbox, livelog, run_terminal_command,
simplify, ahma update, etc.).

> **Prerequisite for the gate to actually gate (one-time bootstrap):** `/ahmadev land`'s
> `--auto` only waits for checks that branch protection marks **required**. Until `CI green`
> is a required status check *and* the merge queue is enabled on `main`, `--auto` may merge
> before the full matrix runs. See **Gate Bootstrap Status** at the bottom of this file for
> the exact settings and how to verify them before trusting auto-merge.

---

## `/ahmadev land` — Branch → PR → Squash-Merge on `main`

### What it does

Takes one focused change and lands it as a **single squashed commit** on `main`, gated on
CI. This is the workhorse of the many-small-features workflow. Squash + auto-merge are
already enabled on the repo; the merge queue runs the full cross-platform matrix at land
time and merges only when the required **`CI green`** check passes.

> **Always branch from `origin/main`, never from local `main`.** This checkout's local
> `main` can sit on a diverged/rewritten history (different root commit than `origin/main`),
> so basing work on it produces a PR full of phantom conflicts. Fetch and branch from the
> remote ref.

### Workflow (how to invoke as an agent)

1. **Branch from canonical main:**
   ```bash
   git fetch origin
   git switch -c feat/<short-slug> origin/main
   ```
   If you already have a feature branch with work, rebase it onto the latest main instead:
   ```bash
   git fetch origin && git rebase origin/main
   ```

2. **Make the change** as small, conventional commits (`feat:`, `fix:`, `refactor:` …). The
   squash body is built from these commit messages (`squash_merge_commit_message =
   COMMIT_MESSAGES`), so they become the permanent `main` log entry — write them well.

3. **Get fast local feedback** (mirrors the PR fast tier — see `.github/workflows/fast-tier.yml`):
   ```bash
   cargo fmt --all && cargo clippy --all-targets --locked && cargo nextest run --profile smoke
   ```

4. **Push and open the PR:**
   ```bash
   git push -u origin HEAD
   gh pr create --fill --base main
   ```
   The PR push triggers the **fast tier** (~5 min Linux fmt + clippy + smoke) for quick feedback.

5. **Auto-merge through the gate.** Enable squash auto-merge; GitHub adds the PR to the merge
   queue, runs the full matrix on the `merge_group` ref, and squash-merges when `CI green` passes:
   ```bash
   gh pr merge --squash --auto --delete-branch
   ```

6. **Watch it land:**
   ```bash
   gh pr checks --watch
   ```
   On success the branch is deleted and the feature is one commit on `main`.

### Why `gh pr merge`, never local `git merge --squash` + push

A local squash-and-push **bypasses the `CI green` gate** and can put red code on `main`.
Always route landings through the PR + merge queue so nothing merges untested. (Branch
protection blocks direct pushes to `main` anyway once the gate is enabled.)

### Human-intervention points (pause and ask first)

Drive routine features straight to merged, but **stop and confirm with the human** when the
change: touches the sandbox/security invariants (SPEC R5/R6) or the release-signing path;
alters CI or branch-protection itself; changes a public API in a breaking way; or when the
merge queue reports a cross-platform failure that is **not** an obvious flake. Otherwise,
the philosophy is push-forward-and-clean-up: land it, and use `git revert` if it turns out wrong.

### If it turns out wrong after landing

Because the feature is one single-parent commit, the undo is a one-liner — see
**Workflow Model** at the bottom:
```bash
git revert <sha>     # then land the revert via a PR (or /ahmadev release to ship it)
```

---

## `/ahmadev release` — Land + Bump + Publish

### What it does

Ships a release. The **version number is the release trigger**: every push to `main` builds
release binaries, but `job-publish-release` creates the GitHub Release `v<version>` **only if
that tag does not already exist**. So a release = landing a version bump on `main`.

> **`cargo xtask bump-version` now also refreshes `Cargo.lock`** (every workspace member
> carries its version there, and all CI builds `--locked`). A bump commit therefore builds
> cleanly under the gate.

### Workflow (how to invoke as an agent)

1. **Land the feature(s)** with `/ahmadev land` (skip if the work is already on `origin/main`).

2. **Sync to canonical main** (local `main` is frequently behind/diverged):
   ```bash
   git fetch origin
   git switch main && git reset --hard origin/main
   ```
   > `reset --hard` discards local-`main` state. That is intended here (local `main` carries
   > only stale duplicates of already-merged commits). If you are unsure local `main` has no
   > unpushed work, confirm with the human before resetting.

3. **Decide the version. HUMAN GATE.** Default = increment the patch component `Z` of the
   current `[workspace.package].version` in `Cargo.toml`. Show the human the diff since the
   last release tag and the proposed `vX.Y.Z`, and confirm before bumping. Only deviate from
   patch-increment if the human specifies a version.

4. **Bump on a release branch** (branch protection routes everything through PRs, including
   the bump):
   ```bash
   git switch -c chore/release-<X.Y.Z> origin/main
   cargo xtask bump-version <X.Y.Z>     # edits Cargo.toml, Cargo.lock, SKILL.md, install.sh, install.ps1
   git add Cargo.toml Cargo.lock skills/ahma/SKILL.md scripts/install.sh scripts/install.ps1
   git commit -m "chore(release): bump version to <X.Y.Z>"
   git push -u origin HEAD
   gh pr create --fill --base main
   gh pr merge --squash --auto --delete-branch
   ```

5. **Watch the publish.** When the bump lands on `main`, the main run builds the binaries and
   publishes the Release:
   ```bash
   gh run watch
   gh release view v<X.Y.Z>      # confirm it published
   ```

### Abort / failure paths

- **Main run fails:** no release publishes (the tag is never created). Fix forward with a new
  `/ahmadev land`, then re-run `/ahmadev release`. **Never** force a tag or reuse a version.
- **A released version is bad:** do not delete or rewrite the tag. `git revert` the offending
  commit, then `/ahmadev release` a new patch that ships the fix.
- **Pushed to `main` without bumping:** CI runs but nothing publishes; `ahma update` keeps
  serving the previous release. Any user-facing change you want shipped needs a bump.

---

## `/ahmadev bisect` — Find the Commit That Introduced a Regression

### When to use it (be honest)

`git bisect` is the right tool for **one** situation: a regression of **unknown origin** that
**reproduces deterministically** with a one-command test. Its advantage here is that it runs
**locally at zero CI cost**. Do **not** reach for it when:

- CI is simply red on the change you just made → **fix forward**, just read the failure.
- There is an obvious suspect commit → check that one first.
- You have no reliable repro → **write the failing regression test first**, then bisect.

### Inputs

| Input | Default |
|-------|---------|
| **bad** ref (bug reproduces here) | `origin/main` |
| **good** ref (bug absent here) | latest release tag: `gh release view --json tagName -q .tagName` |
| **repro** command (exits non-zero when the bug is present) | a narrow `cargo nextest` filter — ideally a regression test |

### Procedure

```bash
git fetch origin
# Always clean up bisect state, even on Ctrl-C or error:
trap 'git bisect reset' EXIT

git bisect start
git bisect bad  <bad-ref>      # default: origin/main
git bisect good <good-ref>     # default: last release tag

# Exit-code contract for `git bisect run`:
#   0        => commit is GOOD
#   1..124   => commit is BAD
#   125      => SKIP this commit (e.g. it does not compile)
git bisect run bash -c '
  cargo build --locked -q 2>/dev/null || exit 125
  cargo nextest run --no-default-features -E "test(<narrow_repro>)" 2>/dev/null
'
# git prints: "<sha> is the first bad commit"
git bisect reset    # (the trap also does this)
```

### Output to the human

Report the culprit and the suggested undo:
```bash
git show --stat <sha>          # what the bad commit changed (and which PR it came from)
git revert <sha>               # clean single-parent revert; land via PR, or /ahmadev release to ship
```

### Notes

- A **flaky** repro poisons bisect — make it deterministic first, or `git bisect skip`
  commits where the test genuinely can't run.
- Because every feature lands as **one squashed commit**, the culprit `<sha>` *is* the
  feature; reverting it removes exactly that feature, nothing more.
- This is intentionally a **skill procedure, not a `cargo xtask`** — the value is the
  good/bad/repro discipline and the exit-code contract, which is documentation, not code.

---

## Workflow Model — Squash-Merge, Revert, Bisect (primer)

The three tools compose cleanly, and squash-merge is what makes the other two clean:

- **Squash-merge** is how every feature lands: one PR → one commit on a **linear** `main`
  (branch protection enforces linear history). This gives a readable `git log`, a trivial
  revert, and an ideal bisect space.

- **`git revert <sha>`** undoes a landed feature. Because a squashed commit has a **single
  parent**, there is no `-m` parent-selection ambiguity — it is a clean one-liner:
  ```bash
  git revert <sha>      # creates a new commit that undoes <sha>
  ```
  - **Re-introduce later:** `git revert <revert-sha>` (revert the revert) brings the change
    back, or simply re-land the original branch as a fresh PR.
  - **Conflicts:** if `main` moved a lot since `<sha>`, the revert may conflict; resolve, then
    `git revert --continue`.
  - A revert is a normal change → it lands through a **PR + the CI gate** like anything else.
    Don't push reverts straight to `main`.

- **`git bisect`** finds an **unknown** culprit (see `/ahmadev bisect`). Use it only when a
  regression appeared, you don't know which landed feature caused it, **and** you have a
  deterministic repro. Otherwise fix forward.

**Are you over-relying on bisect?** A little — most regressions in this workflow are caught
at the merge gate (before landing) or have an obvious suspect (the last land). `git revert`
is the everyday safety net; `git bisect` is the occasional diagnostic for "it broke sometime
in the last N landed features and I have a repro." Keep features small and squashed and you
rarely need bisect — but when you do, it's surgical and free.

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
   *Note: This command updates `Cargo.toml`, `Cargo.lock` (via `cargo update --workspace`, so the `--locked` CI build passes), `skills/ahma/SKILL.md`, `scripts/install.sh`, and `scripts/install.ps1` automatically. Running this command first prevents build panics/errors caused by version mismatches between files.*
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

---

## Gate Bootstrap Status — make `--auto` actually safe (one-time setup)

`/ahmadev land` relies on `gh pr merge --squash --auto`. GitHub's auto-merge only waits for
checks that branch protection marks **required**. If `CI green` is not required, `--auto` can
merge a PR before the full cross-platform matrix has even run — silently defeating the gate.
PR #286 deliberately bootstrapped the *workflow* (the `CI green` job now exists) but could not
configure protection in the same PR, because the `CI green` status context only starts existing
**after** `build.yml` first runs on `main`. That has now happened, so the protection can be set.

### Target configuration (repo `paulirotta/ahma`, ruleset `15266938`)

| Setting | Required value | Why |
|---------|----------------|-----|
| Required status check | `CI green` | The single rename-stable gate `--auto` must wait for |
| Merge queue rule | enabled | Runs the full matrix on the `merge_group` ref, merges in order |
| `required_linear_history` | enabled | Squash-only linear `main` → clean revert/bisect |
| `non_fast_forward` | enabled (already on) | Blocks force-push to `main` |
| `allow_merge_commit` | `false` | Force squash-only landings |
| `allow_squash_merge` | `true` (already on) | The one allowed merge style |
| `delete_branch_on_merge` | `true` | Auto-clean merged feature branches |

### Verify current state

```bash
# Required checks + merge_queue + linear history present in the ruleset?
gh api repos/paulirotta/ahma/rulesets/15266938 --jq '[.rules[].type]'
# Repo merge-style + branch cleanup flags:
gh api repos/paulirotta/ahma --jq '{merge:.allow_merge_commit, squash:.allow_squash_merge, delete:.delete_branch_on_merge}'
```

### Applied vs. remaining (status: 2026-06-22)

**Applied via API (done):**
- ✅ ruleset rules: `deletion`, `non_fast_forward`, `required_linear_history`, `code_quality`
- ✅ repo flags: `allow_merge_commit=false`, `allow_rebase_merge=false`, `allow_squash_merge=true`,
  `delete_branch_on_merge=true`

**Remaining — must be done in the web UI (the API cannot):**
The `merge_queue` rule returns a `422 Invalid rule 'merge_queue'` on PUT/POST — a long-standing
GitHub REST limitation. The merge queue is **only** configurable in the web UI. And because
`build.yml` triggers on `push`/`merge_group` but **not** `pull_request`, the `CI green` check
*only runs inside the merge queue* — so requiring it without the queue would **deadlock every
PR**. Therefore add both, together, in one UI visit:

1. Repo → **Settings → Rules → Rulesets → `main`** (id 15266938).
2. Enable **Require merge queue** → method **Squash**, grouping **ALLGREEN** (defaults for the
   entry counts/timeout are fine).
3. Enable **Require status checks to pass** → add **`CI green`**.
4. Save.

Then verify the queue + check are present:
```bash
gh api repos/paulirotta/ahma/rulesets/15266938 --jq '[.rules[].type]'
# expect to include: merge_queue, required_status_checks
```

> **Until step 2–4 are done, do not trust `--auto`.** With no merge queue, `gh pr merge
> --squash --auto` would merge as soon as branch protection is satisfied — and since `CI green`
> can't run on a PR, it isn't gating. For now, land with `gh pr merge --squash` **only after**
> `gh pr checks` shows the run green, or finish the UI step above first.

> **Caution (history-rewrite constraints):** editing this ruleset is sensitive — see the
> repo's known constraints around force-pushing `main` and release-backed tags. Changing CI
> or branch-protection is a documented **human-confirmation** point in `/ahmadev land`; make
> these changes deliberately, not as part of an automated land.
