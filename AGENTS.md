# AGENTS.md

How to work in this repo. **What** the product does and **why** lives in [SPEC.md](SPEC.md)
(root) and each crate's own `SPEC.md`. If you need to state a product rule, it goes in a
SPEC, not here and not only in code.

> **A rule that binds one surface binds all of them.** If you write a constraint as a comment
> in the one file that currently obeys it, the next surface to implement the same thing will
> not know about it, and it regresses silently. Promote it to the relevant `SPEC.md` and
> reference the requirement id from the code.

## Build, test, verify

Standard cargo. Package names use **underscores** (`-p ahma_http_bridge`, not `ahma-http-bridge`).
Prefer `cargo nextest run` over `cargo test`. Rust edition 2024, MSRV in `Cargo.toml`.

Incubating features are quarantined behind non-default features (`simplify`,
`full`) — test them directly with `-p`.

### Definition of done

Before you claim "all green", run **both**:

1. `cargo nextest run`
2. `cargo nextest run --workspace --run-ignored all` — ignored tests here are expensive
   stress/regression coverage, not dead weight; they are part of the required set.
3. **If you touched `Cargo.toml`/`Cargo.lock`:** the *full* `cargo deny check`.
   Not `cargo deny check advisories`. The full check also enforces licences, and a licence is
   what actually broke main (#470): `zip 8.6`'s default features silently pulled in `bzip2`,
   whose licence is not on the allow-list. A new transitive dep arrives with a licence you did
   not choose, so the advisories subset proves nothing about it.

If an ignored test can't run (missing platform prerequisite) or is broken, say so in the PR
with a repro — don't quietly drop it.

### Target Directory & Cache Management

The workspace profile is configured (`[profile.dev]` and `[profile.test]`) to strip dependency
debug symbols (`debug = 0` for `package."*"`) while preserving fast line-table debug info
(`debug = 1`) for workspace code. Deliberately does **not** raise dependency `opt-level`: that
trades a real, repeated compile-time cost (no benefit on CI's fresh, non-incremental builds) for
a runtime speedup only local dev sessions actually reuse — it once pushed the Windows CI job over
its 40-minute budget. To keep `target/` bloat bounded during long development sessions:

* `cargo xtask clean-stale` (default) prunes `target/*/incremental/` sessions older than 3 days,
  plus coverage counters (`*.profraw`/`*.profdata`) and `target/tmp/` at any age. It leaves
  `deps/`/`build/`/`.fingerprint/` alone on purpose: cargo bumps an artifact's mtime only when it
  recompiles it, so a rarely-rebuilt dependency looks old while still being exactly what the next
  build links against, and evicting it buys a pointless relink.
* **The default reclaims nothing inside an active session** — everything it is willing to touch
  is younger than the cutoff. On a workspace `target/` that had grown to 17.5 GB in one day, the
  default freed 0 bytes. When you are actually short of disk, you need one of:
  * `--aggressive` — also prunes `deps/`, `build/`, `.fingerprint/` and `examples/`. Combined
    with `--max-age-days 0` this took that same tree from 17.51 GB to 0.27 GB. The next build
    is a full rebuild; that is the trade, and it is safe in kind — cargo detects a missing
    output even when the fingerprint is fresh, and recompiles.
  * `--max-size-gb N` — a hard ceiling. After the age pass it keeps removing oldest-first until
    `target/` fits, and says so if it cannot get there. This is the only option that *bounds*
    growth rather than reacting to it; prefer it for long sessions.
* Use `--dry-run` to preview. Every run reports `target/`'s total size before and after, because
  unbounded growth is otherwise invisible until a build dies on a full disk.
* Don't run any of this concurrently with a build or test run.

### When to route through ahma vs native tools

Use ahma's `run_terminal_command` when the command **writes to disk** (kernel sandbox keeps
writes in-workspace), **runs more than a few seconds** (async, returns an operation_id so you
can work meanwhile), needs **mid-run error watching** (`monitor_level`), or when several
independent commands should run **concurrently**.

Keep read-only inspection (read/grep/glob/find/replace) on native file tools — faster and
cheaper than a round trip through MCP.

## Testing

### Test pyramid — read this first

**Prefer in-process tests over subprocess E2E.** This is the single most important rule for CI
stability: GitHub runners have 2 cores, every spawned subprocess burns scheduler slots, and at
20+ parallel tests this causes IPC pipe back-pressure and handshake timeouts.

| Layer | Use for | Helpers |
|---|---|---|
| **Unit** (default) | Logic, schema gen, config parsing, state machines | `#[cfg(test)]` |
| **Integration (in-process)** | MCP protocol logic, tool dispatch, path security, async ops | `create_in_process_mcp_from_dir()` / `_with_scope()` |
| **E2E (subprocess)** — sparingly | Binary wiring, CLI flags, cross-binary IPC | `ClientBuilder`, `spawn_http_bridge` |

Decision rule: *can this be written without forking a process?* If yes, do that.

⚠️ **`setup_mcp_service_with_client()` spawns a subprocess** despite the name — it is E2E.

**Asserting on what the client receives.** For anything ahma *pushes* — `notifications/progress`
above all — assert on the wire, not on the router's bookkeeping. `test_utils::recording_client::
RecordingClient` is a real `ClientHandler` that records notifications and answers `roots/list`;
pass it to `create_in_process_mcp_with_client(client, configs, scopes)`. Its `clientInfo.name` is
configurable because ahma keys real behaviour off it (`supports_progress`, `request_budget`), and
passing **empty `scopes`** gives you a server whose sandbox scope never settles — the only way to
observe the `tools/call` gate in-process.

Progress tokens are the *client's* to mint: rmcp assigns one per request and overwrites anything
you set, so use `call_tool_observing_token()` to learn the token a request actually carried
rather than trying to choose it.

### Rules that break CI when ignored

- **Every test uses `tempfile::tempdir()`.** Never create files in the repo tree.
- **Never hardcode timeouts.** Windows runners are 3–5× slower. Use
  `ahma_common::timeouts::{TestTimeouts, TimeoutCategory}` — semantic categories
  (`Handshake`, `ToolCall`, `SandboxReady`, …), `scale_secs()`, `poll_interval()`.
- **Never hardcode `/tmp`, `/var/folders`, `/dev/null`.** Use `test_utils::path_helpers`:
  `test_temp_path`, `test_out_of_scope_path`, `test_blocked_device_path`, `test_abs`, `test_root`.
- **Never hardcode `/bin/sh`, `/bin/bash`, or bash redirection** (`>&2`, `2>&1`) in command
  strings sent through the tool pipeline — on Windows the shell is PowerShell.
- **Never `#[cfg(unix)]` a test that has a cross-platform equivalent.** Gating is correct only
  when the test genuinely needs a Unix-only API (e.g. `std::os::unix::fs::symlink`).
- **`Path::starts_with` is case-sensitive on Windows** though the filesystem isn't —
  `dunce::canonicalize` both sides before comparing.
- **Spawned test processes must die with the test.** Set `.process_group(0)` and kill with
  `kill(-pgid)`, never `child.kill()` alone — that signals one PID and orphans grandchildren.
  A test that SIGKILLed only its direct shell leaked a 100%-CPU busy loop on every single suite
  run; CI never noticed because runners are discarded. See `kill_process_tree`.
- **Don't add `test-threads` to `[profile.default]`.** Local runs are meant to use full
  parallelism; CI throttling belongs in `--profile ci` only.

### Hard invariants — fix the harness, don't weaken the assertion

**MCP Streamable HTTP handshake** (E2E HTTP tests only; unit/in-process exempt), in order:
`initialize` (no session header) → open SSE stream **before** `notifications/initialized` →
send `notifications/initialized` → answer the server's `roots/list` over SSE with the same id →
only then `tools/call`.

**Sandbox gating is observable**: `tools/call` before sandbox lock returns HTTP 409 with
JSON-RPC `-32001`. Assert it explicitly.

**Dual-transport coverage** (SPEC §R15.5): every HTTP-bridge test calling `tools/call` or
`tools/list` runs against **both** `application/json` and `text/event-stream`. Extract the body
into `run_*(mode)` and add `_json`/`_sse` entry points; set up with `common::setup_test_mcp(mode)`
(not the legacy `sse_test_helpers`). Exempt: `sse_*`, `handshake_*`, `sandbox_*` tests.

**No print-only integration tests.** Printing is fine; asserting on success/failure and key
output patterns is mandatory.

### Reproducing failures

Capture full logs (`<cmd> 2>&1 | tee …`) and reduce concurrency to a single test
(`RUST_TEST_THREADS=1 … --no-capture`) — narrow filters so only the failure prints.

## Code conventions that differ from defaults

- **Never `std::fs` or blocking I/O in an async fn** — use `tokio::fs`/`tokio::io`. Reserve
  `spawn_blocking` for sync-only third-party APIs and CPU-bound work, not I/O. Tests exempt.
- **Never `println!`/`print!` for protocol data on stdout** (SPEC R5.6.1). They panic on write
  errors — a broken pipe on Windows is OS error 232, and in stdio server mode stdout *is* a
  pipe the bridge may close during shutdown. Use `crate::utils::stdio::emit_stdout_notification`,
  which downgrades broken-pipe to `debug` and returns other I/O errors. `println!` is fine in
  CLI mode, where stdout is a terminal.
- **Never commit `*.py`.** Temporary Python for local one-offs is fine — delete it before you
  finish. **Tests must never depend on `python3`**; Python is a worker-synthesis *target*, but
  the suite assumes only a Rust toolchain. Write Rust-native equivalents rather than relying on
  a silent skip that asserts nothing.
- **Errors**: `anyhow::Result` internally, converted to `rmcp::error::McpError` at the MCP
  boundary. Give actionable context ("Install with `cargo install cargo-nextest`").
- **Docs**: don't hardcode directory trees or file lists in markdown — they rot, and agents can
  read the workspace.

## Security invariants

- **Sandbox scope cannot change during a session.** Validate every user-supplied path through
  `path_security`; the kernel enforces the scope.
- **ahma never silently disables enforcement.** Inside a host sandbox (Cursor, VS Code, Docker)
  it picks one authoritative sandbox per execution path and discloses which, loudly (SPEC R7):
  terminal hooks **defer to the host** (override with `AHMA_PREFER_OWN_SANDBOX=1`); the **MCP
  server stays authoritative** because the host does not wrap ahma's own executions, and fails
  loudly if it cannot sandbox. Detecting a host doesn't prove its sandbox is on — the disclosure
  says so.
- **`AHMA_*` configuration env vars are retired and ignored.** Use CLI flags or
  `~/.ahma/settings.toml`. Still live: `AHMA_HOOKS`, `AHMA_DISABLE_HOOKS`,
  `AHMA_PREFER_OWN_SANDBOX`.
- Tool configs are validated against the MTDF schema at startup; `format: "path"` triggers path
  security validation.

### Windows

Job Object enforcement is done; **AppContainer spawn isolation is still pending**, so
out-of-scope writes are not yet OS-blocked on Windows — don't mark R6.3 done until Windows CI
proves it. `red_team_command_write_escape_blocked` is `#[cfg_attr(windows, ignore)]` for exactly
this reason; remove the ignore only when R6.3.3 lands.

- Root checks use `is_filesystem_root()` — never compare to `Path::new("/")`; `C:\` and UNC
  roots differ.
- Shell invocation goes through `platform_shell_program()` (`shell_pool.rs`). Do **not**
  reintroduce `is_shell_program_invocation()` — it caused a double `-c` bug.
- Use `MAIN_SEPARATOR`/`Path` APIs, never string separator assumptions.
- `expand_home` handles both `~/` and `~\` — test both when changing it.

## Commits and PRs

Conventional commits (`feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`). PR titles are
`[<crate>] <description>`. Squash-merge onto `main`; the squash body is built from your commit
messages, so write them for the permanent log.

Before committing: `cargo fmt --all && cargo clippy --all-targets && cargo nextest run`.
Bump versions with `cargo xtask bump-version X.Y.Z` (updates Cargo.toml, Cargo.lock, SKILL.md
and both install scripts together). Optional local guard:
`cp scripts/check-guardrails.sh .git/hooks/pre-push && chmod +x .git/hooks/pre-push`.

## Repo-local skills

`.agents/skills/` is repo-local, not shipped with releases (`.claude/skills/` symlinks into it).
`/ahmadev help` covers dev workflows — land, release, bisect, coverage, dep updates.
`/ahma help` covers ahma's own tooling — sandbox, livelog, `run_terminal_command`.
