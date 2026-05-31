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

# Public key for release verification (RSA-2048)
PUB_KEY_PEM="-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA5veFxEchlM3iyFx8BQzs
f+yn6ZNJygRwfOfLS901Rxm/I3YRwn2Jksyp2bVckjgDeGJVK7IPGaHe1dL7+Ljn
5V3zvU9B7CLeeIGdZRRngV/n6r+dsGy0FWQIcN/+dfKPWvhz4m/4QMTLXL05WK8j
iI/Qatp2Fs32CUJTJ6NpIDQZi4xd1xhQbF/jk2+pwgwpup7kAVKPa49QegFQEQcS
i8duqBKX2ynTA6QhknBX1fY+6vEFLh6uMePjzGyHLax8mMg8sk2WU59bgMGgtPPy
le7gp692r3UaP9YgzuNDTyDSoU4gJmOOYYAtMWkNOyD2Bcr8JndwPXG0CD3Hj+j0
GwIDAQAB
-----END PUBLIC KEY-----"

# Parse CLI arguments
VERIFY_ONLY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --verify|-v)
            VERIFY_ONLY=1
            shift
            ;;
        *)
            echo "Error: Unknown argument: $1"
            echo "Usage: $0 [--verify]"
            exit 1
            ;;
    esac
done

# Detect OS and Architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
LIBC=""

# Helper function to compute SHA256 of a file in a portable way
compute_sha256() {
    local filepath="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$filepath" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$filepath" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$filepath" | awk '{print $NF}'
    else
        echo "Error: No sha256 program found (sha256sum, shasum, or openssl required)." >&2
        exit 1
    fi
}

# Verify the signature of a data file using a signature file and the public key
verify_signature() {
    local data_file="$1"
    local sig_file="$2"
    local temp_dir="$3"
    
    echo "$PUB_KEY_PEM" > "$temp_dir/pubkey.pem"
    
    if ! command -v openssl >/dev/null 2>&1; then
        echo "Error: openssl is required for cryptographic verification." >&2
        exit 1
    fi
    
    if ! openssl dgst -sha256 -verify "$temp_dir/pubkey.pem" -signature "$sig_file" "$data_file" >/dev/null 2>&1; then
        echo "########################################################################" >&2
        echo "CRITICAL SECURITY ERROR: Release signature verification FAILED!" >&2
        echo "The checksums file '$data_file' is NOT signed by the official private key." >&2
        echo "This release might be compromised or tampered with." >&2
        echo "########################################################################" >&2
        exit 1
    fi
    echo "Authenticity verified: Release signature is valid."
}

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

# ── Verification-Only Mode ───────────────────────────────────────────────────
if [ "$VERIFY_ONLY" = "1" ]; then
    echo "Checking installed binary signature..."
    EXISTING_BIN=""
    if command -v ahma >/dev/null 2>&1; then
        EXISTING_BIN="$(command -v ahma)"
    elif [ -x "$INSTALL_DIR/ahma" ]; then
        EXISTING_BIN="$INSTALL_DIR/ahma"
    fi
    
    if [ -z "$EXISTING_BIN" ]; then
        echo "Error: ahma is not currently installed or not in PATH." >&2
        exit 1
    fi
    
    echo "Found binary at: $EXISTING_BIN"
    local_hash=$(compute_sha256 "$EXISTING_BIN")
    echo "Local SHA-256: $local_hash"
    
    echo "Fetching latest release info..."
    fetch_release_json
    
    SUMS_URL=$(echo "$RELEASE_JSON" | grep "browser_download_url" | grep "SHA256SUMS" | grep -v "\.sig" | head -n 1 | cut -d '"' -f 4 || true)
    SIG_URL=$(echo "$RELEASE_JSON" | grep "browser_download_url" | grep "SHA256SUMS.sig" | head -n 1 | cut -d '"' -f 4 || true)
    
    if [ -z "$SUMS_URL" ] || [ -z "$SIG_URL" ]; then
        echo "Error: Could not find SHA256SUMS or SHA256SUMS.sig in the latest release." >&2
        exit 1
    fi
    
    TEMP_DIR=$(mktemp -d)
    trap 'rm -rf "$TEMP_DIR"' EXIT
    
    echo "Downloading release manifest and signature..."
    if command -v curl >/dev/null 2>&1; then
        curl -sSfL "$SUMS_URL" -o "$TEMP_DIR/SHA256SUMS"
        curl -sSfL "$SIG_URL" -o "$TEMP_DIR/SHA256SUMS.sig"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$TEMP_DIR/SHA256SUMS" "$SUMS_URL"
        wget -qO "$TEMP_DIR/SHA256SUMS.sig" "$SIG_URL"
    fi
    
    # Verify signature
    verify_signature "$TEMP_DIR/SHA256SUMS" "$TEMP_DIR/SHA256SUMS.sig" "$TEMP_DIR"
    
    # Find match in SHA256SUMS
    if grep -E "^${local_hash}[[:space:]]" "$TEMP_DIR/SHA256SUMS" >/dev/null; then
        matching_entry=$(grep -E "^${local_hash}[[:space:]]" "$TEMP_DIR/SHA256SUMS")
        echo "Success: Installed binary matches a verified release entry!"
        echo "Verified: $matching_entry"
        exit 0
    else
        echo "########################################################################" >&2
        echo "SECURITY WARNING: Local binary verification FAILED!" >&2
        echo "The local hash '$local_hash' does not match any entry in the verified release manifest." >&2
        echo "The binary may have been modified or is a different/unreleased version." >&2
        echo "########################################################################" >&2
        exit 1
    fi
