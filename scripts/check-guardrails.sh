#!/bin/bash
# Unified guardrail checks for local commit/push workflows.
#
# Usage:
#   ./scripts/check-guardrails.sh --phase commit
#   ./scripts/check-guardrails.sh --phase push
#   ./scripts/check-guardrails.sh --phase commit --allow-dirty
#
# Recommended:
# - Before commit: run with --phase commit
# - Before push:   run with --phase push (requires clean working tree)

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

PHASE="push"
ALLOW_DIRTY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --phase)
      PHASE="${2:-}"
      shift 2
      ;;
    --allow-dirty)
      ALLOW_DIRTY=1
      shift
      ;;
    *)
      # Installed as .git/hooks/pre-push, git invokes us with positional
      # <remote-name> <remote-url> (plus ref updates on stdin) — ignore
      # anything that isn't one of our own flags rather than rejecting it.
      shift
      ;;
  esac
done

if [[ "$PHASE" != "commit" && "$PHASE" != "push" ]]; then
  echo "Invalid phase: '$PHASE' (expected 'commit' or 'push')"
  exit 2
fi

echo "Running guardrail checks (phase: $PHASE)..."

if [[ "$ALLOW_DIRTY" -ne 1 ]]; then
  DIRTY_STATUS="$(git status --porcelain)"
  if [[ -n "$DIRTY_STATUS" ]]; then
    echo "FAIL Working tree is not clean."
    echo ""
    echo "$DIRTY_STATUS"
    echo ""
    echo "Commit/stash/remove local changes (including untracked files), or rerun with --allow-dirty."
    exit 1
  fi
  echo "OK Working tree is clean"
else
  echo "WARNING️  --allow-dirty enabled (clean tree check skipped)"
fi

echo "=== Guardrail: SKILL.md self-containment (no relative file links) ==="
# Skills are installed standalone to ~/.agents/skills/ — external relative links break.
# All cross-references must use absolute GitHub URLs, not relative paths like ](docs/foo.md).
SKILL_LINK_VIOLATIONS=$(grep -rn '](docs/' skills .agents/skills 2>/dev/null | grep -v 'https://' || true)
if [[ -n "$SKILL_LINK_VIOLATIONS" ]]; then
  echo ""
  echo "FAIL SKILL.md files contain relative docs/ links that break when installed standalone:"
  echo "$SKILL_LINK_VIOLATIONS"
  echo ""
  echo "Replace relative paths with absolute GitHub URLs:"
  echo "  ](docs/foo.md)  ->  ](https://github.com/ahma-labs/ahma/blob/main/docs/foo.md)"
  exit 1
fi
echo "OK No relative docs/ links in SKILL.md files"

echo "=== Guardrail: SKILL.md size limit (R-SK3, 500 lines) ==="
# SPEC.md R-SK3: skills must stay dense and link to docs/ for deep dives, so a skill
# growing unboundedly with every feature (as skills/ahma/SKILL.md did, 1152 -> 1244 lines
# across two PRs) goes unnoticed until an agent has to load a huge file for a small task.
SKILL_SIZE_VIOLATIONS=""
while IFS= read -r -d '' skill_file; do
  LINE_COUNT=$(wc -l < "$skill_file")
  if [[ "$LINE_COUNT" -gt 500 ]]; then
    SKILL_SIZE_VIOLATIONS+="  $skill_file: $LINE_COUNT lines (limit 500)"$'\n'
  fi
done < <(find skills -mindepth 2 -name 'SKILL.md' -print0)
if [[ -n "$SKILL_SIZE_VIOLATIONS" ]]; then
  echo ""
  echo "FAIL SKILL.md files exceed the R-SK3 500-line cap:"
  echo "$SKILL_SIZE_VIOLATIONS"
  echo "Move worked examples and deep-dive prose into docs/<feature>.md and link to it instead."
  exit 1
fi
echo "OK All SKILL.md files are within the 500-line cap"

echo "=== Guardrail: skill version consistency with Cargo.toml ==="
CARGO_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
AHMA_SKILL_VER=$(grep '^version:' skills/ahma/SKILL.md | head -1 | awk '{print $2}')
SKILL_HTML_VER=$(grep '<!-- version:' skills/ahma/SKILL.md | head -1 | sed -E 's/.*version: ([0-9]+\.[0-9]+\.[0-9]+).*/\1/')
INSTALL_SH_VER=$(grep -m1 'AHMA_VERSION="' scripts/install.sh | awk -F'"' '{print $2}')
INSTALL_PS1_VER=$(grep -m1 "Version '" scripts/install.ps1 | awk -F"'" '{print $2}')

