# Ahma Environment Variables

> [!IMPORTANT]
> **`AHMA_*` variables in the RETIRED sections below are ignored** (R-CFG1.2), logged as a `WARN`.
> Configure Ahma via `~/.ahma/settings.toml` or CLI flags instead.
> Run `ahma settings init` to create a pre-documented settings file, or `ahma settings show` to inspect effective values.
> This does **not** cover the three [Terminal Hooks](#live--terminal-hooks) variables below, which
> remain live (hooks are invoked directly by the editor, not by `ahma`, so there is no CLI flag to
> replace them), or variables read by the `scripts/install.sh` / `install.ps1` bootstrap installers,
> which run before any `ahma` binary exists.

> [!IMPORTANT]
> **Retirement binds every binary and every subcommand, not just `ahma serve`.**
> A variable listed as RETIRED is ignored by `ahma`, by `ahma-tui`, and by subcommands with
> their own configuration resolution (`ahma update`, `ahma uninstall`) alike. A surface that
> kept honoring a retired name would give one variable two meanings in one product — setting
> it would change one binary's behaviour and not the other's, which is worse than either
> answer on its own. `ahma_common::config::warn_retired_env` is the single function that
> states the verdict — it lives at the bottom of the dependency graph so every crate can
> reach it, and it returns only *whether* a variable was set, never its value, so a caller
> cannot accidentally honor one. Every surface calls it rather than re-reading the variable.
>
> **The tables below are enforced, not just documentation.**
> `ahma_mcp/tests/unit/retired_env_drift_test.rs` parses every RETIRED table here and fails the
> build if any production source reads one of these names directly. Adding a row extends the
> guard automatically. This exists because the tables and the code had genuinely drifted:
> `AHMA_PREFER_MUSL` was listed as retired while `ahma update` still honored it, and
> `AHMA_LOG_TARGET`, `AHMA_TLS_DIR` and the two `AHMA_INSECURE_SKIP_*` variables each warned
> in their own words instead of through the shared verdict. Nothing noticed, because the only
> thing tying the docs to the code was someone remembering.
>
> The one deliberate exception is the **bootstrap installers**, `scripts/install.sh`,
> `scripts/install.ps1` and `scripts/install-local.sh`. They read `AHMA_INSTALL_DIR` because
> they run *before* any `ahma` binary exists: there is no CLI to pass `--install-dir` to and
> no settings file to read. Once `ahma` exists, `ahma update --install-dir` is the supported
> way to install somewhere other than `~/.local/bin`.

## Variable classification

| Class | Meaning |
|-------|---------|
| **LIVE** | Still read and honored. Only the three [terminal-hook](#live--terminal-hooks) variables are in this class. |
| **RETIRED** | Set by user, **ignored** with a startup `WARN`. Use the CLI flag or `~/.ahma/settings.toml` instead. |
| **INTERNAL** | Set only by Ahma itself for parent→child process communication. Never set these manually. |
| **PLATFORM** | Standard OS/ecosystem variables read by Ahma (e.g. `RUST_LOG`, `HOME`). Not `AHMA_*` prefixed. |
| **INTERNAL/TEST** | Set only by test harness code (`#[cfg(test)]`). Never present in production binaries. |

---

## RETIRED — Tool Management

All previously accepted. Now ignored with a `WARN`. Use `~/.ahma/settings.toml` or CLI flags.

| Variable | Replacement | Default |
|---|---|---|
| `AHMA_TOOLS_DIR` | `--tools-dir` CLI flag or `tools.tools_dir` in settings.toml | auto-detect `.ahma/` |
| `AHMA_TIMEOUT` | `--timeout` flag or `tools.timeout_secs` in settings.toml | `360` |
| `AHMA_SYNC` | `--sync` / `--async`, or `tools.execution_mode` in settings.toml | `"sync"` |
| `AHMA_HOT_RELOAD` | none — tool hot-reload was removed entirely (agent-writable tools dir); use the `restart` tool | n/a |
| `AHMA_SKIP_PROBES` | `--skip-probes` flag or `tools.skip_probes = true` in settings.toml | `false` |
| `AHMA_MINIMIZE_TOKENS` | `--minimize-tokens` flag or `tools.minimize_tokens = true` in settings.toml | `false` |
| `AHMA_SMALL_MODEL_HARNESS` | `--small-model-harness` flag or `tools.small_model_harness = true` in settings.toml | `false` |

`AHMA_MINIMIZE_TOKENS` and `AHMA_SMALL_MODEL_HARNESS` are ignored by **both** `ahma` and
`ahma-tui`. The TUI honored them for a while after `ahma` had already retired them, which
meant setting one changed the chat client's behaviour but not the server's.

---

## RETIRED — Sandbox & Security

> [!WARNING]
> Security-tier variables were immediately ignored (never honored) per R-CFG2.3. Preference-tier variables were honored during the migration window and are now also ignored.

| Variable | Replacement |
|---|---|
| `AHMA_DISABLE_SANDBOX` | `--no-sandbox` CLI flag (**CLI-only** per R-CFG2.3) |
| `AHMA_SANDBOX_SCOPE` | `--sandbox-scope` CLI flag or `sandbox.scopes` in user settings.toml |
| `AHMA_SANDBOX_DEFER` | `--defer-sandbox` CLI flag or `sandbox.defer = true` in settings.toml |
| `AHMA_WORKING_DIRS` | `--working-dir` CLI flag or `sandbox.working_dirs` in settings.toml |
| `AHMA_TMP_ACCESS` | `--tmp` CLI flag or `sandbox.tmp_access = true` in settings.toml |
| `AHMA_DISABLE_TEMP` | `--disable-temp-files` CLI flag or `sandbox.disable_temp = true` in settings.toml |
| `AHMA_NO_PACKAGE_CACHE_WRITE` | `--no-package-cache-write` flag or `sandbox.package_cache_write = false` in settings.toml |
| `AHMA_TASK_VAULT` | `--task-vault` CLI flag |

---

## RETIRED — Authentication

| Variable | Replacement |
|---|---|
| `AHMA_REQUIRE_TOKEN` | `--require-token` CLI flag or `auth.require_token` in settings.toml |
| `AHMA_REQUIRE_TOKEN_PATH` | `--require-token-path` CLI flag or `auth.require_token_path` in settings.toml |
| `AHMA_RATE_LIMIT_RPS` | `--rate-limit-rps` CLI flag or `auth.rate_limit_rps` in settings.toml |
| `AHMA_RATE_LIMIT_BURST` | `--rate-limit-burst` flag or `auth.rate_limit_burst` in settings.toml |
| `AHMA_HTTP_CLIENT_TOKEN_PATH` | none — OAuth tokens for external HTTP MCP servers always live in `~/.ahma/mcp_http_token.json` |

---

## RETIRED — Logging

| Variable | Replacement |
|---|---|
| `AHMA_LOG_TARGET` | `logging.target = "stderr"` in settings.toml or `--log-to-stderr` CLI flag |
| `AHMA_LOG_MONITOR` | `--log-monitor` CLI flag or `logging.log_monitor = true` in settings.toml |
| `AHMA_MONITOR_RATE_LIMIT` | `--monitor-rate-limit` flag or `logging.monitor_rate_limit_secs` in settings.toml |
| `AHMA_LOG_DIR` | `--log-dir` CLI flag or `logging.dir` in settings.toml |

---

## RETIRED — HTTP Transport

| Variable | Replacement |
|---|---|
| `AHMA_HTTP_PORT` | `--port` CLI flag on `serve http` subcommand |
| `AHMA_HTTP_URL` | `ahma tui --connect <URL>` |
| `AHMA_UNIX_SOCKET` | `--unix-socket-path` CLI flag or `http.unix_socket_path` in settings.toml |
| `AHMA_UNIX_SOCKET` (TUI) | `ahma tui --connect unix://<path>`, or the same `http.unix_socket_path` settings key |
| `AHMA_DISABLE_QUIC` | `--disable-quic` CLI flag or `http.disable_quic = true` in settings.toml |
| `AHMA_DISABLE_HTTP1_1` | `--disable-http1-1` CLI flag or `http.disable_http1_1 = true` in settings.toml |
| `AHMA_HANDSHAKE_TIMEOUT` | `--handshake-timeout` CLI flag or `http.handshake_timeout_secs` in settings.toml |

---

## RETIRED — TLS / Update

| Variable | Replacement |
|---|---|
| `AHMA_TLS_DIR` | `--tls-dir` CLI flag |
| `AHMA_INSECURE_SKIP_VERIFY` | `--insecure-skip-verify` CLI flag (**CLI-only** per R-CFG2.3) |
| `AHMA_INSECURE_SKIP_SIGNATURE` | `--insecure-skip-signature` CLI flag (**CLI-only** per R-CFG2.3) |
| `AHMA_PREFER_MUSL` | `--prefer-musl` CLI flag on `update` subcommand |
| `AHMA_INSTALL_DIR` | `--install-dir` CLI flag on `update` subcommand (still read by the bootstrap installer scripts — see the note at the top) |
| `AHMA_INSTANCE_LABEL` | `--instance-label` CLI flag or `instance.label` in settings.toml |

---

## LIVE — Terminal Hooks

These are the **only** `AHMA_*` variables still honored. They are used by hook subprocesses spawned
by editors, set **before** the hook binary runs (by the editor or the user's shell profile), and
cannot be replaced by CLI flags since hooks are invoked directly by the editor, not by ahma.

| Variable | Default | Description |
|---|---|---|
| `AHMA_HOOKS` | `auto` | `on` = always sandbox, `off` = pass through, `auto` = sandbox when MCP server detected |
| `AHMA_DISABLE_HOOKS` | off | Alias for `AHMA_HOOKS=off`. Set to `1` to disable hook routing. |
| `AHMA_PREFER_OWN_SANDBOX` | off | Set to `1` so a hook applies **ahma's own** sandbox instead of deferring to a detected host sandbox (Cursor, VS Code, Docker). Accepts the double-sandbox and the host's build-cache friction in exchange for ahma being the authority. See SPEC R7 and [security-sandbox.md](security-sandbox.md#nested-sandbox-environments-cursor-vs-code-docker). |

```bash
# Temporarily disable hook routing without uninstalling
AHMA_HOOKS=off
# or
AHMA_DISABLE_HOOKS=1
```

---

## INTERNAL — Process Communication

> [!CAUTION]
> These variables are set **only by Ahma itself** for parent→child subprocess communication.
> Never set them manually — doing so may confuse the subprocess and produce unpredictable behavior.

| Variable | Set by | Purpose |
|---|---|---|
| `AHMA_SERVER_CHILD` | Parent bridge process | Tells a child subprocess it was spawned by a parent bridge. Equivalent to `--server-child` flag. |
| `AHMA_MCP_ARGS` | HTTP bridge | Passes resolved tool configuration to the per-session subprocess. |
| `AHMA_RESTARTED` | `re_exec_current_process()` | Prevents infinite re-exec loops during version-mismatch auto-restart. |
| `AHMA_OUTER_SANDBOX_PID` | Every command ahma runs inside its kernel sandbox | The pid of the ahma that sandboxed the command. A nested ahma (ahma's test suite or `ahma serve` run through `run_terminal_command`) reads it only to *name* the outer sandbox it defers to on macOS, where Seatbelt cannot nest (SPEC R7.6). A marker, not a setting. |

---

## INTERNAL/TEST — Test Isolation

> [!CAUTION]
> These are set only by `init_test_daemon_isolation()` inside `#[cfg(test)]` code.
> They are never present in production builds. Do not set these manually.

| Variable | Purpose |
|---|---|
| `AHMA_DAEMON_SOCK` | Isolates each test process's daemon to a unique Unix socket path |
| `AHMA_DAEMON_PORT` | Isolates each test process's daemon to a unique TCP port (Windows) |
| `AHMA_TEST_BINARY` | Locates the compiled test binary for in-process test helpers |
| `AHMA_TEST_HTTP_CLIENT_TOKEN_PATH` | Redirects the external-MCP OAuth token file in `ahma_http_mcp_client` tests. Compiled in **debug builds only** — a release binary ignores it |
| `AHMA_TEST_LOG_DIR` | Redirects the project log directory in `ahma_mcp` unit tests. Read only under `cfg!(test)` — no shipped binary contains the read |
| `AHMA_TEST_HOME` | Redirects `~` resolution (`ahma_common::config::ahma_home_dir`) at a temp directory so a test can supply its own `~/.ahma/settings.toml`. Compiled in **debug builds only** (`#[cfg(debug_assertions)]`) — a release binary ignores it |
| `AHMA_TEST_ISOLATION` | Set by test harnesses on spawned ahma binaries: forces private (non-global) bridge/daemon endpoints (SPEC R-ISO.1) |
| `NEXTEST` / `NEXTEST_RUN_ID` | Set by `cargo nextest`, inherited by spawned binaries; read solely to force the same private-endpoint isolation as `AHMA_TEST_ISOLATION` — the single R-CFG9.2 carve-out (SPEC R-ISO.1) |

---

## PLATFORM — Standard OS Variables

These are standard ecosystem variables that Ahma reads but does not define:

| Variable | Purpose |
|---|---|
| `RUST_LOG` | Log verbosity: `debug`, `info`, `warn`, `error`, or crate-specific filters |
| `HOME` / `USERPROFILE` | Home directory for `~` expansion and `~/.ahma/` paths |
| `XDG_RUNTIME_DIR` | Linux: per-user runtime directory for daemon socket |
| `CARGO_HOME` | Cargo home override; affects package cache scope |
| `PATH` | Executable search path |
| `NO_COLOR` | Any non-empty value suppresses colour in the TUI (https://no-color.org). Styles keep bold/dim emphasis; only hues are dropped. Glyph choice is separate — it follows `TERM` |
| `TERM` | `dumb` selects ASCII fallbacks for box-drawing and status glyphs |
| `OTEL_*` / `TRACEPARENT` | OpenTelemetry distributed tracing. Requires a binary built with the `otel` cargo feature (off by default; release binaries ship it — see `ahma_common/Cargo.toml`'s `otel` feature comment). Without it these variables, and `--opentelemetry <url>`, are accepted but produce no export |

---

## Migration guide

To migrate from environment variables to settings.toml:

```bash
# 1. Create the settings file with all options documented
ahma settings init

# 2. Edit it to set your preferences (e.g. timeout, log_monitor, etc.)
# File location: ~/.ahma/settings.toml

# 3. Verify the effective configuration
ahma settings show

# 4. Remove AHMA_* variables from your shell profile
```

For options that **must** be CLI flags (security-tier, R-CFG2.3), add them to the `args` array
in your IDE's `mcp.json`:

```json
{
  "mcpServers": {
    "ahma": {
      "command": "ahma",
      "args": ["serve", "stdio", "--timeout", "600", "--sandbox-scope", "/home/user/projects"]
    }
  }
}
```
