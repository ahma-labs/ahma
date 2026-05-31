#!/bin/bash
# One-liner installer for ahma
# Usage: curl -sSf https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.sh | bash
#
# Supported platforms:
#   - Linux x86_64 (glibc and musl)
#   - Linux ARM64/aarch64 (glibc and musl)
#   - Linux ARMv7 (Raspberry Pi 2/3)
#   - macOS ARM64 (Apple Silicon)
#
# Environment variables:
#   AHMA_PREFER_MUSL=1    - Force musl binary on Linux (more portable, no glibc dependency)

set -euo pipefail

# Skill version — keep in sync with [workspace.package] version in Cargo.toml.
# CI guardrails verify this matches. Bump via: cargo xtask bump-version X.Y.Z
AHMA_VERSION="0.8.0"

# Detect OS and Architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
LIBC=""

# Detect libc type on Linux
detect_libc() {
    if [ "$OS" != "linux" ]; then
        return
    fi
    
    # Check if we're on a musl-based system (Alpine, Void, etc.)
    if command -v ldd >/dev/null 2>&1; then
        if ldd --version 2>&1 | grep -qi musl; then
            LIBC="musl"
            return
        fi
    fi
    
    # Check for Alpine specifically
    if [ -f /etc/alpine-release ]; then
        LIBC="musl"
        return
    fi
    
    # Default to glibc
    LIBC="glibc"
}

# Map architecture names
case "$ARCH" in
    x86_64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="arm64" ;;
    armv7l|armv7) ARCH="armv7" ;;
    *)
        echo "Error: Unsupported architecture: $ARCH"
        echo "Supported: x86_64, arm64/aarch64, armv7"
        exit 1
        ;;
esac

# Map OS names and validate combinations
case "$OS" in
    linux)
        detect_libc
        ;;
    darwin)
        if [ "$ARCH" = "x86_64" ]; then
            echo "Error: macOS Intel (x86_64) is no longer supported. Prebuilt binaries are only available for Apple Silicon (arm64)."
            echo "You can still build from source: cargo build --release"
            exit 1
        fi
        ;;
    *)
        echo "Error: Unsupported operating system: $OS"
        echo "Supported: linux, darwin (macOS)"
        exit 1
        ;;
esac

# Construct platform identifier
# Format: {os}-{arch}[-musl]
if [ "$OS" = "linux" ]; then
    # Use musl if detected or explicitly requested
    if [ "${AHMA_PREFER_MUSL:-}" = "1" ] || [ "$LIBC" = "musl" ]; then
        if [ "$ARCH" = "armv7" ]; then
            # armv7 only has glibc build
            PLATFORM="linux-armv7"
            echo "Note: ARMv7 only has glibc build available"
        else
            PLATFORM="linux-${ARCH}-musl"
        fi
    else
        if [ "$ARCH" = "armv7" ]; then
            PLATFORM="linux-armv7"
        else
            PLATFORM="linux-${ARCH}"
        fi
    fi
else
    PLATFORM="${OS}-${ARCH}"
fi

INSTALL_DIR="$HOME/.local/bin"
RELEASE_JSON=""

# Fetch latest release data from GitHub (cached in RELEASE_JSON to avoid duplicate calls)
fetch_release_json() {
    if [ -n "$RELEASE_JSON" ]; then
        return
    fi
    RELEASES_URL="https://api.github.com/repos/paulirotta/ahma/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        RELEASE_JSON=$(curl -s "$RELEASES_URL")
    elif command -v wget >/dev/null 2>&1; then
        RELEASE_JSON=$(wget -qO- "$RELEASES_URL")
    else
        echo "Error: Neither curl nor wget is available."
        exit 1
    fi
}

# Check for existing installation and compare versions
EXISTING_BIN=""
if command -v ahma >/dev/null 2>&1; then
    EXISTING_BIN="$(command -v ahma)"
elif [ -x "$INSTALL_DIR/ahma" ]; then
    EXISTING_BIN="$INSTALL_DIR/ahma"