SKILL_VER_FAIL=0
if [ "$AHMA_SKILL_VER" != "$CARGO_VER" ]; then
  echo "FAIL skills/ahma/SKILL.md version: ${AHMA_SKILL_VER} != Cargo.toml version ${CARGO_VER}"
  SKILL_VER_FAIL=1
fi
if [ "$SKILL_HTML_VER" != "$CARGO_VER" ]; then
  echo "FAIL skills/ahma/SKILL.md HTML comment version: ${SKILL_HTML_VER} != Cargo.toml version ${CARGO_VER}"
  SKILL_VER_FAIL=1
fi
if [ "$INSTALL_SH_VER" != "$CARGO_VER" ]; then
  echo "FAIL scripts/install.sh AHMA_VERSION: ${INSTALL_SH_VER} != Cargo.toml version ${CARGO_VER}"
  SKILL_VER_FAIL=1
fi
if [ "$INSTALL_PS1_VER" != "$CARGO_VER" ]; then
  echo "FAIL scripts/install.ps1 Version call: ${INSTALL_PS1_VER} != Cargo.toml version ${CARGO_VER}"
  SKILL_VER_FAIL=1
fi
if [ "$SKILL_VER_FAIL" -ne 0 ]; then
  echo ""
  echo "  Run: cargo xtask bump-version ${CARGO_VER}"
  echo "  to sync all version strings to the Cargo.toml value."
  exit 1
fi
echo "OK Version strings consistent (v${CARGO_VER})"

echo "=== Guardrail: requirement ids cited in code are defined in a spec ==="
./scripts/check-spec-ids.sh

echo "=== Guardrail: crate root preflight (src/lib.rs or src/main.rs) ==="
missing=0
while IFS= read -r manifest; do
  crate_dir="$(dirname "$manifest")"

  # Workspace root can have Cargo.toml without a package section.
  if ! grep -q "^\[package\]" "$manifest"; then
    continue
  fi

  if [[ ! -f "$crate_dir/src/lib.rs" && ! -f "$crate_dir/src/main.rs" ]]; then
    echo "Missing crate root in $crate_dir (expected src/lib.rs or src/main.rs)"
    missing=1
  fi
done < <(find . -name Cargo.toml -not -path "./target/*")

if [[ "$missing" -ne 0 ]]; then
  echo ""
  echo "FAIL Crate root preflight failed."
  echo "Likely cause: required files exist locally but are untracked or misnamed."
  exit 1
fi
echo "OK Crate root preflight passed"

echo "=== Guardrail: no per-crate edition or rust-version overrides ==="
# All crates must inherit edition and rust-version from the workspace.
# Local overrides silently break MSRV guarantees across the workspace.
EDITION_OVERRIDE_FAIL=0
while IFS= read -r manifest; do
  crate_dir="$(dirname "$manifest")"
  # Skip workspace root (it is allowed to define edition/rust-version).
  if ! grep -q "^\[package\]" "$manifest"; then
    continue
  fi
  if grep -Eq '^edition\s*=' "$manifest"; then
    echo "FAIL $manifest defines its own 'edition'; remove it and inherit from [workspace.package]."
    EDITION_OVERRIDE_FAIL=1
  fi
  if grep -Eq '^rust-version\s*=' "$manifest"; then
    echo "FAIL $manifest defines its own 'rust-version'; remove it and inherit from [workspace.package]."
    EDITION_OVERRIDE_FAIL=1
  fi
done < <(find . -name Cargo.toml -not -path "./target/*")
if [[ "$EDITION_OVERRIDE_FAIL" -ne 0 ]]; then
  echo ""
  echo "  Each member crate must use:"
  echo "    edition.workspace = true"
  echo "    rust-version.workspace = true"
  exit 1
fi
echo "OK No per-crate edition/rust-version overrides"

