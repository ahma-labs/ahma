#!/bin/bash
# Bump workspace version across all files that must stay in sync.
#
# Usage:
#   ./scripts/bump-version.sh 0.7.0
#
# Updates:
#   - Cargo.toml                  [workspace.package] version = "X.Y.Z"
#   - skills/ahma/SKILL.md        version: X.Y.Z (YAML frontmatter and HTML comment)
#
# After updating, runs the version-consistency test to verify.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

NEW_VER="${1:-}"
if [[ -z "$NEW_VER" ]]; then
  echo "Usage: $0 <new-version>  (e.g. $0 0.7.0)"
  exit 1
fi

# Validate semver format (X.Y.Z)
if ! [[ "$NEW_VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "FAIL Version '$NEW_VER' is not a valid semver (expected X.Y.Z)"
  exit 1
fi

# Extract current version from Cargo.toml
CUR_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')

if [[ "$CUR_VER" == "$NEW_VER" ]]; then
  echo "Already at version $NEW_VER — nothing to do."
  exit 0
fi

echo "Bumping $CUR_VER → $NEW_VER"
echo ""

# Helper: in-place replace with perl (works on both macOS and Linux)
replace() {
  local pattern="$1"
  local file="$2"
  perl -i -pe "$pattern" "$file"
}

# 1. Cargo.toml: version = "X.Y.Z"
replace "s|^(version = \")${CUR_VER}\"|\${1}${NEW_VER}\"|" Cargo.toml
echo "  OK Cargo.toml"

# 2. skills/ahma/SKILL.md: version: X.Y.Z and HTML comment
replace "s|^(version: )${CUR_VER}|\${1}${NEW_VER}|" skills/ahma/SKILL.md
replace "s|(<!-- version: )${CUR_VER}( \|)|\${1}${NEW_VER}\${2}|" skills/ahma/SKILL.md
echo "  OK skills/ahma/SKILL.md"

echo ""
echo "Verifying with nextest..."
cargo nextest run -E 'test(skill_versions)'
echo ""
echo "Done. Version bumped to $NEW_VER."
echo "Suggested commit:"
echo "  git add Cargo.toml skills/ahma/SKILL.md"
echo "  git commit -m \"chore(release): bump version to $NEW_VER\""
