#!/bin/bash
# Pre-push guardrail to prevent CI-only workspace failures.
#
# Install:
#   cp scripts/pre-push.sh .git/hooks/pre-push && chmod +x .git/hooks/pre-push

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

echo "Running pre-push checks..."

DIRTY_STATUS="$(git status --porcelain)"
if [ -n "$DIRTY_STATUS" ]; then
  echo "FAIL Refusing push: working tree is not clean."
  echo ""
  echo "Detected changes:"
  echo "$DIRTY_STATUS"
  echo ""
  echo "Commit/stash/remove local changes (including untracked files) and retry."
  exit 1
fi

echo "OK Working tree is clean"

echo "=== Guardrail: version consistency ==="
CARGO_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
AHMA_SKILL_VER=$(grep '^version:' skills/ahma/SKILL.md | head -1 | awk '{print $2}')
SKILL_HTML_VER=$(grep '<!-- version:' skills/ahma/SKILL.md | head -1 | sed -E 's/.*version: ([0-9]+\.[0-9]+\.[0-9]+).*/\1/')

VER_FAIL=0
if [ "$AHMA_SKILL_VER" != "$CARGO_VER" ]; then
  echo "FAIL skills/ahma/SKILL.md version: ${AHMA_SKILL_VER} != Cargo.toml ${CARGO_VER}"
  VER_FAIL=1
fi
if [ "$SKILL_HTML_VER" != "$CARGO_VER" ]; then
  echo "FAIL skills/ahma/SKILL.md HTML comment version: ${SKILL_HTML_VER} != Cargo.toml ${CARGO_VER}"
  VER_FAIL=1
fi
if [ "$VER_FAIL" -ne 0 ]; then
  echo ""
  echo "  Run: cargo xtask bump-version ${CARGO_VER}"
  echo "  to sync skill version strings to the Cargo.toml value."
  exit 1
fi
echo "OK Version strings consistent (v${CARGO_VER})"

echo "=== Running cargo check --workspace --locked ==="
cargo check --workspace --locked

echo "OK Pre-push checks passed"
