---
name: ahmadev
version: 0.1.0
author: Paul Houghton
description: >
   Repo-local development skill for the ahma workspace. NOT distributed.
   USE THIS SKILL to drive a single feature from branch to squash-merged-on-main
   (/ahmadev land), cut a release (/ahmadev release), hunt a regression
   (/ahmadev bisect), add test coverage where it matters most
   (/ahmadev coverage), update dependencies safely (/ahmadev update), bump the
   version (/ahmadev bump), install a local build (/ahmadev install), and help
   (/ahmadev help).
   Trigger phrases: "ahmadev", "ahmadev land", "ahmadev release",
   "ahmadev bisect", "ahmadev coverage", "add test coverage", "improve coverage",
   "where do we need tests", "raise coverage", "coverage report", "ahmadev update", "ahmadev help", "land this feature",
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
| `/ahmadev coverage` | Read the published coverage summary and add tests where they most reduce reversions (integration tests preferred) |
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
  /ahmadev land  ──►  branch from origin/main ─► PR ─► Fast Tier (~5m) ─► squash-merge to main
        │                                              (merges when Fast Tier passes)        │
        │                                                                                     ▼
        │                                                       main: full matrix (~30m) runs post-merge;
        │                                                       red ⇒ fix out-of-band (revert / forward-fix)
        ▼
  (repeat land for each small fix… landings don't wait on the 30m matrix)
        │
        ▼
  /ahmadev release ─► sync main ─► bump version on main ─► CI builds + publishes GitHub Release v<X.Y.Z>
```

**Q: I fixed something — how do I drive it to `main` if it passes PR CI?**
→ `/ahmadev land`. It branches from `origin/main`, makes small conventional commits, opens a
PR, and runs `gh pr merge --squash --auto --delete-branch`. The change squash-merges to `main`
**by itself, the moment the Fast Tier check passes** (~5 min: Linux fmt + clippy + smoke tests).
You don't hand-merge; you don't babysit. The full cross-platform matrix then runs on `main`
*after* the merge (it can't gate the PR — it doesn't run on PRs) as a safety net.

**Q: What do I type to publish a new release?**
→ `/ahmadev release`. The **version number is the release trigger**: every push to `main`
builds release binaries, but the publish step creates `v<X.Y.Z>` *only if that tag doesn't
already exist yet*. So a release = landing a version bump on `main`. `release` syncs `main`,
asks you to confirm the version (default: patch bump), lands the bump through the same gate,
then watches CI publish the GitHub Release. (You almost never type `/ahmadev bump` directly —
`release` runs it for you.)

**Q: Is auto-merging to `main` even a good idea?**
→ Yes, *with this optimistic design* — because "auto" does **not** mean "merge blindly." `--auto`
arms the PR so GitHub merges it **only after the required Fast Tier check passes**. This trades
*pre-merge* cross-platform certainty for **fast, non-blocking landings**: the full matrix runs
post-merge, and a rare platform break is fixed out-of-band without blocking the features that
landed behind it. It's a good idea here precisely because:
- changes are **small and squashed** → one revertable commit each, a clean linear `main`;
- undo is a **one-liner** → `git revert <sha>` (single parent), so the cost of a wrong land is low;
- the full matrix is *off the critical path* → you land in ~5 min instead of waiting ~30.

  It is **not** a fire-and-forget rubber stamp. Fast Tier (and CI generally) can't catch design
  mistakes, security/invariant regressions, or breaking API changes — and it doesn't run the
  other-OS suites at all before merge. So `/ahmadev land` **pauses for human confirmation** when a
  change touches sandbox/security invariants (SPEC R5/R6) or release signing, alters CI or
  branch-protection itself, breaks a public API, or is otherwise platform-sensitive. For routine
  small fixes: let it auto-land. The philosophy is *push-forward-and-clean-up*, with `git revert`
  as the net. See **Gate Model** at the bottom for the full rationale (and why there's no merge queue).

### Subcommand list

```
/ahmadev help      — Show this overview + subcommand list
/ahmadev land      — Branch → PR → auto squash-merge on main when Fast Tier passes  ← drive a fix to main
/ahmadev release   — Land pending work + bump version on main; CI publishes the GitHub Release  ← ship to users
/ahmadev bisect    — git bisect run a repro to find the commit that introduced a regression (local, free)
/ahmadev coverage  — Read the published coverage summary; add tests where they most reduce reversions
/ahmadev update    — Upgrade workspace dependencies (safe: ≥14d old, no known advisories)
/ahmadev install   — Build the working tree (release) and install to ~/.local/bin/ahma (this machine only; unsigned)
/ahmadev bump      — Bump version across version-bearing files (Cargo.toml, Cargo.lock, …) — building block of release
```

Reference `/ahma help` for general ahma tooling (sandbox, livelog, run_terminal_command,
simplify, ahma update, etc.).

> **The gate is live:** `main` requires the **Fast Tier** status check, so `/ahmadev land`'s
> `gh pr merge --squash --auto` is safe — it merges only when Fast Tier passes. The full
> cross-platform matrix runs *post-merge* on `main` as a safety net. See **Gate Model** at the
> bottom for the rationale and the exact settings (and why there's no merge queue).

---

## `/ahmadev land` — Branch → PR → Squash-Merge on `main`

### What it does

Takes one focused change and lands it as a **single squashed commit** on `main`, gated on the
**Fast Tier** check. This is the workhorse of the many-small-features workflow. Squash +
auto-merge are enabled on the repo; `--auto` squash-merges as soon as the required Fast Tier
check (~5 min) passes. The full cross-platform matrix runs *post-merge* on `main` (see
**Gate Model**).

> **Always branch from `origin/main`, never from local `main`.** This checkout's local
> `main` can sit on a diverged/rewritten history (different root commit than `origin/main`),
> so basing work on it produces a PR full of phantom conflicts. Fetch and branch from the
> remote ref.

> **Don't stack feature branches.** Squash-merge rewrites each landed feature into a *new*
> single commit, so a branch based on another *unmerged* feature will replay that feature's
> old commits and conflict. Branch every feature from fresh `origin/main` and serialize —
> landing takes only ~5 min, so the cost is tiny. If you must work ahead, rebase the upper
> branch onto `origin/main` after the lower one lands:
> `git rebase --onto origin/main <lower-branch-old-tip> <upper-branch>`.

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

5. **Auto-merge through the gate.** Arm squash auto-merge; GitHub squash-merges when the
   required **Fast Tier** check passes:
   ```bash
   gh pr merge --squash --auto --delete-branch
   ```

6. **Watch it land, then watch the post-merge matrix (code changes):**
   ```bash
   gh pr checks --watch       # Fast Tier → auto-merges; remote branch auto-deleted
   gh run watch               # the ~30 min full matrix now running on main; red ⇒ fix out-of-band
   ```
   For docs-only changes you can skip the second watch.

7. **Tidy up locally** (remote branch is already gone; `fetch.prune` clears its tracking ref):
   ```bash
   git switch main && git pull --ff-only && git branch -D <branch>   # -D: squash isn't seen as "merged"
   ```

### Why `gh pr merge`, never local `git merge --squash` + push

A local squash-and-push **bypasses the Fast Tier gate** and can put unbuildable code on `main`.
Always route landings through the PR so nothing merges unchecked. (Branch protection — required
status check + `enforce_admins` + blocked force-push — refuses direct pushes to `main` anyway.)

### Human-intervention points (pause and ask first)

Drive routine features straight to merged, but **stop and confirm with the human** when the
change: touches the sandbox/security invariants (SPEC R5/R6) or the release-signing path;
alters CI or branch-protection itself; changes a public API in a breaking way; or is
platform-sensitive (Windows/macOS/Android paths) — Fast Tier won't catch an other-OS break
before it lands, so these warrant extra care. Otherwise, the philosophy is
push-forward-and-clean-up: land it, watch the post-merge matrix, and `git revert` (or
forward-fix) if it turns out wrong.

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

## `/ahmadev coverage` — Add Tests Where They Most Reduce Reversions

### What it does

Reads the **published** coverage report, picks the *one* area where added tests will do the
most to stop code from being reverted, writes those tests (favoring **integration** tests where
the value lives in cross-module behavior), confirms the new tests pass and actually exercise the
target code, then lands them through the normal `/ahmadev land` gate.

This is a coverage-*planning* command, not a coverage-*measuring* one. **Do not run
`cargo llvm-cov` locally** — the coverage tool instruments and writes profraw/profdata files and
is not designed to run inside ahma's kernel sandbox. CI already measures coverage on every push
to `main`; this command *consumes* that measurement and adds tests with reasoning.

### Where the numbers come from (read the compact summary, never the giant HTML)

CI's `job-coverage` (in `.github/workflows/build.yml`) runs the instrumented suite once and
publishes three artifacts to GitHub Pages, rooted at `https://paulirotta.github.io/ahma/`:

| Artifact | URL | Use |
|----------|-----|-----|
| **`coverage-lowest.md`** | `https://paulirotta.github.io/ahma/coverage-lowest.md` | **Read this first.** A few-KB markdown table of every workspace file sorted ascending by line coverage, with totals — no source lines. This is the planning input. |
| `coverage-summary.json` | `https://paulirotta.github.io/ahma/coverage-summary.json` | Same data, machine-readable (`cargo llvm-cov --json --summary-only`), if you want to filter/sort programmatically. |
| Full line-by-line HTML | `https://paulirotta.github.io/ahma/html/` | **Only** drill in here for a *single* already-chosen file. It is large and per-line — reading it broadly swamps context with trivia. |

> **Why a compact summary exists.** The raw llvm-cov HTML is tens of MB of per-line markup —
> useless for planning and ruinous for an LLM's context window. CI therefore also emits the
> sorted `coverage-lowest.md` / `coverage-summary.json`. **If you change the coverage job, keep
> these compact artifacts** — losing them forces this command back onto the HTML.
>
> If the compact artifacts are not on Pages yet (e.g. the job hasn't run since they were added),
> fetch them from the latest run's `coverage-summary` workflow artifact instead:
> `gh run download -n coverage-summary` (pick the most recent `build.yml` run on `main`), or as a
> last resort scrape the HTML index for the per-file table.

### Choosing the target (the judgment that makes this command worth running)

Pick where coverage most **reduces reversions** — not whatever has the lowest percentage.
Rank candidates by *blast radius if it breaks silently*, then by how cheaply a test pins the
behavior:

1. **Prefer code on a hard invariant or a wide call path.** Sandbox scope derivation, path
   security, approvals/gating, the MCP handshake, session isolation, daemon/bridge request
   routing, shell-pool command construction — a silent break here is exactly what gets reverted.
   These are also where **integration tests are the gold standard**: they catch the cross-module
   contract a unit test mocks away. See the **Hard Invariants** section in `AGENTS.md` (HTTP
   handshake, `-32001` sandbox gating, no print-only tests) — encode those as assertions.
2. **Then prefer big, untested, logic-heavy files.** A 0%/low-coverage module with real branching
   (e.g. `ahma_mcp/src/daemon_reporter.rs`, `ahma_mcp/src/service_builder.rs`,
   `ahma_mcp/src/shell/cli/commands.rs`, `ahma_core/src/approvals.rs`) is high-yield: many
   uncovered branches per test.
3. **Skip the trivia even though it shows 0%.** Binary entry points (`ahma_bin/src/main.rs`,
   `ahma_http_bridge/src/main.rs`) and pure glue are **explicitly exempt** from the ≥80% target in
   `AGENTS.md` (tested via CLI integration). Coverage there is cosmetic and rarely prevents a
   revert — don't burn the change on it.

State the chosen target and the one-sentence reason ("breaks silently + wide blast radius +
currently N% over M lines") before writing tests.

### Workflow (how to invoke as an agent)

1. **Branch from canonical main** (same rule as `land` — never from local `main`):
   ```bash
   git fetch origin && git switch -c test/coverage-<area> origin/main
   ```

2. **Fetch the compact summary** and read totals + the worst files:
   ```bash
   curl -fsSL https://paulirotta.github.io/ahma/coverage-lowest.md
   # fallback if not published yet:  gh run download -n coverage-summary
   ```

3. **Choose ONE target** using the ranking above and announce it with its reason.

4. **Read the target source** with native file tools and identify the uncovered behavior worth
   pinning. For one chosen file you may open its HTML page
   (`https://paulirotta.github.io/ahma/html/<path>.html`) to see exactly which lines are red.

5. **Write tests, integration-first where it fits.** Put cross-module/workflow tests in the
   workspace/crate `tests/` dir; reserve in-module `#[cfg(test)]` for pure unit logic. Obey the
   repo test rules in `AGENTS.md`: `tempfile::TempDir` for all file I/O, `test_utils::path_helpers`
   for paths (never hardcode `/tmp`, `/bin/sh`, `/dev/null`), assert on success/failure **and** key
   output (no print-only tests), and for the HTTP bridge follow the exact handshake sequence.

6. **Verify locally** — tests must pass *and* genuinely exercise the target (a test that doesn't
   hit the uncovered lines adds nothing):
   ```bash
   cargo nextest run -E 'test(<your_new_tests>)' --no-default-features
   cargo fmt --all && cargo clippy --all-targets --locked
   ```

7. **Land it** via `/ahmadev land` (`test:`-typed commits). Coverage is re-measured by CI on the
   next push to `main`; check `coverage-lowest.md` after it lands to confirm the target moved.

### Scope discipline (avoid the rabbit hole)

- **One area per invocation.** Resist "while I'm here" sprawl across crates — small, single-target
  test PRs land in ~5 min and keep `main` linear. Re-run the command for the next area.
- **Don't refactor to make code testable as part of this command** unless trivial; if the target
  needs restructuring to be tested, say so and land that separately (it's a `refactor:`, and may be
  a human-confirmation point if it touches an invariant).
- This command **does not** run the coverage tool, bump, or release. It only adds tests.

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

## Gate Model — how `main` is actually protected (live as of 2026-06-22)

> **Design note — no merge queue.** PR #286 envisioned a GitHub *merge queue* running the full
> matrix on the `merge_group` ref at land time. That turned out to be **unavailable** for this
> repo: the `merge_queue` ruleset rule 422s via the REST API, and the toggle is absent from both
> the rulesets UI and the classic branch-protection editor (public Free-tier user repo). So the
> queue-based design was abandoned in favour of the **optimistic model** below.

### The optimistic model (Fast-Tier-gates, full-matrix-post-merge)

Landing is gated on the cheap **Fast Tier** check; the expensive cross-platform matrix runs
**after** merge, off the critical path. This optimizes *time-to-landed* and keeps the pipeline
non-blocking — at the cost of catching Windows/macOS/Android/full-suite breaks *after* a feature
lands (fix out-of-band, never blocks the features that landed behind it).

Three gates, cheapest first:
1. **Local** (before push): `cargo fmt --all && cargo clippy --all-targets --locked && cargo nextest run --profile smoke`.
2. **Fast Tier on the PR (~5 min) — the required merge gate.** `fast-tier.yml` runs on
   `pull_request`; `--auto` waits for it. A clean-room re-run of gate 1 (catches uncommitted
   files / stale `Cargo.lock` / dirty-tree bugs).
3. **Full matrix on `main` (~30 min) — post-merge safety net.** `build.yml` runs on `push` to
   `main`. Not a required check; it can't gate (it doesn't run on PRs). If it goes red → fix
   out-of-band (new branch from current `main`; `git revert <sha>` for a clean undo, or a
   forward-fix if later features depend on the bad code).

### Live configuration (verify with the commands below)

| Setting | Value | Why |
|---------|-------|-----|
| Required status check (classic protection on `main`) | `Fast Tier (fmt + clippy + smoke)` | The single gate `--auto` waits for |
| "Require branches up to date" (`strict`) | **off** | Forcing a rebase between every land kills throughput; the post-merge matrix covers staleness |
| `enforce_admins` ("Do not allow bypassing") | on | Rules apply to the owner too |
| `allow_force_pushes` / `allow_deletions` (on `main`) | off / off | Protect history |
| ruleset `15266938` rules | `deletion`, `non_fast_forward`, `required_linear_history`, `code_quality` | Linear squash-only history → clean revert/bisect |
| `allow_merge_commit` / `allow_rebase_merge` | `false` / `false` | Squash-only landings |
| `allow_squash_merge` / `delete_branch_on_merge` | `true` / `true` | One merge style; auto-clean merged branches |

### Verify current state

```bash
gh api repos/paulirotta/ahma/branches/main/protection --jq '{required:[.required_status_checks.checks[].context], strict:.required_status_checks.strict, admins:.enforce_admins.enabled, force:.allow_force_pushes.enabled}'
gh api repos/paulirotta/ahma/rulesets/15266938 --jq '[.rules[].type]'
gh api repos/paulirotta/ahma --jq '{merge:.allow_merge_commit, rebase:.allow_rebase_merge, squash:.allow_squash_merge, delete:.delete_branch_on_merge}'
```

> **`--auto` is now safe and real.** `gh pr merge --squash --auto --delete-branch` merges each PR
> the moment **Fast Tier** passes. Watch the post-merge `main` build (`gh run watch`) for code
> changes; for docs-only changes you can ignore it.

> **Editing protection via API vs UI.** The GitHub *web UI* triggers a "Confirm access" (sudo /
> 2FA) prompt for these settings; the `gh` CLI token edits them directly without sudo
> (`gh api -X PUT repos/<o>/<r>/branches/main/protection --input <json>`). Prefer the CLI. This
> is a CI/branch-protection change → a documented human-confirmation point; respect
> [[github-history-rewrite-constraints]] when touching the ruleset.

> **Caution (history-rewrite constraints):** editing this ruleset is sensitive — see the
> repo's known constraints around force-pushing `main` and release-backed tags. Changing CI
> or branch-protection is a documented **human-confirmation** point in `/ahmadev land`; make
> these changes deliberately, not as part of an automated land.
