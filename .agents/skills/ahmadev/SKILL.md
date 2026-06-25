---
name: ahmadev
version: 0.1.0
author: Paul Houghton
description: >
   Repo-local development skill for the ahma workspace. NOT distributed.
   USE THIS SKILL to drive a single feature from branch to squash-merged-on-main
   (/ahmadev land), cut a release (/ahmadev release, alias /ahmadev publish), hunt a regression
   (/ahmadev bisect), add test coverage where it matters most
   (/ahmadev coverage), update dependencies safely (/ahmadev update), bump the
   version (/ahmadev bump), install a local build (/ahmadev install), configure
   git for the squash-only workflow (/ahmadev gitconfig), and help
   (/ahmadev help), and simplify the changed code with before/after metrics
   (/ahmadev simplify).
   Trigger phrases: "ahmadev", "ahmadev land", "ahmadev release", "ahmadev publish",
   "publish", "publish this", "publish it", "publish the release",
   "ahmadev bisect", "ahmadev coverage", "add test coverage", "improve coverage",
   "where do we need tests", "raise coverage", "coverage report", "ahmadev update", "ahmadev help", "land this feature",
   "ahmadev gitconfig", "configure git", "git setup", "set up git config",
   "recommended git settings", "git config for ahma",
   "squash merge to main", "open a PR and merge", "merge to main", "ship it",
   "cut a release", "release this", "publish a release", "bump and release",
   "find the regression", "git bisect", "which commit broke", "find what broke",
   "revert the bad commit", "safe dep update", "ahmadev bump", "bump version",
   "version bump", "update rust dependencies safely", "safe dependency upgrade",
   "cargo safe update", "update dependencies", "bump deps", "upgrade workspace deps",
   "ahmadev install", "install local build", "build and install ahma",
   "install my changes", "get the fix on my machine", "local release build",
   "install without waiting for CI",
   "ahmadev simplify", "simplify the diff", "simplify the code", "simplify changed code",
   "clean up the diff", "refactor the diff", "reduce duplication".
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
| `/ahmadev release` *(alias: `/ahmadev publish`)* | Land any pending work, bump the version on `main`, and watch CI publish the GitHub Release after the full cross-platform test matrix passes. **This is how you ship to users.** |

**Occasional (specialists):**

| Command | Purpose |
|---------|---------|
| `/ahmadev help` | Show the process overview + subcommand list |
| `/ahmadev simplify` | Review the changed diff for reuse/simplification/efficiency/altitude issues, apply fixes, report **before/after metrics table** |
| `/ahmadev bisect` | Find the commit that introduced a regression via `git bisect run` (local, zero-CI) |
| `/ahmadev coverage` | Fan out parallel subagents across 3–10 low-coverage files, close all holes in each (≥80% target per file), land as one PR |
| `/ahmadev update` | Upgrade workspace deps that are ≥14 days old and advisory-clean |
| `/ahmadev install` | Build the working tree in release mode and install it to `~/.local/bin/ahma` (unsigned, for *this* machine — does not ship) |
| `/ahmadev gitconfig` | Configure git on this machine for the squash-only, linear-history workflow (idempotent `git config` commands; never overwrites your identity/editor). **One-time per machine/checkout.** |
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
→ `/ahmadev release`. The **version number is the release trigger**: a push to `main` builds
the 6-platform release binaries and publishes `v<X.Y.Z>` *only when it bumps Cargo.toml to a
version whose tag doesn't exist yet* (the `job-release-gate` check). An ordinary land that
doesn't touch the version runs the test matrix but skips the release build entirely. So a
release = landing a version bump on `main`. `release` syncs `main`,
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
                     (alias: /ahmadev publish — "publish" and "release" mean the same thing here)
/ahmadev simplify  — Review the diff for cleanup opportunities, apply fixes, report before/after metrics table
/ahmadev bisect    — git bisect run a repro to find the commit that introduced a regression (local, free)
/ahmadev coverage  — Fan out parallel subagents (3–10 files), close all holes per file (≥80% each), land one batch PR
/ahmadev update    — Upgrade workspace dependencies (safe: ≥14d old, no known advisories)
/ahmadev install   — Build the working tree (release) and install to ~/.local/bin/ahma (this machine only; unsigned)
/ahmadev gitconfig — Configure git for the squash-only, linear-history workflow (one-time per machine/checkout)
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

