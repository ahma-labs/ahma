#!/bin/bash
# Guardrail lint script for recurring test anti-patterns.
# Focus: duplicated path resolution, legacy helper drift, timeout regressions.

set -euo pipefail

echo "Checking for improper CARGO_TARGET_DIR usage in tests..."

# Find all Rust test files
VIOLATIONS=0

# Search for CARGO_TARGET_DIR outside of test_utils::cli
while IFS= read -r file; do
    # Skip the allowed files
    if [[ "$file" == *"ahma_mcp/src/test_utils.rs" || "$file" == *"ahma_mcp/tests/e2e/test_utils_coverage_test.rs" ]]; then
        continue
    fi
    
    # Skip files that just remove the env var (that's OK)
    if grep -q 'std::env::var("CARGO_TARGET_DIR")' "$file" && ! grep -q 'env_remove("CARGO_TARGET_DIR")' "$file"; then
        echo "FAIL VIOLATION: $file"
        echo "   Found manual CARGO_TARGET_DIR access"
        echo "   Use ahma_mcp::test_utils::cli::get_binary_path() instead"
        echo ""
        VIOLATIONS=$((VIOLATIONS + 1))
    fi
done < <(find . -path "*/tests/*.rs" -o -name "*_test.rs" -o -name "test_*.rs" | grep -v target)

echo "Checking timeout literals in handshake-critical integration tests..."
for file in \
    "./ahma_http_bridge/tests/e2e/handshake_timeout_test.rs" \
    "./ahma_http_bridge/tests/e2e/http_bridge_integration_test.rs"
do
    if [[ -f "$file" ]] && rg -q 'Duration::from_(secs|millis)\([0-9]+\)' "$file"; then
        echo "FAIL VIOLATION: $file"
        echo "   Found literal Duration timeout value"
        echo "   Use TestTimeouts::get/scale_* categories instead"
        echo ""
        VIOLATIONS=$((VIOLATIONS + 1))
    fi
done

echo "Checking timeout literals in in-src #[cfg(test)] modules..."
# The check above only reaches `tests/*.rs`. Unit tests living in a `#[cfg(test)]`
# module inside `src/` were invisible to it, and that is where the rule actually
# broke: four `ahma_http_bridge::bridge` tests waited a literal 15s for the
# sandbox to settle and timed out on the Windows leg of CI -- in a binary that is
# deliberately denied retries, because an in-process flake is supposed to be a
# real bug.
#
# Scope is deliberately narrow, so the rule stays true rather than becoming noise:
#   * only `timeout(...)` calls -- an actual wall-clock wait, not a config value
#     handed to something under test;
#   * only >= 1s. A sub-second literal is almost always a negative assertion
#     ("nothing arrives within 200ms"), and scaling those by the Windows
#     multiplier would just make the suite slower for no benefit.
# A longer timeout costs nothing on a green run: it bounds how long a FAILURE
# takes to surface, and a passing wait returns the moment its condition is met.
while IFS= read -r file; do
    # `mod tests` inside src/ only; `tests/` files are covered above.
    grep -q '^#\[cfg(test)\]' "$file" || continue
    # `-U` (multiline) is load-bearing: the common shape puts the duration on
    # the line after `timeout(`, and a line-based match sees neither half.
    # The leading boundary keeps `with_timeout(Duration::from_secs(3600))` out of
    # scope: that is a configuration value handed to the code under test, not a
    # wait, and scaling it would change what the test exercises.
    # Only the test module, not the whole file: production code may legitimately
    # hold a literal timeout (it is configuration, not a test's patience).
    # Test modules sit at the end of the file by convention, so take everything
    # from the first `#[cfg(test)]` onward.
    if awk '/^#\[cfg\(test\)\]/{f=1} f' "$file" \
        | rg -qU '(^|[^A-Za-z0-9_])timeout\(\s*(std::time::)?Duration::from_secs\([1-9][0-9]*\)'; then
        echo "FAIL VIOLATION: $file"
        echo "   Literal >=1s Duration passed to timeout() in an in-src test module"
        echo "   Use ahma_common::timeouts::TestTimeouts::get(TimeoutCategory::_)"
        echo "   for a semantic wait, or ::scale_secs(n) to keep the current value"
        echo "   scaled for Windows (4x) and coverage (2x more)."
        echo ""
        VIOLATIONS=$((VIOLATIONS + 1))
    fi
done < <(find . -path "*/src/*.rs" | grep -v target)

echo "Checking shared custom server spawn usage in HTTP bridge integration test..."
HTTP_BRIDGE_TEST="./ahma_http_bridge/tests/e2e/http_bridge_integration_test.rs"
if [[ -f "$HTTP_BRIDGE_TEST" ]] && ! rg -q 'spawn_server_guard_with_config' "$HTTP_BRIDGE_TEST"; then
    echo "FAIL VIOLATION: $HTTP_BRIDGE_TEST"
    echo "   Missing shared custom server startup helper usage"
    echo "   Use tests/common/server::spawn_server_guard_with_config(...)"
    echo ""
    VIOLATIONS=$((VIOLATIONS + 1))
fi

if [ $VIOLATIONS -eq 0 ]; then
    echo "OK No violations found"
    exit 0
else
    echo ""
    echo "FAIL Found $VIOLATIONS violation(s)"
    echo ""
    echo "Fix: Replace manual CARGO_TARGET_DIR logic with:"
    echo "  - ahma_mcp::test_utils::cli::get_binary_path(package, binary)"
    echo "  - ahma_mcp::test_utils::cli::build_binary_cached(package, binary)"
    exit 1
fi
