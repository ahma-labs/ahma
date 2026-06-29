# Security Sandbox

Ahma enforces **kernel-level filesystem sandboxing** by default. The sandbox scope is set once at server startup and cannot be changed — the AI has full access within the scope but zero access outside it, regardless of how commands are constructed.

## Why Kernel-Level Sandboxing?

Trust-based security ("do you trust this tool?") doesn't protect against mistakes or manipulation at speed. Kernel-enforced boundaries do:

- **String filters can be bypassed** via path traversal, symlinks, or creative shell expansion. Kernel-level policies cannot.
- **Blast radius is bounded** — even if an AI agent tries `rm -rf ~`, the kernel rejects any write outside the workspace.
- **No runtime overhead** — the policy is applied once at sandbox lock and enforced by the OS.

## Sandbox Scope

The sandbox scope is the root directory boundary for all filesystem operations:

- **STDIO mode**: Defaults to the current working directory (`--cwd` set by the IDE). In `mcp.json`, set `"cwd": "${workspaceFolder}"` and the sandbox "just works".
- **HTTP mode**: Set once when the server starts. Configure via:
  1. `--sandbox-scope <path>` CLI flag (highest priority)
  2. `scopes = [...]` in `~/.ahma/settings.toml`
  3. MCP client `roots/list` (when `--defer-sandbox` is used)
  4. Current working directory (when not filesystem root)
  5. Default `sandbox_directory` from settings (auto-created `~/sandbox`)

The default `sandbox_directory` (`~/sandbox`) is auto-created on first use. This ensures that MCP clients that don't send `roots/list` (e.g., Antigravity) have a working sandbox scope without manual configuration. Configure it in `~/.ahma/settings.toml`:

```toml
[sandbox]
sandbox_directory = "~/sandbox"  # default; set to "" to disable
```

**Security invariant**: Once the sandbox scope is set, it cannot be changed for the lifetime of the server process. Any attempt to change it after lock terminates the session.

## Platform-Specific Enforcement

### Linux (Landlock)

