# Installation

## Update (recommended)

If `ahma` is already installed:

```bash
ahma update                    # latest published release
ahma update 0.15.2             # specific release tag
ahma update main               # build from Git branch
ahma update <branch-name>      # build from a named feature branch
ahma update --install-hooks    # also install user-scoped terminal hooks
```

Use `--force` to reinstall when the version already matches. Use `--dry-run` to preview actions.
When run interactively, `ahma update` now offers user-scoped terminal hook installation if none are currently managed.

Custom install location: `--install-dir ~/.local/bin`. `AHMA_INSTALL_DIR` is retired
(R-CFG1.2) and ignored by `ahma update` / `ahma uninstall`; only the bootstrap installer
scripts still read it, because they run before any `ahma` binary exists.

## First-time install

You need `ahma` on PATH before `ahma update` works.

### Linux / macOS

**Latest (build from GitHub main via Cargo — requires [Rust](https://rustup.rs/)):**

The workspace uses `reqwest` with the `http3` feature, so `cargo install --git` builds require
`RUSTFLAGS='--cfg reqwest_unstable'` (config resolution for `cargo install` does not pick up the
source repo's `.cargo/config.toml`, which sets this automatically for `cargo build` inside a
checked-out clone — see [Build from source](#build-from-source-full-checkout) below).

```bash
RUSTFLAGS='--cfg reqwest_unstable' \
  cargo install --git https://github.com/paulirotta/ahma ahma_bin --bin ahma --root ~/.local --locked --force
```

**Specific branch** (replace `<branch-name>` with a real branch, e.g. `main`):

```bash
RUSTFLAGS='--cfg reqwest_unstable' \
  cargo install --git https://github.com/paulirotta/ahma --branch <branch-name> ahma_bin --bin ahma --root ~/.local --locked --force
```

**Unpushed local checkout:**

```bash
cargo install --path ahma_bin --bin ahma --root ~/.local --locked --force
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

**Specific branch (builds via Cargo — requires Rust)**, replacing `<branch-name>` with a real branch, e.g. `main`:

```powershell
# Save script locally, then:
.\install.ps1 <branch-name>
```

Or invoke Cargo directly:

```powershell
$env:RUSTFLAGS='--cfg reqwest_unstable'
cargo install --git https://github.com/paulirotta/ahma --branch <branch-name> ahma_bin --bin ahma --root $HOME\.local --locked --force
```

Ensure `$HOME\.local\bin` is on your PATH.

## Build from source (full checkout)

**Linux / macOS**

```bash
git clone https://github.com/paulirotta/ahma.git
cd ahma
cargo build --release -p ahma_bin
mv target/release/ahma ~/.local/bin/
```

**Windows (PowerShell)**

```powershell
git clone https://github.com/paulirotta/ahma.git
cd ahma
cargo build --release -p ahma_bin
Copy-Item target\release\ahma.exe "$HOME\.local\bin\"
```

## After installation

Run the setup wizard to configure everything below in one step:

```bash
ahma setup          # interactive: asks which editors/features to configure
ahma setup -y       # non-interactive: auto-detect and configure with defaults
```

`ahma setup` writes MCP server entries and agent skills for the editors it detects (Cursor, VS
Code, Claude Code, Codex, …). [Terminal hooks](#terminal-hooks) are opt-in and excluded from the
default selection — this is an experimental project and sandbox-exception handling isn't fully
hardened yet, so an automatically installed hook could interfere with your work. Pass `--hooks`,
or include hooks at the interactive prompt, to opt in. It is safe to re-run at any time — it only
updates the pieces you ask it to (`--mcp`, `--hooks`, `--skills`, `--tls`). `ahma uninstall`
reverses it with the same prompts. Restart your editor after it runs.

To configure by hand instead, or to see what the wizard does under the hood:

- MCP client config — see [connection-modes.md](connection-modes.md).
- Terminal hooks — see [Terminal hooks](#terminal-hooks) below.
- Agent skill — see [agent-skills.md](agent-skills.md).
- Restart MCP clients or reload your IDE after updating the binary.

### Terminal hooks

Terminal hooks route the shell commands an agent runs through its *native* terminal/Bash tool into ahma's kernel sandbox (the MCP server only sandboxes tools the agent calls explicitly). Supported clients and their config files:

| Client | User scope | Project scope |
|---|---|---|
| **Cursor** | `~/.cursor/hooks.json` | `<repo>/.cursor/hooks.json` |
| **Claude Code** | `~/.claude/settings.json` | `<repo>/.claude/settings.json` |
| **Codex** | `~/.codex/hooks.json` | `<repo>/.codex/hooks.json` |
| **GitHub Copilot CLI** | `~/.copilot/hooks/ahma.json` | `<repo>/.github/hooks/ahma.json` |
| **Antigravity** | `~/.gemini/config/hooks.json` | `<repo>/.agents/hooks.json` |

VS Code and Claude Desktop have no execution-hook mechanism and are not supported.

```bash
ahma hooks install                                   # user-scoped, all supported clients
ahma hooks install --platform claude,codex --scope project
ahma hooks status                                    # effective state + where installed
ahma hooks uninstall --platform cursor --scope project
```

- **Installed ≠ active.** `install` only writes the hook file. In the default `auto` mode a hook is **active** only when an ahma MCP server is detected for that client; otherwise it passes commands through **unsandboxed**. `ahma hooks status` prints the *effective* verdict (ACTIVE / INACTIVE) and the reason — always check it after installing.
- **Off-switch**: set `AHMA_HOOKS=off` (or `AHMA_DISABLE_HOOKS=1`) in your shell environment, or `ahma hooks uninstall`. `AHMA_HOOKS=on` forces hooks active regardless of detection.
- **Fail-safe, not silent.** If ahma is active but *cannot* sandbox a command (missing binary, no kernel support, unknown working directory), the command is **blocked** — not run unsandboxed — with an actionable message to both user and agent. Diagnose with `ahma hooks doctor`; to allow unsandboxed execution for the current session only (cleared on reboot), run `ahma hooks approve-unsandboxed`, and revoke with `ahma hooks revoke`. The only silent pass-through is when ahma is *inactive* (the off/auto-undetected case above). Cursor hooks ship with `failClosed: false` so a crashed/absent hook binary never wedges your terminal.

## Platform notes

Supported prebuilt release platforms: Linux x86_64/arm64, macOS Apple Silicon, Windows x86_64. Musl builds are available for Linux x86_64 and ARM64 (`ahma update --prefer-musl`; the `AHMA_PREFER_MUSL` env var is deprecated).

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

See **[docs/release-signing.md](release-signing.md)** for:

- Keyless Sigstore trust model architecture and AGPL supply chain defense rationale
- Step-by-step instructions for out-of-band manual verification of release signatures
- Details on Sigstore OIDC issuing identities and transparency logging properties
