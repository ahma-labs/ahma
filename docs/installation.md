# Installation

## Update (recommended)

If `ahma` is already installed:

```bash
ahma update                    # latest published release
ahma update 0.6.7              # specific release tag
ahma update main               # build from Git branch
ahma update feature/my-branch  # build from feature branch
ahma update --install-hooks    # also install user-scoped terminal hooks
```

Use `--force` to reinstall when the version already matches. Use `--dry-run` to preview actions.
When run interactively, `ahma update` now offers user-scoped terminal hook installation if none are currently managed.

Custom install location: `--install-dir ~/.local/bin` or `AHMA_INSTALL_DIR`.

## First-time install

You need `ahma` on PATH before `ahma update` works.

### Linux / macOS

**Latest (build from GitHub main via Cargo — requires [Rust](https://rustup.rs/)):**

```bash
cargo install --git https://github.com/paulirotta/ahma ahma_mcp --bin ahma --root ~/.local --locked --force
```

**Specific branch:**

```bash
cargo install --git https://github.com/paulirotta/ahma --branch feature/update ahma_mcp --bin ahma --root ~/.local --locked --force
```

**Unpushed local checkout:**

```bash
cargo install --path ahma_mcp --bin ahma --root ~/.local --locked --force
```

Ensure `~/.local/bin` is on your PATH:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### Windows (PowerShell 5.1+)

**Latest release (prebuilt binary):**

```powershell
irm https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.ps1 | iex
```

**Specific branch (builds via Cargo — requires Rust):**

```powershell
# Save script locally, then:
.\install.ps1 feature/update
```

Or invoke Cargo directly:

```powershell
cargo install --git https://github.com/paulirotta/ahma --branch feature/update ahma_mcp --bin ahma --root $HOME\.local --locked --force
```

Ensure `$HOME\.local\bin` is on your PATH.

## Build from source (full checkout)

**Linux / macOS**

```bash
git clone https://github.com/paulirotta/ahma.git
cd ahma
cargo build --release -p ahma_mcp
mv target/release/ahma ~/.local/bin/
```

**Windows (PowerShell)**

```powershell
git clone https://github.com/paulirotta/ahma.git
cd ahma
cargo build --release -p ahma_mcp
Copy-Item target\release\ahma.exe "$HOME\.local\bin\"
```

## After installation

- The install script now offers optional user-scoped terminal hook setup for Cursor, Claude Code, and Codex.
- Configure your MCP client — see [connection-modes.md](connection-modes.md).
- Optional terminal hooks for Cursor, Claude Code, and Codex:
	- `ahma hooks install` installs user-scoped managed hooks using the current binary path.
	- `ahma hooks install --scope project` writes portable project hooks that call `ahma` from `PATH`.
	- `ahma hooks status` shows both user and project hook status.
	- `ahma hooks uninstall` removes managed hooks again if you no longer want shell-tool wrapping.
- Optional agent skill — see [agent-skills.md](agent-skills.md).
- Restart MCP clients or reload your IDE after updating the binary.

## Platform notes

Supported prebuilt release platforms: Linux x86_64/arm64/armv7, macOS Apple Silicon, Windows x86_64. Musl builds are available for Linux x86_64 and ARM64 (`AHMA_PREFER_MUSL=1` during platform detection in `ahma update`).

For sandbox behavior and day-to-day usage, see [README.md](../README.md) and [security-sandbox.md](security-sandbox.md).

## Release verification

Official prebuilt binaries are cryptographically signed during the release pipeline. The installer script downloads the release manifest (`SHA256SUMS`) and its signature (`SHA256SUMS.sig`), verifying the authenticity of the manifest before comparing the local binary's SHA-256 hash against it.

You can manually trigger release signature verification of your currently installed binary at any time.

**Linux / macOS:**

```bash
curl -sSf https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.sh | bash -s -- --verify
```

**Windows (PowerShell):**

```powershell
$Mode = "verify"; irm https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.ps1 | iex
```

### Managing Release Signing Keys

To sign release manifests securely, the GitHub Actions release pipeline requires an RSA-2048 private key configured as a repository secret named `AHMA_RELEASE_SIGNING_KEY`.

If you need to rotate or generate new signing keys:

1. **Generate the private key:**
   ```bash
   openssl genpkey -algorithm RSA -out release_key.pem -pkeyopt rsa_keygen_bits:2048
   ```

2. **Extract the public key in PEM format:**
   ```bash
   openssl rsa -pubout -in release_key.pem -out ahma-release.pub.pem
   ```
   Copy the contents of `ahma-release.pub.pem` and update `PUB_KEY_PEM` inside:
   - [scripts/install.sh](file:///Users/paulhoughton/github/ahma/scripts/install.sh)
   - [ahma_mcp/src/update/install.rs](file:///Users/paulhoughton/github/ahma/ahma_mcp/src/update/install.rs)

3. **Convert the public key to XML format (for Windows installer):**
   Run the following PowerShell command on Windows to generate the XML representation:
   ```powershell
   # First convert PEM to DER format
   openssl rsa -pubout -outform DER -in release_key.pem -out ahma-release.pub.der
   
   # In PowerShell:
   $rsa = New-Object System.Security.Cryptography.RSACryptoServiceProvider
   $rsa.ImportSubjectPublicKeyInfo([System.IO.File]::ReadAllBytes("ahma-release.pub.der"), [ref]$null)
   $rsa.ToXmlString($false)
   ```
   Update `$PUB_KEY_XML` / `ahma-release.pub.xml` inside [scripts/install.ps1](file:///Users/paulhoughton/github/ahma/scripts/install.ps1).

4. **Configure GitHub Repository Secret:**
   Add the entire content of the private key `release_key.pem` as a secret named `AHMA_RELEASE_SIGNING_KEY` in the repository settings:
   `Settings` -> `Secrets and variables` -> `Actions` -> `New repository secret`.

5. **Clean up:**
   Securely delete the local `release_key.pem` and DER files once done to avoid leakage.