### Preflight: git-config drift check (read-only — never writes)

Before branching, run this **read-only** check. It only calls `git config --get` (safe inside the
sandbox, instant, mutates nothing) and nudges the human to run `/ahmadev gitconfig` if their git
isn't set up for the squash-only, linear-history workflow. It never edits config itself — a drift
warning is advisory, not a blocker:

```bash
for kv in pull.ff=only fetch.prune=true rebase.autostash=true; do
  k=${kv%=*}; want=${kv#*=}; got=$(git config --get "$k" || true)
  [ "$got" = "$want" ] || echo "⚠ git config $k = '${got:-unset}' (want '$want') — run /ahmadev gitconfig"
done
```

If it prints any `⚠` line, surface it to the human and suggest `/ahmadev gitconfig`, then continue
— don't block the land. If it prints nothing, git is configured; proceed silently.

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

3. **Fix and check locally** (clippy before fmt so fmt doesn't revert auto-fixes):
   ```bash
   cargo clippy --allow-dirty --fix
   cargo clippy --tests --allow-dirty --fix
   cargo fmt --all
   cargo nextest run
   ```

4. **Push, open the PR, and view it in the browser:**
   ```bash
   git push -u origin HEAD
   gh pr create --fill --base main
   gh pr view --web
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

## `/ahmadev release` (alias `/ahmadev publish`) — Land + Bump + Publish

> **`publish` is an alias for `release`.** "Publish this", "publish the release", or a bare
> "publish" all mean exactly this command — there is no separate publish step. Treat them
> identically.

### What it does

Ships a release. The **version number is the release trigger**: a push to `main` builds the
release binaries and `job-publish-release` creates the GitHub Release `v<version>` **only when
the push bumps to a version whose tag does not already exist** (decided by `job-release-gate`).
An ordinary land that doesn't change the version skips the 6-platform release build and
attestation altogether. So a release = landing a version bump on `main`.

> **The full heavy test cycle gates every publish.** `job-publish-release` `needs: ci-green`,
> and `ci-green` only passes when the entire cross-platform matrix is green on the exact bump
> commit: `job-cargo-nextest` (Linux/macOS/Windows full + ARM64 smoke), `job-android-nextest`
> (Android/Kotlin), and `job-security` (`cargo deny`). If any leg is red the release is **not**
> published — the tag is never created (`ahma update` keeps serving the previous release). Note
> the bump *lands* on `main` gated only on Fast Tier (like any land), but it only *publishes*
> after the heavy matrix passes post-merge. So a released `v<X.Y.Z>` has always cleared the full
> suite on every supported platform.

> **`cargo xtask bump-version` now also refreshes `Cargo.lock`** (every workspace member
> carries its version there, and all CI builds `--locked`). A bump commit therefore builds
> cleanly under the gate.

### Workflow (how to invoke as an agent)

0. **Preflight: git-config drift check (read-only).** Run the same read-only drift check as
   `/ahmadev land` (see its **Preflight** section — `git config --get` on `pull.ff`, `fetch.prune`,
   `rebase.autostash`). It matters extra here because release does `git pull --ff-only` and
   `git reset --hard origin/main`; if `pull.ff` isn't `only`, nudge the human to run
   `/ahmadev gitconfig` first. Advisory, not a blocker.

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

4. **Bump on a release branch, then land it exactly like `/ahmadev land`** — no duplicated
   steps here, the land flow owns quality checks, browser open, auto-merge, and local tidy:
   ```bash
   git switch -c chore/release-<X.Y.Z> origin/main
   cargo xtask bump-version <X.Y.Z>     # edits Cargo.toml, Cargo.lock, SKILL.md, install.sh, install.ps1
   git add Cargo.toml Cargo.lock skills/ahma/SKILL.md scripts/install.sh scripts/install.ps1
   git commit -m "chore(release): bump version to <X.Y.Z>"
   ```
   Then follow **`/ahmadev land` steps 3–7 exactly**: `cargo clippy --allow-dirty --fix`,
   `cargo clippy --tests --allow-dirty --fix`, `cargo fmt --all`, `cargo nextest run`,
   push, `gh pr create --fill --base main`, `gh pr view --web`,
   `gh pr merge --squash --auto --delete-branch`, `gh pr checks --watch`,
   then `git switch main && git pull --ff-only && git branch -D chore/release-<X.Y.Z>`.

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

## `/ahmadev coverage` — Drive Significant Coverage Gains Across Multiple Files

### What it does

Reads the **published** coverage report, selects a **batch of 3–10 high-value target files**,
fans out **parallel subagents** (one per file) to write comprehensive tests that close all holes
in each target, then consolidates and lands everything in one PR.

This is NOT a "add a few tests and call it done" command. Each invocation should move the
workspace coverage total **visibly** — measured in hundreds of newly-covered lines, not tens.
The minimum bar for each chosen file is **≥80% line coverage from its current baseline**. A
file at 0% with real logic should finish at 80%+. A file at 40% should finish at ≥80%. If you
write 3 tests for a 300-line file you have not done this work.

This is a coverage-*planning + writing* command, not a coverage-*measuring* one. **Do not run
`cargo llvm-cov` locally** — CI already measures coverage on every push to `main`. This command
consumes CI's measurements and writes the tests.

### Where the numbers come from (read the compact summary, never the giant HTML)

CI's `job-coverage` (in `.github/workflows/build.yml`) runs the instrumented suite once and
publishes three artifacts to GitHub Pages, rooted at `https://paulirotta.github.io/ahma/`:

| Artifact | URL | Use |
|----------|-----|-----|
| **`coverage-lowest.md`** | `https://paulirotta.github.io/ahma/coverage-lowest.md` | **Read this first.** A few-KB markdown table of every workspace file sorted ascending by line coverage, with totals — no source lines. This is the planning input. |
| `coverage-summary.json` | `https://paulirotta.github.io/ahma/coverage-summary.json` | Same data, machine-readable (`cargo llvm-cov --json --summary-only`), if you want to filter/sort programmatically. |
| Full line-by-line HTML | `https://paulirotta.github.io/ahma/html/` | Drill into this **per-file** once you have chosen targets. The per-file page shows exactly which lines are red — the input to "close the holes". |

> **Why a compact summary exists.** The raw llvm-cov HTML is tens of MB of per-line markup —
> useless for planning and ruinous for an LLM's context window. CI therefore also emits the
> sorted `coverage-lowest.md` / `coverage-summary.json`. **If you change the coverage job, keep
> these compact artifacts** — losing them forces this command back onto the HTML.
>
> If the compact artifacts are not on Pages yet (e.g. the job hasn't run since they were added),
> fetch them from the latest run's `coverage-summary` workflow artifact instead:
> `gh run download -n coverage-summary` (pick the most recent `build.yml` run on `main`), or as a
> last resort scrape the HTML index for the per-file table.

### Choosing the BATCH (not one file — a batch of 3–10)

**Do not pick a single file.** Pick 3–10 files. Rank by:

1. **Blast radius × uncovered lines.** A 0%-covered 300-line module with real branching is
   worth 10× the attention of a 0%-covered 14-line glue file. Prioritize:
   - Sandbox scope derivation, path security, approvals/gating
   - The MCP handshake and session isolation paths
   - Daemon/bridge request routing, shell-pool command construction
   - Any logic-heavy module where a silent break causes a reversion
   Integration tests are the gold standard here — they catch cross-module contracts that unit
   tests mock away. See the **Hard Invariants** section in `AGENTS.md`.

2. **Logic density.** Skip files that are pure wiring (no branching, no error paths). Prefer
   files with many `match`, `if let`, `?` chains, and error paths — those are the branches
   that break silently and never get caught without tests.

3. **Coherent groupings land faster.** Group related files in one PR when they share fixtures
   (e.g. all of `update/`, all of `sandbox/`, all of a single crate's handler layer). One PR
   touching 5 related files is faster to review than 5 separate PRs.

4. **Skip exempt trivia even at 0%:** `ahma_bin/src/main.rs`, `ahma_http_bridge/src/main.rs`
   (binary entry points — exempt per `AGENTS.md`), and test-infrastructure files (`test_utils/`
   — already covered by the tests that use them).

5. **State your batch before starting.** List each file, current %, target %, and the estimated
   uncovered lines you will cover. Total the lines — that is your PR's "lines added to coverage"
   metric. If you cannot credibly bring a file to ≥80%, note why and pick a different file.

### Depth requirement: close the holes, not check the box

For each chosen file, the goal is **systematic coverage of every branch**, not a handful of
happy-path smoke tests. Before writing tests for a file:

1. **Read the source completely** — understand every function, every error path, every `match`
   arm, every early return.
2. **Fetch and read the HTML page for that file** to see exactly which lines are red:
   `https://paulirotta.github.io/ahma/html/<crate>/src/<path>.html`
3. **Make a list of uncovered behaviors** — every red block is a gap to close.
4. **Write tests for every item on that list:**
   - Every public function (happy path AND error path)
   - Every `Result::Err` branch, every `bail!`, every `?` that can fail
   - Every early return / guard clause
   - Boundary conditions: empty input, None, max-size, zero timeout
   - Env-var-gated behavior (use the `ENV_MUTEX` + `unsafe { std::env::set_var }` pattern
     from `ahma_mcp/src/update/source.rs` to serialize env-var-touching tests)

A file previously at 20% should be at ≥80% when you are done with it. A small file at 0%
should be at 100%. Stopping at 50% is not acceptable unless the remaining lines require
network I/O, a real platform binary, or a refactor (explain which, and why).

### Build economics: fan-out writes, the orchestrator verifies once

The single most important rule for parallel fan-out: **subagents WRITE, they do not BUILD.**
Each subagent reads source and returns test code; the orchestrator compiles and runs the suite
**exactly once**, after consolidating every subagent's output.

Why this matters: N subagents each running `cargo` means **N cold builds of the entire
dependency graph in parallel** — CPU/disk saturation, and on macOS N× the
`com.apple.provenance` contamination surface (a build wedged on a denied write is the classic
symptom). The agents in a coverage batch write *disjoint* files (a different `#[cfg(test)]`
block each), so there is nothing a per-agent build verifies that the single consolidated build
does not verify better, and once.

Corollaries:

- **Do NOT give coverage subagents `isolation: worktree`.** Worktree isolation exists for
  agents that mutate the *same* files concurrently. Coverage agents touch different files, so
  they share one branch and the orchestrator merges their `#[cfg(test)]` blocks. A worktree per
  agent only buys a redundant cold build and its own contaminated `target/`. Reserve worktrees
  for genuinely conflicting parallel edits.
- **If parallel builds are ever truly unavoidable, lean on sccache — never a shared
  `CARGO_TARGET_DIR`.** sccache (which ahma now supports inside the sandbox; set
  `sandbox.trust_build_caches = true`) shares *compiled artifacts* content-addressably and
  concurrency-safely, so a second build of `ring` is a cache hit instead of a recompile. A
  shared target dir is the wrong tool: concurrent writers to one `target/` reintroduce the very
  provenance contamination above.

This generalizes beyond coverage: **any** fan-out of file-writing subagents should separate the
write phase (parallel, no builds) from a single consolidated verify phase. Isolation and
per-agent builds are costs to justify, not defaults.

### Parallel subagent workflow (one Agent per file)

After selecting the batch, fan out one `Agent` tool call per file. All agents run concurrently.
Each subagent's prompt must include:

- The exact file path and its current coverage %
- The URL of its HTML coverage page (so it can fetch the red-line map)
- Instruction to read the source completely before writing a single test
- Instruction to cover every uncovered branch identified from the HTML page
- The repo test conventions from `AGENTS.md`:
  - `tempfile::TempDir` for all file I/O (never hardcode `/tmp`, `/dev/null`, `/bin/sh`)
  - `test_utils::path_helpers` for cross-platform paths
  - Assert on success/failure AND key output (no print-only tests)
  - In-module `#[cfg(test)]` block for pure unit logic; crate `tests/` dir for integration tests
  - HTTP bridge: follow the exact 5-step handshake sequence documented in Hard Invariants
  - Env-var tests: use `LazyLock<Mutex<()>>` guard (see `update/source.rs` for the pattern)
- **Instruction to WRITE the tests and RETURN the code — NOT to build or run them.** The
  subagent must not invoke `cargo build`/`cargo nextest`/`cargo clippy`; the orchestrator
  compiles and runs the suite once, after consolidation (see *Build economics* below).
- Instruction to **return the complete test code** and a summary: test count, which lines
  each test covers, and expected new coverage %

After all agents complete, apply their output to your branch. Where agents edited the same
file, merge their `#[cfg(test)]` blocks. Then run the full suite **once** on the consolidated
result — that single build is the verification for the whole batch.

**Up to 10 files per invocation.** Launching 10 agents in parallel is normal and expected —
that is the whole point of this command. Do not serialize them; do not do this file-by-file
yourself. The agent fan-out IS the work.

### Workflow (how to invoke as an agent)

0. **Branch from canonical main** (never from local `main`):
   ```bash
   git fetch origin && git switch -c test/coverage-batch-<area> origin/main
   ```

1. **Fetch and read the compact summary:**
   ```bash
   curl -fsSL https://paulirotta.github.io/ahma/coverage-lowest.md
   # fallback: gh run download -n coverage-summary  (most recent build.yml run on main)
   ```

2. **Select and announce the batch.** Before writing any code, output a table:

   | File | Current % | Lines uncovered | Target % | Why chosen |
   |------|-----------|-----------------|----------|------------|
   | ahma_mcp/src/update/mod.rs | 9% | 275 | ≥80% | update flow, wide blast radius |
   | ... | | | | |

   Total the "lines uncovered" column. That is the impact of this PR.

3. **Fan out subagents.** One `Agent` call per file, all in the same response (parallel).
   Each agent **writes** tests and returns the test code + summary — it does **not** build or
   run them, and is **not** given `isolation: worktree` (see *Build economics* above).

4. **Consolidate.** Apply all subagent edits to your branch. Merge any overlapping `#[cfg(test)]`
   blocks. Resolve any naming conflicts between test functions.

5. **Verify the full batch — the single build for the whole fan-out:**
   ```bash
   cargo nextest run  # all tests in the workspace (or scope to touched crates)
   cargo fmt --all && cargo clippy --all-targets
   ```

6. **Land via `/ahmadev land`.** PR title: `test: coverage batch — <area> (<N> files, +<M> lines)`.
   PR description must list:
   - Each file: before % → estimated after %
   - Total new lines brought into coverage
   - Which hard invariants are now pinned by tests

### Scope discipline

- **Batch = 3–10 files per invocation.** Fewer is leaving value on the table. More starts to
  make the PR hard to review — split at 10.
- **Each file must reach ≥80%** or the subagent must explain why (network call, platform binary,
  needs a refactor first). "I ran out of time" is not an explanation.
- **Don't refactor to make code testable** as part of this command. If a file cannot be tested
  without restructuring, flag it (suggest a `refactor:` PR) and move to the next target.
- This command **does not** run the coverage tool, bump, or release. It only adds tests.

### Anti-patterns (what NOT to do)

- **Picking the smallest/easiest 0% file instead of the highest-blast-radius file.** A 14-line
  glue file at 0% is noise. A 300-line request handler at 0% is a reversion waiting to happen.
- **Writing 3–10 tests for a file and calling it done** when the file has 50+ uncovered branches.
  If you would not call this "systematic coverage", do not call it coverage.
- **Landing one file at a time in separate PRs.** The parallel subagent workflow exists precisely
  to avoid this. Ship 5–10 files in one PR; it lands in the same ~5 min as one file.
- **Picking files based on % alone without reading the source.** A file at 2% with 20 lines of
  pure `println!` is not the same as a file at 2% with 500 lines of branching logic. Read first.
- **Skipping the HTML per-file page.** The compact summary gives % and line counts. The HTML gives
  you the exact red lines. You need both: the summary to pick targets, the HTML to close the holes.
- **Letting subagents build/run cargo, or giving them `isolation: worktree`.** This is the most
  expensive anti-pattern: N agents each cold-building the dependency graph saturates the machine
  and multiplies macOS provenance contamination. Subagents write; the orchestrator builds once.
  See *Build economics* above.

---

## `/ahmadev simplify` — Simplify Changed Code with Before/After Metrics

### What it does

Reviews the diff for reuse, simplification, efficiency, and altitude issues, applies the fixes,
and bookends the work with a **mandatory before/after metrics table** that makes the value of each
run concrete and comparable across sessions. The metrics table is not optional — running simplify
and reporting no numbers is not acceptable.

Quality only: this command does not hunt for correctness bugs (use `/code-review` for that) and
does not bump, release, or push (unless the human explicitly asks to land the result).

### Mandatory metrics — collect BEFORE doing anything

Before writing a single line of code, measure every file that appears in the diff and display
this table:

| File | Lines | Duplication instances |
|------|---------|-----------------------|
| `path/to/file.rs` | N | e.g. "5× identical `Sandbox::new(…)` blocks; 4× `get_envs()→HashMap`" |

**How to collect:**
- **Lines**: `wc -l <file>` for each file in the diff.
- **Duplication instances**: read the diff, count visually identical blocks (copy-paste groups).
  Name them concisely: "N× `<short description of the block>`".
- **Complexity** (optional): if `mcp__Ahma__simplify` is available, run it on the changed files
  and add a "Complexity" column with the score. This is a bonus, not a gate.

Display the before-table and wait for the review agents before touching any code.

### Four parallel review agents (same as `/simplify`)

Launch all four concurrently. Pass each agent the diff and one angle:

| Agent | Angle |
|-------|-------|
| **Reuse** | New code that re-implements something the codebase already has — name the existing helper |
| **Simplification** | Redundant state, copy-paste variation, deep nesting, dead code — name the simpler form |
| **Efficiency** | Wasted computation, sequential independent ops, blocking work on hot paths |
| **Altitude** | Bandaid layered on shared infrastructure — fix at the right depth, not with a special case |

Each agent returns: `file`, `line`, one-line `summary`, concrete cost.

### Apply fixes and collect AFTER metrics

Dedup overlapping findings. Apply each fix. For findings skipped (behavior change, out-of-scope,
false positive), note the skip and the reason.

After all fixes are applied, measure every file that was modified and display:

| File | Lines | Duplication removed | Δ lines |
|------|-------|---------------------|---------|
| `path/to/file.rs` | N′ | e.g. "5 setup blocks → `make_test_sandbox`; 4 collection blocks → `cmd_envs`" | −ΔN |

Then a one-line summary: **"Net −N lines across K files; M duplication instances collapsed."**

If nothing was fixable, say so explicitly ("diff is already clean — no findings to apply") and
show the after-table anyway so the before/after comparison is complete.

### Example (from a real session)

**Before:**

| File | Lines | Duplication instances |
|------|---------|-----------------------|
| `ahma_mcp/src/sandbox/command.rs` | 386 | 5× `Sandbox::new(…,Test,…)` setup; 4× `get_envs()→HashMap` |

**After:**

| File | Lines | Duplication removed | Δ lines |
|------|-------|---------------------|---------|
| `ahma_mcp/src/sandbox/command.rs` | 302 | 5 setup blocks → `make_test_sandbox`; 4 collection blocks → `cmd_envs` | −84 |

**Summary:** Net −84 lines across 1 file; 9 duplication instances collapsed into 2 helpers.

### Landing the result

If fixes were applied, land via `/ahmadev land` with a `refactor:`-typed commit:
```bash
git add <modified files>
git commit -m "refactor(<scope>): <what was simplified>"
```
Then follow `/ahmadev land` steps 3–7 (clippy → fmt → nextest → push → PR → auto-merge → tidy).

---

## `/ahmadev bump [X.Y.Z]` — Bump ahma Version

### What it does

Bumps the version of the `ahma` workspace. This updates the version in `Cargo.toml` (`[workspace.package].version`) and runs `cargo xtask bump-version <X.Y.Z>` to propagate the new version across all other version-bearing files (such as installation scripts, skill files, and locks), making updates and signing run smoothly.

### Why a bump is required to ship

**The version number is the release trigger.** A push to `main` builds and attests the release binaries — and the CI publish step (`job-publish-release` in `.github/workflows/build.yml`) creates a GitHub Release — *only when the push bumps to a version whose tag `v<version>` does not already exist* (gated by `job-release-gate`). If you push to `main` without bumping, the test matrix still runs but the 6-platform release build is skipped and no new release is published — so `ahma update` and the install scripts keep serving the **previous** release artifact, and your merged changes never reach users.

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
  && mv -f ~/.local/bin/ahma.new ~/.local/bin/ahma \
  && { [ "$(uname -s)" = "Darwin" ] && \
       codesign --force --sign - --options runtime ~/.local/bin/ahma || true; }
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
>
> **Why `codesign --options runtime` on macOS** — `cargo`-built binaries are ad-hoc signed
> (`Signature=adhoc, linker-signed`) without the hardened runtime entitlement. Under memory
> pressure the OS can evict and must re-validate those code pages; without `--options runtime`
> re-validation fails and the kernel sends `SIGKILL` (`EXC_BAD_ACCESS`, termination
> namespace=`CODESIGNING`, indicator=`Invalid Page`). Adding the flag opts the binary into
> the hardened runtime, enabling safe page eviction. This is the same signing mode that
> `scripts/install.sh` applies after Sigstore attestation.

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

## `/ahmadev gitconfig` — Configure Git for the Squash-Only Workflow

### What it does

Sets the handful of git options that make `/ahmadev land`, `release`, and `bisect` run without
friction, by **emitting idempotent `git config` commands** (and applying the in-repo ones for
you where the sandbox allows). It is a **one-time** setup, run once per machine (`--global`) or
once per checkout (`--local`). It is safe to re-run — it only sets keys that are missing or
different, and **never** touches your identity or editor.

> **Honest framing — ahma does not require any of this.** The ahma MCP server / sandbox reads
> **no** git config (`~/.gitconfig` is outside the sandbox and the binary never opens it). These
> settings exist to make *the human + agent git workflow* match how `main` is actually protected:
> **squash-only merges on a linear history** (see **Gate Model**). Without them nothing breaks —
> you just hit avoidable papercuts (a `git pull` that makes a merge commit `main` will reject, a
> stale tracking ref after auto-delete, a rebase that aborts on a dirty tree).

### The settings (and why each one earns its place)

**Tier 1 — workflow-aligned (the reason this command exists):**

| `git config` key | Value | Why it matters here |
|------------------|-------|---------------------|
| `pull.ff` | `only` | `main` enforces **linear history**; `land`/`release` do `git pull --ff-only`. This makes a *bare* `git pull` refuse to create the merge commit `main` would reject — fail loud, not silently-wrong. |
| `fetch.prune` | `true` | `gh pr merge --delete-branch` removes the remote branch; `land` step 7 relies on prune to clear its now-dangling tracking ref so `git branch -D` is clean. |
| `rebase.autostash` | `true` | The "rebase your feature onto `origin/main`" path won't abort just because the tree is dirty. |
| `rerere.enabled` | `true` | The workflow does `git revert` and rebases; rerere remembers a conflict resolution and replays it next time (reverts of reverts, re-lands, etc.). |
| `rerere.autoupdate` | `true` | Stages the remembered resolution automatically, so a replayed conflict needs no re-add. |
| `merge.conflictstyle` | `zdiff3` | Clearer 3-way conflict markers (shows the common base) during those reverts/rebases. Requires Git ≥ 2.35 — skip on older git (see notes). |
| `push.autoSetupRemote` | `true` | `git push` on a fresh branch just works without `-u` (the skill passes `-u` explicitly, but this covers ad-hoc pushes). |
| `commit.verbose` | `true` | Shows the diff in the commit editor. The squash body is built from your commit messages (`squash_merge_commit_message = COMMIT_MESSAGES`) — better messages become the permanent `main` log. |

**Tier 2 — harmless sensible defaults (set them; not ahma-specific):**

| `git config` key | Value | Why |
|------------------|-------|-----|
| `init.defaultBranch` | `main` | Matches this repo; only affects *new* `git init`. |
| `branch.sort` | `-committerdate` | `git branch` lists most-recent first — handy with many short-lived feature branches. |
| `column.ui` | `auto` | Multi-column `git branch`/`status` output in a terminal. |

**Tier 3 — DO NOT prescribe (verify, never overwrite):**

| Key | Action |
|-----|--------|
| `user.name`, `user.email` | **Required for commits, but the values are the human's.** Only *check* they are set; if missing, tell the human to set them (don't invent a value). Never overwrite an existing identity. |
| `core.editor` | Pure personal preference (nano/vim/code/…). **Leave it entirely alone.** |

### Scope: `--global` vs `--local` (and the sandbox angle)

| Scope | Writes to | Reach | Can ahma apply it itself? |
|-------|-----------|-------|---------------------------|
| `--global` *(default)* | `~/.gitconfig` | **all** repos on this machine | **No** — `~/.gitconfig` is outside the workspace sandbox; the human runs the commands in their **native** terminal (prefix each with `!` in this session to run it inline). |
| `--local` | `<workspace>/.git/config` | this checkout only | **Yes (usually)** — `.git/config` is inside the workspace, so `run_terminal_command` can apply it; fall back to the native terminal if the sandbox blocks the write. |

Default to **`--global`**: the intent is "set up each client computer once" and the Tier-1 settings
are good defaults for any repo you treat as squash-only. Offer `--local` when the human wants to
keep their global git untouched (e.g. they deliberately use merge commits elsewhere) — that
variant is fully applyable from inside the sandbox.

### Workflow (how to invoke as an agent)

1. **Show current vs desired — don't blind-set.** Read what's already configured so the human
   sees exactly what will change (and that re-running is a no-op):

   ```bash
   for k in pull.ff fetch.prune rebase.autostash rerere.enabled rerere.autoupdate \
            merge.conflictstyle push.autoSetupRemote commit.verbose \
            init.defaultBranch branch.sort column.ui; do
     printf '%-22s %s\n' "$k" "$(git config --global --get "$k" || echo '(unset)')"
   done
   git config --global --get user.name  || echo 'user.name  (unset — you must set this)'
   git config --global --get user.email || echo 'user.email (unset — you must set this)'
   ```
   (Use `--local` instead of `--global` for the per-checkout variant.)

2. **Emit the apply block.** Present the exact commands. These are idempotent — running them
   again with the same values changes nothing:

   ```bash
   git config --global pull.ff only
   git config --global fetch.prune true
   git config --global rebase.autostash true
   git config --global rerere.enabled true
   git config --global rerere.autoupdate true
   git config --global merge.conflictstyle zdiff3   # needs git >= 2.35; omit on older git
   git config --global push.autoSetupRemote true
   git config --global commit.verbose true
   git config --global init.defaultBranch main
   git config --global branch.sort -committerdate
   git config --global column.ui auto
   ```

   - **`--global`:** these write `~/.gitconfig`, **outside the sandbox** — the human runs them in
     their native terminal. In this session they can paste the block with a leading `!` so it runs
     inline (e.g. `! git config --global pull.ff only`). Do **not** try to route a global write
     through `run_terminal_command`; the sandbox will (correctly) refuse it.
   - **`--local`:** swap `--global`→`--local` and run them via `run_terminal_command` from the
     workspace root; `.git/config` is in-scope. If a write is blocked, fall back to the native
     terminal (same pattern as `/ahmadev install`'s out-of-scope step).

3. **Identity check — never auto-fill.** If step 1 showed `user.name`/`user.email` unset, tell the
   human to set their own:
   ```bash
   git config --global user.name  "Your Name"
   git config --global user.email "you@example.com"
   ```
   Do not guess these from the OS, the git log, or the session — wrong identity on a commit is
   worse than an unset one. **Never** modify an already-set identity, and **never** set
   `core.editor` (leave the human's editor choice alone).

4. **Confirm.** Re-print the Tier-1/2 keys (the step-1 loop) so the human sees them all populated.

### Why `git config` commands, not editing `~/.gitconfig` text

Editing the file by hand is clobber-prone: a stray bracket can break `[user]`, and you can't tell
"already correct" from "needs change" without parsing it. `git config <key> <value>` is
idempotent, scope-aware, validates the key, and leaves every *other* section (your identity, your
editor, your aliases) untouched. Always prefer it.

### Notes

- **Idempotent + non-destructive:** safe to re-run; only sets the listed keys; never removes or
  rewrites unrelated config, identity, or editor.
- **`merge.conflictstyle zdiff3` needs Git ≥ 2.35.** Check `git --version`; on older git use
  `diff3` or omit the line — it's a nicety, not load-bearing.
- **This is a skill procedure, not a `cargo xtask`,** and intentionally so: it touches files
  *outside* the workspace (`~/.gitconfig`), which the sandbox must not write — the value here is
  the curated key set + the human-run apply step, which is documentation, not code.
- It does **not** commit, push, bump, or release. It only configures git.

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
1. **Local** (before push): `cargo clippy --allow-dirty --fix`, then `cargo clippy --tests --allow-dirty --fix`, then `cargo fmt --all`, then `cargo nextest run`.
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
