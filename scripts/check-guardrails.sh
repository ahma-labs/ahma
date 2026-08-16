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
  echo "  ](docs/foo.md)  ->  ](https://github.com/paulirotta/ahma/blob/main/docs/foo.md)"
  exit 1
fi
echo "OK No relative docs/ links in SKILL.md files"

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

echo "=== Guardrail: lint recurring test patterns ===" 
./scripts/lint_test_paths.sh

echo "=== Guardrail: workspace license boundaries ==="
bash ./scripts/check-license-boundaries.sh

echo "=== Guardrail: workspace cargo check ==="
cargo check --workspace --locked

echo "=== Guardrail: cargo smoke test scope (ahma package) ==="
cargo test -p ahma_mcp --test tool_tests tool_execution_integration_test::test_cargo_check_dry_run -- --nocapture

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

echo "=== Guardrail: target directory stale cache auto-clean ==="
cargo xtask clean-stale --max-age-days 3 --max-size-gb 30

echo ""
echo "OK All guardrails passed for phase: $PHASE"

