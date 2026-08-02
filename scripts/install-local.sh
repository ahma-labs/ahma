#!/bin/bash
# Local fast install script for ahma.
# Builds ahma_bin in dev (non-release) mode by default, reusing the local workspace
# target/ directory and atomically installs it to ~/.local/bin/ahma (or $AHMA_INSTALL_DIR).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

INSTALL_DIR="${AHMA_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"

PROFILE="debug"
CARGO_CMD=("cargo" "build")

for arg in "$@"; do
    if [ "$arg" = "--release" ]; then
        PROFILE="release"
        CARGO_CMD+=("--release")
    fi
done
CARGO_CMD+=("-p" "ahma_bin")

if [ "$PROFILE" = "release" ]; then
    echo "Building ahma_bin (release mode)..."
else
    echo "⚠️  Building ahma_bin (DEV / DEBUG mode - NOT a release build)..."
    echo "   Reusing local workspace target/debug directory for fast installation."
fi

"${CARGO_CMD[@]}"

TARGET_BIN="$PROJECT_ROOT/target/$PROFILE/ahma"
DEST_BIN="$INSTALL_DIR/ahma"
TEMP_BIN="$INSTALL_DIR/ahma.tmp.$$"

echo "Installing to $DEST_BIN..."
cp "$TARGET_BIN" "$TEMP_BIN"
chmod +x "$TEMP_BIN"
mv -f "$TEMP_BIN" "$DEST_BIN"

if [ "$(uname -s)" = "Darwin" ]; then
    codesign --force --sign - --options runtime "$DEST_BIN" 2>/dev/null || true
fi

if [ "$PROFILE" = "debug" ]; then
    echo "⚠️  NOTE: Installed build is a DEV/DEBUG build (NOT a release build)."
    echo "Successfully installed local dev build of ahma:"
else
    echo "Successfully installed local release build of ahma:"
fi
"$DEST_BIN" --version