On Linux, Ahma uses [Landlock](https://docs.kernel.org/userspace-api/landlock.html) — a kernel LSM that applies fine-grained filesystem access rules in-process with no daemon or capability escalation required.

**Requirements**: Linux kernel 5.13 or newer (released June 2021). The server refuses to start on older kernels unless sandbox is explicitly disabled.

```bash
uname -r                            # check kernel version
cat /sys/kernel/security/lsm        # verify landlock is active
```

**Older kernels / Raspberry Pi**: Landlock requires kernel ≥ 5.13. On older Pi OS kernels, run with:

```bash
export AHMA_DISABLE_SANDBOX=1
ahma serve stdio
```

or add `"--disable-sandbox"` to `mcp.json` args.

### macOS (Seatbelt)

On macOS, Ahma uses Apple's built-in `sandbox-exec` with a generated Seatbelt profile (SBPL) that restricts write access to the sandbox scope. No additional installation required.

**Requirements**: Any modern macOS version. `sandbox-exec` is built into macOS.

### Windows (Job Objects + AppContainer)

On Windows, Ahma uses Job Object enforcement (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) at startup, with AppContainer profile DACL grants for per-scope access control. PowerShell (5.1+) is the shell. See [SPEC.md R6.3](../SPEC.md) for status.

## Nested Sandbox Environments

When running inside Cursor, VS Code, or Docker, the outer environment may prevent Ahma from applying its own sandbox. Ahma detects this and exits with instructions.

## HTTP Transport Authentication

When running in HTTP mode (`ahma serve http`), all `/mcp` endpoints are protected by **bearer token authentication**:

```jsonc
// mcp.json
{
  "mcpServers": {
    "ahma": {
      "url": "http://localhost:4000/mcp",
      "headers": { "Authorization": "Bearer YOUR_TOKEN" }
    }
  }
}
```

- Start the server with `--require-token <token>` or set `AHMA_REQUIRE_TOKEN`.
- The `Authorization: Bearer` scheme is **case-insensitive** (RFC 7235 §2.1).
- The `/health` endpoint is explicitly **exempt** from authentication so orchestrators can probe liveness without credentials.
- Bearer tokens are compared in **constant time** to prevent timing attacks.
- **Hot-reload**: Send `SIGHUP` to swap the token without restarting; the new token is read from config and applied atomically.

**Manual override** (when you know the outer environment is safe):

```bash
ahma --disable-sandbox
# or
export AHMA_DISABLE_SANDBOX=1
```

Common `mcp.json` for nested environments (VS Code with workspace scoping):

```json
{
    "servers": {
        "Ahma": {
            "type": "stdio",
            "command": "ahma",
            "args": ["--tmp", "--livelog", "--simplify"]
        }
    }
}
```

## Package Manager Cache Write (`--no-package-cache-write`)

By default, ahma grants **write access** to the package-manager fetch directories so that agents can autonomously upgrade dependencies (e.g. `cargo add sqlx@0.9`, `cargo update`) without requiring `--sandbox-scope ~/.cargo`:

| Path | Access |
|------|--------|
| `~/.cargo/registry/` | Read + Write |
| `~/.cargo/git/` | Read + Write |
| `~/.cargo/.package-cache` | Read + Write |
| `~/.cargo/.package-cache-mutate` | Read + Write |
| `~/.cargo/bin/` | Read only |
| `~/.cargo/config.toml` | Read only |
| `~/.cargo/credentials.toml` | Read only |

> **Do not use `--sandbox-scope ~/.cargo`**: that flag grants **read-write to the entire cargo home**, including installed binaries and credentials. The built-in `package_cache_write` feature is narrower and safer.

To disable (strictest isolation):

```bash
ahma serve stdio --no-package-cache-write
# or
AHMA_NO_PACKAGE_CACHE_WRITE=1 ahma serve stdio
# or in ~/.config/ahma/settings.toml:
# [sandbox]
# package_cache_write = false
```

`$CARGO_HOME` is respected; defaults to `~/.cargo`.

### `cargo install` / `cargo binstall` and other tool installs

`cargo install`, `cargo binstall`, `rustup component add`, `npm i -g`, etc. write a
binary into `~/.cargo/bin` (or the equivalent) **and** update an install manifest
such as `~/.cargo/.crates.toml`. Those paths are intentionally **read-only** (see
the table above), so the install fails with a low-level kernel error — on macOS:

```
error: failed to open: /Users/<you>/.cargo/.crates.toml

Caused by:
  Operation not permitted (os error 1)
```

This is expected and is **not** a bug: installing global binaries is denied by
default. There is no special flag for it — the supported remedy is the standard
**runtime-denial grant loop**:

1. ahma detects the denied path from the command's stderr and returns a structured
   `sandbox_denial` error (over MCP) or prints an `ahma sandbox grant …` hint (in a
   hooked native terminal).
2. **Grant the path** — via the `sandbox_grant` MCP tool (preview, then `confirm: true`),
   or on the CLI: `ahma sandbox grant ~/.cargo/bin` (and `~/.cargo` for the
   manifest). The grant is written to `~/.ahma/settings.toml`, which lives outside
   every sandbox scope. Credential/config files are never auto-granted and the
   path is risk-classified before it is offered.
3. **Apply it** — run the `restart` MCP tool (or restart the server) so the new
   scope takes effect; scopes are immutable for the lifetime of a running session.
4. **Re-run** the original command.

If a maintenance script bootstraps tools (e.g. `cargo install cargo-binstall`),
expect the first run to surface a grant prompt; once granted and applied, the
script proceeds. Prefer scripts that install into a workspace-local directory
(`cargo install --root <workspace>/.tools`) when you want installs to land
in-scope without any grant.

## Temp Directory Access (`--tmp`)

By default, the system temp directory is accessible only via platform-implicit rules. Use `--tmp` (or `AHMA_TMP_ACCESS=1`) to add it as an explicit read/write scope — useful for compilers and build tools.

| Flag combination | Behavior |
|-----------------|----------|
| (default) | Temp access via platform rules |
| `--tmp` | Temp dir added as explicit scope |
| `--disable-temp-files` | Temp access blocked entirely |
| `--tmp --disable-temp-files` | `--disable-temp-files` wins (blocked) |

**Security considerations**: `/tmp` is shared by all users and processes. Use `mktemp` with random suffixes to avoid TOCTOU attacks. Clean up sensitive temp files after use.

## Live Log Monitoring (`--livelog`)

The `--livelog` flag grants additional read-only access to specific log files via symlinks in the `log/` directory at server startup — see [live-log-monitoring.md](live-log-monitoring.md) and [SPEC.md R9](../SPEC.md).

---

## Task Vaults

A **Task Vault** is a per-question isolated working directory that promotes the "dedicated folder per task" security principle from user discipline to a kernel-enforced architectural guarantee.

### Why task vaults?

Cowork's user guidance says: _"Create a per-task working folder. Copy in inputs."_ This is good advice, but it relies on users remembering to follow it. In Ahma, a vault is the only way to start a task — there is no "grant my whole Documents folder" option.

### Directory layout

```
~/.ahma/tasks/<utc-date>-<slug>-<hex>/
  inputs/       — copies of user-provided files (read intent; never modified in-place)
  workdir/      — kernel sandbox scope root; all agent commands run here
  outputs/      — artifacts produced by tools (HTML reports, CSV exports, etc.)
  trash/        — staged-deletion holding area (two-phase delete)
  audit.jsonl   — append-only JSONL audit log of all operations
```

### Creating a vault

```bash
# Create a vault and print its root path
VAULT=$(ahma vault create summarise-q4-report)
echo $VAULT
# ~/.ahma/tasks/20260520T120000Z-summarise-q4-report-a1b2c3d4e5f60001/

# Start an ahma HTTP bridge scoped to that vault's workdir
ahma serve http --task-vault "$VAULT"
```

Or in a single `mcp.json` entry:

```json
{
    "servers": {
        "Ahma (vault)": {
            "type": "stdio",
            "command": "sh",
            "args": ["-c", "ahma serve stdio --task-vault \"$(ahma vault create $SLUG)\""]
        }
    }
}
```

### Security properties

| Property | Detail |
|----------|--------|
| **Kernel-enforced scope** | The sandbox scope is `<vault>/workdir/` — the kernel rejects writes outside it |
| **Inputs are copies** | The agent never touches original files — only copies placed in `inputs/` |
| **Two-phase delete** | `trash/` holds staged deletions; `purge` requires explicit confirmation |
| **Append-only audit** | `audit.jsonl` records every tool call, artifact write, and elevation grant |
| **Egress allowlist** | `egress.allowlist` in the vault root controls outbound network access |

### Two-phase delete

The AI can never permanently delete a file in one step. The `TrashManager` enforces:

1. **Stage** — the file is moved to `trash/<timestamp>_<filename>`; the original location is empty immediately.
2. **Review** — `ahma vault list-staged <vault>` shows what is waiting.
3. **Purge** — only after explicit per-batch confirmation does `purge()` permanently remove staged entries.

This limits the blast radius of a confused `rm -rf` to a recoverable staging operation.

### Audit log format

Each line in `audit.jsonl` is a JSON object:

```json
{"timestamp":"2026-05-20T12:00:01Z","type":"tool_call","operation_id":"op_1","tool_name":"cargo_build","args_summary":"--release"}
{"timestamp":"2026-05-20T12:00:04Z","type":"tool_complete","operation_id":"op_1","success":true,"duration_ms":3200}
{"timestamp":"2026-05-20T12:00:05Z","type":"artifact_written","path":"outputs/result.html","size_bytes":4096}
```

### Renewal contract

A task vault session that runs unattended for more than `T_renew` seconds (default 5 minutes) is automatically halted with a checkpoint written to the vault. The user must re-approve before the operation continues. This closes Cowork's "scheduled task drift" vulnerability where a re-injection can steer a long unattended run without the user knowing.

---

## Egress Sandbox

The **egress sandbox** closes the network egress carve-out present in Cowork (web-fetch and MCP connections bypass org egress policies).

Each task vault can have an `egress.allowlist` file:

```
# ahma egress allowlist — one domain pattern per line
api.openai.com          # explicit allow
*.anthropic.com         # wildcard subdomain
# (empty = deny all outbound connections)
```

When `ahma serve http --task-vault <vault>` starts, an egress proxy is bound to a random localhost port and injected into the subprocess environment:

```
HTTP_PROXY=http://127.0.0.1:<port>
HTTPS_PROXY=http://127.0.0.1:<port>
NO_PROXY=127.0.0.1,::1,localhost
```

Requests to domains not in the allowlist receive `407 Proxy Authentication Required` (CONNECT) or `403 Forbidden` (plain HTTP), indistinguishable from a real network failure — the subprocess learns nothing about which domains are blocked.

The kernel FS sandbox prevents the subprocess from modifying `/etc/hosts` or `/etc/resolv.conf`, so DNS rebinding cannot route traffic around the proxy.
