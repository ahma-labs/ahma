#!/bin/bash
# Every requirement id cited from code must be written down in a spec.
#
# Code cites SPEC requirement ids (`R5.4`, `R-DAEMON.2`, …) in comments and test
# docs so a reader can find the rule a line obeys. When a SPEC section is edited,
# moved to a crate SPEC, or renumbered, those citations silently dangle and the
# rule they pointed at is lost. This check fails when a cited id appears in no
# `SPEC.md` (root or crate) and not in `AGENTS.md` (which owns the testing
# rules).
#
# Usage: ./scripts/check-spec-ids.sh
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

ID_RE='\bR(-[A-Z]+)?[0-9]+(\.[0-9]+)*\b|\bR-[A-Z]+(\.[0-9]+)*\b'

cited=$(git ls-files '*.rs' '*.sh' '*.toml' '*.yml' '*.yaml' \
  | xargs grep -hoE "$ID_RE" 2>/dev/null | sort -u || true)
defined=$(git ls-files '*SPEC.md' AGENTS.md \
  | xargs grep -hoE "$ID_RE" 2>/dev/null | sort -u || true)

missing=$(comm -23 <(printf '%s\n' "$cited") <(printf '%s\n' "$defined") | sed '/^$/d')

if [[ -n "$missing" ]]; then
  echo "FAIL Requirement ids cited in code but written in no SPEC.md or AGENTS.md:"
  while IFS= read -r id; do
    where=$(git ls-files '*.rs' '*.sh' '*.toml' '*.yml' '*.yaml' \
      | xargs grep -lwF "$id" 2>/dev/null | head -3 | tr '\n' ' ')
    echo "  $id  ($where)"
  done <<< "$missing"
  echo ""
  echo "Define the id in the SPEC.md that owns the rule, or correct the citation."
  exit 1
fi
echo "OK Every cited requirement id is defined in a spec"