echo "=== Guardrail: no println!/print! on the protocol path (SPEC R5.6.1) ==="
# `println!` panics unconditionally on a write error. In stdio server mode and on
# the terminal-hook path, stdout is a PIPE the peer may close during shutdown —
# a broken pipe (EPIPE, or Windows OS error 232) then kills the process with a
# stack trace instead of a log line. `crate::utils::stdio::emit_stdout_notification`
# / `emit_stdout_text` classify that error instead. `println!` stays fine in CLI
# mode, where stdout is a terminal, which is why this is scoped to the modules
# that only ever run under a protocol peer rather than applied workspace-wide.
#
# This was documented in SPEC R5.6.1, in AGENTS.md, and in the stdio module's own
# header — and a `println!` on the hook execution path survived all three,
# because nothing checked. Hence a check.
PROTOCOL_DIRS=(
  ahma_mcp/src/mcp_service
  ahma_mcp/src/adapter
  ahma_mcp/src/sandbox
  ahma_mcp/src/livelog
  ahma_output_optimizer/src
  ahma_http_bridge/src
)
STDOUT_VIOLATIONS=$(grep -rn --include='*.rs' -E '(^|[^a-z_])print(ln)?!' "${PROTOCOL_DIRS[@]}" 2>/dev/null \
  | grep -v 'eprintln!' | grep -v 'eprint!' || true)
if [[ -n "$STDOUT_VIOLATIONS" ]]; then
  echo ""
  echo "FAIL println!/print! found on a protocol-path module:"
  echo "$STDOUT_VIOLATIONS"
  echo ""
  echo "  These write to a pipe the peer can close. Use:"
  echo "    crate::utils::stdio::emit_stdout_notification(json)  # JSON-RPC"
  echo "    crate::utils::stdio::emit_stdout_text(text)          # raw output"
  exit 1
fi
echo "OK No println!/print! on the protocol path"

echo "=== Guardrail: no Result-shaped use of a parking_lot guard ==="
# `parking_lot`'s `lock()`/`read()`/`write()` return the guard directly, not a
# `Result` — there is no poisoning. Code written against `std::sync` treats them
# as a `Result`, and that mistake only shows up where the code is *compiled*.
#
# Both times it bit, it bit in platform-gated code a developer on another OS
# cannot build: once under `#[cfg(target_os = "windows")]` (found by hand), once
# under `#[cfg(target_os = "linux")]` (found by CI, after the local suite,
# clippy, and a full `--run-ignored all` run were all green on macOS). A grep is
# the only check that sees every `#[cfg]` arm at once.
#
# Only Result-*only* consumers are flagged. `.map`/`.unwrap_or` are ambiguous —
# a guard derefs to its inner value, so `guard.map(..)` is legal when that value
# is an `Option` — and flagging them would train people to ignore this.
LOCK_RESULT_MISUSE=$(
  grep -rnE '\.(lock|read|write)\(\)[[:space:]]*(\.ok\(\)|\.map_err\(|\.is_ok\(\)|\.is_err\(\))' \
    --include='*.rs' ahma_*/src 2>/dev/null | grep -v 'tokio::sync' || true
  # `.ok()` etc. on the line after a trailing `.read()` / `.lock()`.
  grep -rn -A1 -E '\.(lock|read|write)\(\)$' --include='*.rs' ahma_*/src 2>/dev/null \
    | grep -E '^[^ ]+-[0-9]+-[[:space:]]*(\.ok\(\)|\.map_err\(|\.is_ok\(\)|\.is_err\(\))' || true
  # `if let Ok(g) = x.lock()` / `match x.lock() {` without a leading deref.
  grep -rnE 'if let Ok\([^)]*\)[[:space:]]*=[^;]*\.(lock|read|write)\(\)' \
    --include='*.rs' ahma_*/src 2>/dev/null | grep -v 'tokio::sync' || true
  grep -rnE 'match[[:space:]]+[^*{]*\.(lock|read|write)\(\)[[:space:]]*\{' \
    --include='*.rs' ahma_*/src 2>/dev/null | grep -v 'tokio::sync' || true
)
if [[ -n "$LOCK_RESULT_MISUSE" ]]; then
  echo ""
  echo "FAIL A parking_lot guard is being used as if it were a Result:"
  echo "$LOCK_RESULT_MISUSE"
  echo ""
  echo "  parking_lot locks cannot be poisoned, so lock()/read()/write() hand back the"
  echo "  guard directly. Drop the .ok()/.unwrap()/if-let-Ok and use the guard, or"
  echo "  deref it first (\`match *x.read() { .. }\`)."
  exit 1
fi
echo "OK No Result-shaped use of a parking_lot guard"

