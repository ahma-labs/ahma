#!/usr/bin/env bash
# Guardrails for the `ahma gui` page (SPEC R-GUI.2, R-GUI.5, R-GUI.10).
#
# Static, needs no Node: a grep over the TypeScript source and a look at the lockfile.
# (The committed-build check, which does need Node, is the `web` job in build.yml.)
set -euo pipefail

cd "$(dirname "$0")/.."
web=ahma_gui/web
fail=0

# R-GUI.5: nothing from outside becomes markup or code. R-GUI.2: the token is held in
# memory only. A match in a comment is still a match; reword the comment.
forbidden='innerHTML|outerHTML|insertAdjacentHTML|document\.write|createContextualFragment|DOMParser|srcdoc|\beval\(|new Function|localStorage|sessionStorage|indexedDB|document\.cookie|import\('
if hits=$(grep -rnE "$forbidden" "$web/src" 2>/dev/null); then
  echo "FAIL forbidden API in the GUI page source (SPEC R-GUI.2, R-GUI.5):"
  echo "$hits"
  fail=1
fi

# R-GUI.10: first-party modules only. Every import is relative.
if hits=$(grep -rnE "^\s*(import|export)\b.*\bfrom\s+['\"][^./]" "$web/src" 2>/dev/null); then
  echo "FAIL the page imports something that is not a relative first-party module (SPEC R-GUI.10):"
  echo "$hits"
  fail=1
fi

# R-GUI.10: `typescript` is the one package in the lockfile, and it has no dependencies.
packages=$(grep -cE '^\s+"node_modules/' "$web/package-lock.json" || true)
if [ "$packages" != "1" ]; then
  echo "FAIL $web/package-lock.json lists $packages packages; SPEC R-GUI.10 allows exactly one (typescript)"
  fail=1
fi
if grep -qE '"dependencies"' "$web/package.json"; then
  echo "FAIL $web/package.json has runtime \"dependencies\"; the page has none (SPEC R-GUI.10)"
  fail=1
fi

# The compiled files are what ships; the same rules apply to them.
if hits=$(grep -rnE "$forbidden" "$web/dist" 2>/dev/null); then
  echo "FAIL forbidden API in the compiled page ($web/dist):"
  echo "$hits"
  fail=1
fi

[ "$fail" = 0 ] && echo "OK GUI page: no forbidden API, first-party imports only, one lockfile package"
exit $fail
