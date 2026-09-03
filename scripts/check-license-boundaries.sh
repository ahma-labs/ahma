#!/bin/bash
# Guardrail: permissive first-party crates must not gain normal/build
# dependencies on AGPL-licensed first-party crates.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

PERMISSIVE_CRATES=(
  ahma_mcp
  ahma_core
  ahma_common
  ahma_http_bridge
  ahma_http_mcp_client
  ahma_llm_monitor
  ahma_bundle
  ahma_update
  ahma_vault
  ahma_log_monitor
  ahma_harness_guard
  ahma_output_optimizer
  ahma_simplify
  ahma_test_support
  ahma_harness_tools
  generate_tool_schema
)

AGPL_CRATES=(
  ahma_bin
  ahma_tui
)

agpl_pattern="^($(IFS='|'; echo "${AGPL_CRATES[*]}")) v"
violations=0

echo "🔍 Checking workspace license boundaries..."

audit_crate() {
  local crate="$1"
  local matches

  matches="$({ cargo tree -p "$crate" --edges normal,build --prefix none --format '{p}' \
    | grep -E "$agpl_pattern" \
    | sort -u; } || true)"

  if [[ -n "$matches" ]]; then
    echo "FAIL $crate depends on AGPL-licensed workspace crate(s):"
    while IFS= read -r line; do
      [[ -n "$line" ]] && echo "  - $line"
    done <<< "$matches"
    echo ""
    violations=$((violations + 1))
  else
    echo "OK $crate"
  fi
}

for crate in "${PERMISSIVE_CRATES[@]}"; do
  audit_crate "$crate"
done

if [[ "$violations" -ne 0 ]]; then
  echo ""
  echo "FAIL Found $violations workspace license boundary violation(s)."
  echo "Permissive crates must not take normal/build dependencies on AGPL workspace crates."
  exit 1
fi

echo ""
echo "OK Workspace license boundaries intact"