echo "=== Guardrail: lint recurring test patterns ===" 
./scripts/lint_test_paths.sh

echo "=== Guardrail: workspace license boundaries ==="
bash ./scripts/check-license-boundaries.sh

echo "=== Guardrail: workspace cargo check ==="
cargo check --workspace --locked

echo "=== Guardrail: dependency graph invariants (hakari in sync, one crypto provider) ==="
# Shared with PR CI so a PR cannot land a stale workspace-hack/ or a second
# rustls crypto provider; see the script for the rationale of each invariant.
bash ./scripts/check-dependency-graph.sh

echo "=== Guardrail: integration tests live in per-harness-class binaries ==="
# Every top-level tests/*.rs file is its own statically-linked executable carrying
# the full dependency closure, relinked for every feature flavour CI builds. The
# integration tests are therefore grouped into ONE binary per harness class
# (see the layout table in .config/nextest.toml), and nextest's throttle/retry
# filters key off those binary names. Cargo turns every `tests/*.rs` AND every
# `tests/*/main.rs` into a test target, so both shapes are checked: a new one of
# either would both bring back the per-file link cost and escape the structural
# filters. Only the roots listed here may exist.
ALLOWED_TEST_ROOTS=(
  ahma_mcp/tests/unit/main.rs
  ahma_mcp/tests/e2e/main.rs
  ahma_mcp/tests/latency_guard_test.rs                # all #[ignore]; ignored-tests.yml targets it
  ahma_simplify/tests/integration/main.rs
  ahma_http_bridge/tests/e2e/main.rs
  ahma_http_bridge/tests/unit/main.rs
  ahma_http_bridge/tests/stress/main.rs
  ahma_tui/tests/connection/main.rs
  ahma_core/tests/integration/main.rs
  ahma_http_mcp_client/tests/integration/main.rs
  ahma_llm_monitor/tests/integration/main.rs
)
LAYOUT_VIOLATIONS=""
while IFS= read -r test_root; do
  test_root="${test_root#./}"
  allowed=0
  for ok in "${ALLOWED_TEST_ROOTS[@]}"; do
    if [[ "$test_root" == "$ok" ]]; then
      allowed=1
      break
    fi
  done
  if [[ "$allowed" -ne 1 ]]; then
    LAYOUT_VIOLATIONS+="$test_root"$'\n'
  fi
done < <({
  find . -mindepth 3 -maxdepth 3 -type f -path '*/tests/*.rs' -not -path './target/*'
  find . -mindepth 4 -maxdepth 4 -type f -path '*/tests/*/main.rs' -not -path './target/*'
} | sort)
if [[ -n "$LAYOUT_VIOLATIONS" ]]; then
  echo ""
  echo "FAIL New integration test root file(s) found:"
  printf '%s' "$LAYOUT_VIOLATIONS"
  echo ""
  echo "  Each tests/*.rs or tests/*/main.rs is a separate executable that relinks the"
  echo "  whole dependency closure and dodges the binary_id() throttle/retry filters in"
  echo "  .config/nextest.toml. Put the test in the binary for its harness class:"
  echo "    spawns a process / mixed harness -> tests/e2e/<name>.rs    + 'mod <name>;' in tests/e2e/main.rs"
  echo "    in-process or pure-unit          -> tests/unit/<name>.rs   + 'mod <name>;' in tests/unit/main.rs"
  echo "    #[ignore] stress (bridge)        -> tests/stress/<name>.rs + 'mod <name>;' in tests/stress/main.rs"
  echo "  Declare the mod alphabetically; test names are unchanged, so -E 'test(...)' filters"
  echo "  keep working. A genuinely new harness class needs a new root file here AND a"
  echo "  matching override in .config/nextest.toml."
  exit 1
fi
echo "OK Only the per-harness-class test roots exist under tests/"

echo "=== Guardrail: cargo smoke test scope (ahma_mcp::unit binary) ==="
# A deliberately tiny in-process test so a broken unit binary (a missing `mod`
# line, a dev-dependency dropped from Cargo.toml) fails here rather than on CI.
# The previous incarnation named a test path that did not exist, so `cargo test`
# matched nothing and exited 0 — a silent no-op for its whole life.
#
# Workspace-level, not `-p ahma_mcp`: with the workspace-hack in place a `-p` build
# resolves the same third-party features as the workspace build, and ahma_mcp has
# no features of its own any more (simplify moved to the ahma_simplify crate) —
# but `--workspace` is what CI runs, so it is what the smoke test must exercise.
cargo nextest run --workspace --test unit -E 'test(test_classify_ref_branch_and_release)' --no-fail-fast