fi

if [ -n "$EXISTING_BIN" ]; then
    INSTALLED_VER=$("$EXISTING_BIN" --version 2>&1 | awk '{print $2}' || true)

    echo "Fetching latest release info..."
    fetch_release_json
    LATEST_VER=$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | cut -d'"' -f4 | sed 's/^v//')

    if [ "$INSTALLED_VER" != "$LATEST_VER" ] && [ -n "$LATEST_VER" ]; then
        echo "Upgrading ahma from ${INSTALLED_VER} to ${LATEST_VER}..."
    else
        echo "Ahma ${INSTALLED_VER} is already installed and up to date."
        echo ""
        echo "  Location : $EXISTING_BIN"
        echo "  Simplify : available via 'ahma simplify --help'"
        echo ""
        if [ -e /dev/tty ]; then
            printf "Reinstall anyway? [y/N]: "
            IFS= read -r CONFIRM < /dev/tty
            case "$CONFIRM" in
                [Yy]*) echo "Reinstalling..." ;;
                *) echo "No changes made."; exit 0 ;;
            esac
        else
            echo "No changes made (non-interactive — same version already installed)."
            exit 0
        fi
    fi
else
    echo "Fetching latest release info..."
    fetch_release_json
fi

echo "Installing Ahma for ${PLATFORM}..."

# Create install directory
mkdir -p "$INSTALL_DIR"

# Extract download URL for the platform-specific tarball
# Expected asset name format: ahma-release-{platform}.tar.gz
ASSET_NAME="ahma-release-${PLATFORM}.tar.gz"

# Use grep/cut to parse JSON (avoiding jq dependency for maximum portability)
DOWNLOAD_URL=$(echo "$RELEASE_JSON" | grep "browser_download_url" | grep "$ASSET_NAME" | cut -d '"' -f 4 || true)

if [ -z "$DOWNLOAD_URL" ]; then
    echo "Error: Could not find release asset '$ASSET_NAME'."
    echo "Please check https://github.com/paulirotta/ahma/releases for available binaries."
    exit 1
fi

echo "Downloading ${DOWNLOAD_URL}..."

# create temporary directory
TEMP_DIR=$(mktemp -d)
trap 'rm -rf "$TEMP_DIR"' EXIT

# Download and extract
if command -v curl >/dev/null 2>&1; then
    curl -sL "$DOWNLOAD_URL" | tar -xz -C "$TEMP_DIR"
elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$DOWNLOAD_URL" | tar -xz -C "$TEMP_DIR"
fi

# Install binaries
echo "Installing binaries to ${INSTALL_DIR}..."
if [ -f "$TEMP_DIR/ahma" ]; then
    mv "$TEMP_DIR/ahma" "$INSTALL_DIR/"
    chmod +x "$INSTALL_DIR/ahma"
else
    echo "Error: ahma binary not found in archive"
    exit 1
fi

"$INSTALL_DIR/ahma" --version
echo "Success! Installed ahma to ${INSTALL_DIR}"
AHMA_BIN="$INSTALL_DIR/ahma"

# Remove legacy ahma-simplify binary if present
for legacy_bin in "$INSTALL_DIR/ahma-simplify" "$HOME/.local/bin/ahma-simplify" "/usr/local/bin/ahma-simplify"; do
    if [ -x "$legacy_bin" ]; then
        rm -f "$legacy_bin"
        echo "Removed legacy binary: $legacy_bin"
        echo "  Code complexity analysis is now built into ahma."
        echo "  New command: ahma simplify <directory> --ai-fix 1"
    fi
done
echo ""
echo "Please ensure ${INSTALL_DIR} is in your PATH:"
echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
echo ""

# Run the Rust-based setup wizard
if [ -e /dev/tty ] && [ -t 1 ]; then
    "$AHMA_BIN" setup < /dev/tty
else
    echo "Non-interactive shell detected. Running auto-setup..."
    "$AHMA_BIN" setup --auto
fi
