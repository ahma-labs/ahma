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

Official prebuilt binaries are attested with **GitHub Build Provenance Attestations**
(Sigstore SLSA Level 3). The installer script verifies the Sigstore attestation after
download — no embedded keys, no rotation, no private secrets.

Verify your installed binary at any time:

```bash
ahma verify --self
```

Or use the `gh` CLI for out-of-band verification:

```bash
gh attestation verify ahma-release-linux-x86_64.tar.gz --repo paulirotta/ahma
```

See [docs/release-signing.md](release-signing.md) for the full trust model.

### Release signing and key rotation

Release binaries are signed with an RSA-2048 key held exclusively inside GitHub Actions
secrets — it never exists on any developer machine. All key generation and rotation
happens via a `workflow_dispatch` action in the CI pipeline.

See **[docs/release-signing.md](release-signing.md)** for:

- Full signing architecture and AGPL supply chain defence rationale
- How to verify a release signature manually
- Key rotation procedure (triggered from the GitHub Actions UI — no local machine required)
- Emergency rotation SOP for suspected compromises
- Key rotation log