fi

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
SUMS_URL=$(echo "$RELEASE_JSON" | grep "browser_download_url" | grep "SHA256SUMS" | grep -v "\.sig" | head -n 1 | cut -d '"' -f 4 || true)
SIG_URL=$(echo "$RELEASE_JSON" | grep "browser_download_url" | grep "SHA256SUMS.sig" | head -n 1 | cut -d '"' -f 4 || true)

if [ -z "$DOWNLOAD_URL" ]; then
    echo "Error: Could not find release asset '$ASSET_NAME'."
    echo "Please check https://github.com/paulirotta/ahma/releases for available binaries."
    exit 1
fi

if [ -z "$SUMS_URL" ] || [ -z "$SIG_URL" ]; then
    echo "Error: Could not find release manifest (SHA256SUMS) or signature (SHA256SUMS.sig)."
    exit 1
fi

# create temporary directory
TEMP_DIR=$(mktemp -d)
trap 'rm -rf "$TEMP_DIR"' EXIT

echo "Downloading release manifest and signature..."
if command -v curl >/dev/null 2>&1; then
    curl -sSfL "$SUMS_URL" -o "$TEMP_DIR/SHA256SUMS"
    curl -sSfL "$SIG_URL" -o "$TEMP_DIR/SHA256SUMS.sig"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TEMP_DIR/SHA256SUMS" "$SUMS_URL"
    wget -qO "$TEMP_DIR/SHA256SUMS.sig" "$SIG_URL"
fi

# Verify signature
verify_signature "$TEMP_DIR/SHA256SUMS" "$TEMP_DIR/SHA256SUMS.sig" "$TEMP_DIR"

# Extract expected hash
EXPECTED_HASH=$(grep -E "[[:space:]]${ASSET_NAME}$" "$TEMP_DIR/SHA256SUMS" | awk '{print $1}')
if [ -z "$EXPECTED_HASH" ]; then
    echo "Error: Checksum entry for '$ASSET_NAME' not found in release manifest."
    exit 1
fi

echo "Downloading ${DOWNLOAD_URL}..."
# Download archive
if command -v curl >/dev/null 2>&1; then
    curl -sSfL "$DOWNLOAD_URL" -o "$TEMP_DIR/$ASSET_NAME"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TEMP_DIR/$ASSET_NAME" "$DOWNLOAD_URL"
fi

# Verify archive hash
ACTUAL_HASH=$(compute_sha256 "$TEMP_DIR/$ASSET_NAME")
if [ "$EXPECTED_HASH" != "$ACTUAL_HASH" ]; then
    echo "########################################################################" >&2
    echo "CRITICAL SECURITY ERROR: Archive integrity check failed!" >&2
    echo "Checksum mismatch for $ASSET_NAME." >&2
    echo "Expected: $EXPECTED_HASH" >&2
    echo "Actual:   $ACTUAL_HASH" >&2
    echo "########################################################################" >&2
    exit 1
fi
echo "Integrity verified: Archive hash matches release manifest."

# Extract
tar -xzf "$TEMP_DIR/$ASSET_NAME" -C "$TEMP_DIR"

# Install binaries
echo "Installing binaries to ${INSTALL_DIR}..."
if [ -f "$TEMP_DIR/ahma" ]; then
    mv "$TEMP_DIR/ahma" "$INSTALL_DIR/"
    chmod +x "$INSTALL_DIR/ahma"
else
    echo "Error: ahma binary not found in archive"
    exit 1
fi

# Verify the installed binary hash and print it
INSTALLED_BIN="$INSTALL_DIR/ahma"
INSTALLED_HASH=$(compute_sha256 "$INSTALLED_BIN")
echo "Installed binary hash: $INSTALLED_HASH"

"$INSTALLED_BIN" --version
echo "Success! Installed ahma to ${INSTALL_DIR}"
AHMA_BIN="$INSTALLED_BIN"

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