echo "=== Guardrail: nextest diagnostics config ==="
if ! grep -q 'success-output = "immediate"' .config/nextest.toml; then
  echo "FAIL .config/nextest.toml missing success-output = \"immediate\""
  exit 1
fi
if ! grep -q 'failure-output = "immediate"' .config/nextest.toml; then
  echo "FAIL .config/nextest.toml missing failure-output = \"immediate\""
  exit 1
fi
echo "OK Nextest diagnostics config looks good"

echo "=== Guardrail: nextest override ordering (narrow before broad) ==="
# nextest resolves each setting from the FIRST matching override in file order,
# not the most specific one. A narrow binary_id() override listed BELOW the
# broad override it refines is silently dead config — this is how the bridge
# stress slow-timeouts stopped applying. The filters are disjoint today, but the
# stress override is kept ABOVE the e2e one so a future broadening of the e2e
# filter (e.g. back to `~ahma_http_bridge::`) cannot swallow it. Assert the
# order holds in every profile that defines both.
for profile in default ci coverage; do
  narrow=$(grep -n "^\[\[profile\.${profile}\.overrides\]\]" -A1 .config/nextest.toml \
    | grep 'binary_id(ahma_http_bridge::stress)' | head -1 | cut -d- -f1)
  broad=$(grep -n "^\[\[profile\.${profile}\.overrides\]\]" -A1 .config/nextest.toml \
    | grep 'binary_id(ahma_http_bridge::e2e)' | head -1 | cut -d- -f1)
  if [ -z "$narrow" ] || [ -z "$broad" ]; then
    echo "FAIL profile.${profile}: expected both a binary_id(ahma_http_bridge::stress) and a binary_id(ahma_http_bridge::e2e) override"
    exit 1
  fi
  if [ "$narrow" -gt "$broad" ]; then
    echo "FAIL profile.${profile}: binary_id(ahma_http_bridge::stress) (line $narrow) must come BEFORE binary_id(ahma_http_bridge::e2e) (line $broad)"
    echo "     nextest takes the first matching override per setting, so the narrow one is dead where it is."
    exit 1
  fi
done
echo "OK Nextest override ordering is narrow-before-broad"

echo "=== Guardrail: subprocess/network suites may retry on CI ==="
# RETRY POLICY (see .config/nextest.toml header): every suite that crosses a
# process or network boundary gets retries on the CI + coverage profiles, so a
# scheduler stall on a cold 2-CPU runner cannot redden main on its own. The
# in-process binaries (ahma_mcp::unit, ahma_http_bridge::unit) must NOT appear
# here — a flake there is a real bug.
for filter in 'binary_id(ahma_http_bridge::e2e)' 'binary_id(ahma_http_bridge::stress)' 'binary_id(ahma_mcp::e2e)' 'binary_id(ahma_tui::connection)'; do
  for profile in ci coverage; do
    if ! awk -v f="$filter" -v p="\\\\[\\\\[profile.${profile}.overrides\\\\]\\\\]" '
      $0 ~ p {inblock=1; hasfilter=0; next}
      /^\[\[/ || /^\[profile/ {inblock=0}
      inblock && index($0, f) {hasfilter=1}
      inblock && hasfilter && /^retries = [1-9]/ {found=1}
      END {exit !found}
    ' .config/nextest.toml; then
      echo "FAIL profile.${profile}: '${filter}' must set retries >= 1 (see RETRY POLICY in .config/nextest.toml)"
      exit 1
    fi
  done
done
echo "OK Subprocess/network suites carry retries on ci and coverage"
for filter in 'binary_id(ahma_mcp::unit)' 'binary_id(ahma_http_bridge::unit)'; do
  if grep -q "filter = \"${filter}\"" .config/nextest.toml; then
    echo "FAIL .config/nextest.toml has an override for '${filter}' — the in-process binaries must stay unthrottled and retry-free (see RETRY POLICY)"
    exit 1
  fi
done
echo "OK In-process binaries carry no override"

echo "=== Guardrail: target directory stale cache auto-clean ==="
cargo xtask clean-stale --max-age-days 3 --max-size-gb 30

echo ""
echo "OK All guardrails passed for phase: $PHASE"

