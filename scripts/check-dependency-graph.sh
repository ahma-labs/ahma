#!/bin/bash
# Dependency-graph invariants. No compile: `cargo hakari verify` and `cargo tree` read
# Cargo.lock only, so this runs in seconds and belongs in every gate (pre-push via
# check-guardrails.sh, and Fast Tier on every PR).
#
# 1. workspace-hack/ is in sync with the real dependency graph. It pins one third-party
#    feature set for every cargo invocation so `-p <crate>` builds reuse the workspace
#    build's artifacts — but only while it matches the graph, and it never updates itself.
# 2. Exactly one rustls crypto provider. `aws-lc-rs` is mandatory (rustls' default, and the
#    only provider sigstore/tough accept), so every `ring` edge is a *second* provider:
#    another native build script, another copy of every TLS primitive in every binary,
#    and a `rustls::crypto` ambiguity to resolve by hand. `ring` only ever entered through
#    defaults ahma chose (`rcgen`, `quinn`), so it is kept out by feature selection, and
#    this is what notices when a new dependency's defaults bring it back.
#    Scope: the product graph (`default-members`, i.e. everything but `xtask`) on the
#    platforms CI builds — the same list as `.config/hakari.toml`. Not `--target all`:
#    quinn-proto has an unconditional wasm-only `ring` edge, which is also why `ring`
#    still appears in Cargo.lock (the lock records every target's edges — grep it and you
#    learn nothing; ask the resolver). Not `--workspace`: `xtask`'s `ureq` uses
#    `rustls/ring`, and moving that dev-only tool to aws-lc-rs would cost a second,
#    differently-unified `aws-lc-sys` build (it is excluded from hakari on purpose).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

echo "=== Dependency graph: workspace-hack is in sync (cargo hakari verify) ==="
if ! command -v cargo-hakari > /dev/null 2>&1; then
  echo "FAIL cargo-hakari is not installed. Install with: cargo install cargo-hakari --locked"
  exit 1
fi
if ! cargo hakari verify; then
  echo ""
  echo "FAIL workspace-hack/ is out of date with the workspace dependency graph."
  echo "  Regenerate it (and re-add the workspace-hack dependency to any new crate):"
  echo "    cargo hakari generate && cargo hakari manage-deps"
  echo "  then commit the resulting workspace-hack/Cargo.toml and Cargo.toml changes."
  exit 1
fi
echo "OK workspace-hack is in sync"

echo "=== Dependency graph: exactly one rustls crypto provider (aws-lc-rs, never ring) ==="
# Keep in step with `platforms` in .config/hakari.toml.
PLATFORMS=(
  aarch64-apple-darwin
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-pc-windows-msvc
)
tree() {
  local crate="$1" platform="$2"
  cargo tree --locked --target "$platform" -e normal,build --prefix none -i "$crate" 2>/dev/null || true
}
for platform in "${PLATFORMS[@]}"; do
  if ! tree aws-lc-rs "$platform" | grep -q '^aws-lc-rs '; then
    echo "FAIL aws-lc-rs is no longer in the compile graph for $platform; it is the workspace's rustls crypto provider."
    exit 1
  fi
  ring_path="$(tree ring "$platform")"
  if [[ -n "$ring_path" ]]; then
    echo "FAIL \`ring\` is back in the compile graph for $platform: a dependency's default features"
    echo "  enable a second rustls crypto provider. Find the edge with:"
    echo "    cargo tree --target $platform -e features -i ring"
    echo "  and disable that feature (see the quinn/rcgen notes in ahma_http_bridge/Cargo.toml)."
    echo "  Path:"
    while IFS= read -r line; do echo "    $line"; done <<< "$ring_path"
    exit 1
  fi
done
echo "OK single crypto provider (aws-lc-rs) on ${#PLATFORMS[@]} platforms"
