# AGENTS.md

How to work in this repo and project. **What** the product does and **why** lives in [SPEC.md](SPEC.md)
(root) and each crate's own `SPEC.md`. If you need to state a product rule, it goes in a
SPEC, not here and not only in code.

> **A rule that binds one surface binds all of them.** If you write a constraint as a comment
> in the one file that currently obeys it, the next surface to implement the same thing will
> not know about it, and it regresses silently. Promote it to the relevant `SPEC.md` and
> reference the requirement id from the code.

---

## 1. Build, Test, Verify & Development Workflow

### Prerequisites and Toolchain

* Rust 2024 Edition (MSRV in `Cargo.toml`, currently 1.93+).
* Standard cargo commands. Package names use **underscores** (`-p ahma_http_bridge`, not `ahma-http-bridge`).
* Prefer `cargo nextest run` over `cargo test`.
* **One canonical build flavour: default features, whole workspace.** `cargo build`,
  `cargo nextest run`, `cargo clippy --all-targets` and `cargo doc` with no feature flags and no
  `-p` are the only invocations that share artifacts with each other and with CI. `simplify`
  is a default feature of `ahma_bin`, so the workspace build already compiles and tests it.
  Do **not** run `-p <crate> --features …` or `--no-default-features` "to test a flavour":
  every distinct feature set is a separate copy of every affected artifact in `target/`
  (see [Target Directory & Cache Management](#target-directory--cache-management)). To run a
  subset of tests, filter by name (`cargo nextest run -E 'test(livelog)'`) or by binary
  (`--test unit`), never by feature. The lean `--no-default-features` build is proven to
  compile by CI's clippy step, in check mode, which is all it needs.
* **The one sanctioned `-p`: `xtask`.** It is not in `default-members`, so no ordinary
  invocation reaches it — it had zero CI coverage until v0.20.2, and its tests had never
  run outside a developer's machine, despite `cargo xtask bump-version` being the command
  that cuts every release. It is also excluded from cargo-hakari on purpose (its `ureq`
  wants `ring`, which `scripts/check-dependency-graph.sh` bans from the product graph), so
  `--workspace` is the wrong tool. CI now runs `cargo clippy -p xtask --all-targets` and
  `cargo nextest run -p xtask`. Don't make a habit of it locally: on a persistent `target/`
  that invocation forks the dependency resolution and costs a parallel copy of ~45 shared
  crates. On a fresh CI runner the cost is discarded.

```bash
# Update stable toolchain
rustup update stable

# Build workspace release binary
cargo build --release
# The binary is located at target/release/ahma
```

### Definition of Done

Before you claim "all green" and stop work, run:

1. `cargo fmt --all` — format code.
2. `cargo clippy --all-targets` — verify zero warnings or errors.
3. `cargo nextest run` — run all standard tests.
4. `cargo nextest run --workspace --run-ignored all` — ignored tests here are expensive stress/regression coverage and latency guards (`latency_guard_test`), not dead weight; they are part of the required set.
5. `cargo doc --no-deps` — verify docs build.
6. `cargo test --doc` — verify the examples in docs still *compile*. This is a
   separate step because nextest cannot run doctests, so the entire suite and all
   of CI can be green while a `rust` example no longer builds. That is not
   hypothetical: `ahma_mcp`'s headline "Initializing the Engine" example passed a
   stale argument to `Adapter::new` and went unnoticed, because the only runner
   that would have caught it is not the one anyone runs. Cheap (~1s) — it
   compiles examples, it does not rebuild the workspace.
7. **If you touched `Cargo.toml`/`Cargo.lock`:** the *full* `cargo deny check`.
   Not `cargo deny check advisories`. The full check also enforces licences, and a licence is
   what broke main (#470): `zip 8.6`'s default features silently pulled in `bzip2`,
   whose licence is not on the allow-list. A new transitive dep arrives with a licence you did
   not choose, so the advisories subset proves nothing about it.
8. **If you touched any `[dependencies]`/`[dev-dependencies]` section:**
   `cargo hakari generate && cargo hakari manage-deps` — regenerates `workspace-hack/`, the
   crate that pins one third-party feature set for every invocation. `cargo hakari verify`
   runs in `scripts/check-dependency-graph.sh` (called by the pre-push guardrails and by
   Fast Tier CI), so a stale hack fails before it lands. Install once with
   `cargo install cargo-hakari --locked`.
9. **If you added a dependency that touches TLS** (anything with a `rustls`, `rcgen`,
   `quinn` or `*-tls` feature): the same script also checks that `aws-lc-rs` is the *only*
   rustls crypto provider in the product graph. `ring` is a second provider (another
   native build, another copy of every primitive in every binary) and only ever arrives
   through a dependency's *default* features — turn them off, as `ahma_http_bridge/Cargo.toml`
   does for `quinn` and `rcgen`. Never carry both.

If an ignored test can't run (missing platform prerequisite) or is broken, say so in the PR
with a repro — don't quietly drop it.

### Test-First Development (TDD)

All functional changes and bug fixes **must** follow test-first development:

1. **Write the test first** — Write a test that expresses the desired behavior or reproduces the bug.
2. **See it fail** — Run the test and verify it fails for the expected reason.
3. **Implement the fix** — Write the minimal code to make the test pass.
4. **See it pass** — Run the test and verify it passes.
5. **Refactor** — Clean up the code while keeping tests green.

Code changes without corresponding tests **must not** be merged unless:
- The change is purely documentation.
- The change is a trivial typo fix in comments.
- Tests are genuinely impossible (must be justified in code review).

### Core Principle: Dogfood Ahma

**Always use Ahma tools** instead of raw terminal commands when developing:

| Instead of... | Use Ahma tool... |
|---|---|
| `run_in_terminal("cargo build")` | `cargo` with `{"subcommand": "build"}` |
| `run_in_terminal("any command")` | `run_terminal_command` with `{"command": "any command"}` |

**Why**: We dogfood our own product. Using Ahma catches bugs immediately, runs faster (no GUI prompts), and enforces sandbox security.

Use ahma's `run_terminal_command` when the command **writes to disk** (kernel sandbox keeps
writes in-workspace), **runs more than a few seconds** (async, returns an operation_id so you
can work meanwhile), needs **mid-run error watching** (`monitor_level`), or when several
independent commands should run **concurrently**.

Keep read-only inspection (read/grep/glob/find/replace) on native file tools — faster and
cheaper than a round trip through MCP.

**Terminal Fallback (Rare):** Only use terminal directly when:
1. **Coverage**: `cargo llvm-cov` — instrumentation is incompatible with sandboxing.
2. **Ahma completely broken** — fix immediately after recovery.

### Dogfooding `mcp.json` Configuration

When configuring an IDE (VS Code, Cursor, Claude Code, Antigravity) to use the local development build:

```json
{
  "servers": {
    "Ahma": {
      "type": "stdio",
      "cwd": "${workspaceFolder}",
      "command": "/path/to/ahma/target/release/ahma",
      "args": []
    }
  }
}
```

### Target Directory & Cache Management

**Why `target/` grows without bound — read this before adding a feature flag or a test file.**
Cargo hashes *features, every profile setting, compile mode (check/build/test/doc), target
kind and rustc version* into each artifact's file name, and never garbage-collects `target/`.
Every distinct combination is therefore a **parallel copy** that coexists forever with the
others. (`RUSTFLAGS`/`--cfg` are the other failure mode: they are *not* in the file name, so
changing them rebuilds everything **in place** — a time cost rather than a disk cost.) Measured
on this workspace, 2026-09: a clean `cargo build` + `cargo test --no-run` was 9.4 GB; running
`cargo build -p ahma_core`, `-p ahma_common`, `test -p ahma_mcp`, `-p ahma_mcp --features
simplify`, `-p xtask` and `clippy --all-targets` afterwards — no source change — took it to
18 GB, because each `-p` invocation unified third-party features differently and recompiled
70–200 crates into new file names. The three multipliers, and the rule that neutralises each:

| Multiplier | Rule |
|---|---|
| Feature flavours of workspace crates (`--no-default-features`, `--features x`, a no-op feature used as a CI test selector) | **One canonical flavour** (default features) for every build/test/clippy/doc invocation, locally and in CI. Select tests by name/binary, never by feature. |
| Third-party feature unification differing between `-p X` and the workspace build | **`workspace-hack/` (cargo-hakari)** pins the union of features for every invocation. Regenerate after any dependency edit (Definition of Done step 8). |
| One statically-linked executable per `tests/*.rs` file (143 executables = 3.2 GB, each re-linked per flavour, each with its own `incremental/` session) | **One test binary per harness class per crate** (`tests/unit.rs`, `tests/e2e.rs`, …); new integration tests are a `mod` inside one of them, never a new top-level `tests/*.rs` file — enforced by `scripts/check-guardrails.sh`. |

Full analysis, measurements and the industry references behind these rules:
[docs/build-and-test-performance.md](docs/build-and-test-performance.md).

The workspace profile is configured (`[profile.dev]` and `[profile.test]`) to strip dependency
debug symbols (`debug = 0` for `package."*"`) while preserving fast line-table debug info
(`debug = 1`) for workspace code. `dev` and `test` **must stay identical**: a profile
difference is a second copy of every workspace crate (CI sets `CARGO_PROFILE_DEV_DEBUG` and
`CARGO_PROFILE_TEST_DEBUG` to the same value for exactly this reason). Deliberately does
**not** raise dependency `opt-level`: that trades a real, repeated compile-time cost (no
benefit on CI's fresh, non-incremental builds) for a runtime speedup only local dev sessions
actually reuse — it once pushed the Windows CI job over its 40-minute budget.

Machine-specific limits (`[build] jobs = N` for a small ARM board, `RUSTC_WRAPPER = "sccache"`)
belong in that machine's `~/.cargo/config.toml`, never in the repo's `.cargo/config.toml`: a
repo-level `jobs = 4` once capped every 10-core developer machine at 40 % on every compile.
On macOS, add each clone's `target/` to *System Settings → Spotlight → Search Privacy*:
Spotlight re-indexes every rebuilt object file, and during a large build `mds_stores` +
`mdworker` were observed consuming three to four cores.

To keep `target/` bloat bounded during long development sessions:

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

### Feature Documentation Contract (R-DOC)

Every major feature in ahma **must** have a corresponding page in `docs/` and an entry in `README.md`. This applies to both stable and experimental features.

* **R-DOC.1 — Dedicated doc page**: Each major feature **must** have its own `docs/<feature>.md` file with:
  - A clear statement of whether the feature is stable or **Experimental** (version introduced).
  - A motivating "Why" paragraph explaining the security or usability rationale.
  - A practical quickstart with runnable commands or code.
  - A reference table of configuration options where applicable.
  - A "See also" section linking to related docs and the relevant SPEC.md section.
* **R-DOC.2 — README entry**: Each major feature **must** have a brief entry in `README.md` under the appropriate section (stable features) or the "vX.Y Experimental Features" section (new/unstable features). The entry **must** link to the dedicated doc page.
* **R-DOC.3 — SPEC.md accuracy**: When a feature's behaviour is changed, the corresponding SPEC.md section and its `docs/<feature>.md` page **must** be updated in the same commit or PR.
* **R-DOC.4 — Experimental graduation**: When an experimental feature is stabilised, its doc page **must** remove the "Experimental" notice, update SPEC.md status to `tests-pass`, and move its README entry from the "Experimental" section to the appropriate stable section.
* **R-DOC.5 — Removal**: When a feature is removed, its `docs/<feature>.md` **must** be deleted and all README and SPEC.md references **must** be removed in the same commit.
* **R-DOC.6 — No orphan docs**: Every file in `docs/` **must** be referenced from at least one of: `README.md`, `SPEC.md`, or another `docs/*.md` file. Orphan documentation is misleading and should not accumulate.
* **R-DOC.7 — CLI Help Text Guidelines**: Command-line interface help descriptions **must** follow two strict guidelines:
  - **Contiguous Layout**: Descriptions for arguments, flags, and subcommands **must** be written as contiguous blocks of text without blank lines (double carriage returns). Since Clap outputs help text inside lists, internal blank lines disrupt the alignment and layout.
  - **Educational Context**: Help text for complex or non-obvious features (e.g., `--task-vault`) **must** be educational. It must explain what the feature is and why/when a user or tool would use it, while remaining concise and precise.

#### Feature Documentation Map

| Feature area | Stable doc | SPEC.md section |
|---|---|---|
| Kernel sandbox | [docs/security-sandbox.md](docs/security-sandbox.md) | R5, R6 |
| Connection modes | [docs/connection-modes.md](docs/connection-modes.md) | §6 |
| Custom tools / MTDF | [docs/custom-tools.md](docs/custom-tools.md) | §5 |
| Live log monitoring | [docs/live-log-monitoring.md](docs/live-log-monitoring.md) | §5.5 |
| Environment variables | [docs/environment-variables.md](docs/environment-variables.md) | — |
| Installation | [docs/installation.md](docs/installation.md) | — |
| Session isolation | [docs/session-isolation.md](docs/session-isolation.md) | R10 |
| Task vaults | [docs/task-vault.md](docs/task-vault.md) | §5.8 |
| TUI | [docs/tui.md](docs/tui.md) | — |
| Egress sandbox | [docs/egress-sandbox.md](docs/egress-sandbox.md) | — |
| Network egress (subprocess) | [docs/network-egress.md](docs/network-egress.md) | R-WEB.16, R-PERM.5.3 |
| Execution audit log | [docs/execution-audit-log.md](docs/execution-audit-log.md) | R-HANDOFF.10 |
| Artifacts | [docs/artifacts.md](docs/artifacts.md) | — |
| Bundle audit | [docs/bundle-audit.md](docs/bundle-audit.md) | — |
| ahma_core library | [docs/ahma-core-library.md](docs/ahma-core-library.md) | — |

### Maintenance Checklist

When modifying code or discovering issues:
1. Update the "Quick Status" table in [SPEC.md](SPEC.md).
2. Add to "Known Issues" if new bugs are discovered.
3. Update feature tables with status changes.
4. **BEFORE stopping work:** run `cargo fmt --all && cargo clippy --all-targets && cargo nextest run`.
   If you edited a dependency list, also `cargo hakari generate && cargo hakari manage-deps`.
5. `skills/ahma/SKILL.md` is a **living document** — update it in the same PR/commit whenever
   you change: CLI flags or subcommands (`ahma_mcp/src/shell/cli.rs`), environment variables
   (`ahma_mcp/src/config/`), tool bundle names or contents
   (`ahma_mcp/src/mcp_service/bundle_registry.rs`), built-in tool signatures
   (`run_terminal_command`, `status`, `await`, `cancel`), connection modes or HTTP endpoints
   (`ahma_http_bridge/`), sandbox scope semantics (`ahma_core/src/sandbox/`), or live-log
   monitoring configuration.

---

## 2. Testing Philosophy & Test Pyramid

### The Test Pyramid — Read This First

**Prefer in-process tests over subprocess E2E.** This is the single most important rule for CI
stability: GitHub runners have 2 cores, every spawned subprocess burns scheduler slots, and at
20+ parallel tests this causes IPC pipe back-pressure and handshake timeouts.

| Layer | Use for | Helpers | Execution Time |
|---|---|---|---|
| **Unit** (preferred) | Logic, schema gen, config parsing, state machines | Direct API calls, `#[cfg(test)]` | <5 ms |
| **Integration (in-process)** | MCP protocol logic, tool dispatch, path security, async ops | `create_in_process_mcp_from_dir()` / `_with_scope()` | <50 ms |
| **E2E (subprocess)** — sparingly | Binary wiring, CLI flags, cross-binary IPC | `ClientBuilder`, `spawn_http_bridge` | 1–5 s |

Decision rule: *Can this test be written without forking a process?* If yes, do that.

⚠️ **`ClientBuilder`/`Client::start_process*` spawn a subprocess** — tests built on them are E2E. Using them for integration tests causes CI timeouts on 2-CPU runners.

### Test Binary Layout — One Binary per Harness Class

Every top-level `tests/*.rs` file is its own crate and its own statically-linked executable
(~20–60 MB each here, plus an `incremental/` session), re-linked for every build flavour.
With 127 such files the suite was 3.2 GB of executables and ~130 link steps per flavour. The
suite is therefore organised as **a few root files per crate, one per harness class**, and
each former file is a `mod` inside one of them:

| Crate | Root files (`tests/<name>.rs` + `tests/<name>/`) |
|---|---|
| `ahma_mcp` | `unit` (pure logic + in-process MCP), `e2e` (spawns the `ahma` binary; OS-gated sandbox suites live here behind their `#![cfg]`), `latency_guard_test` (all `#[ignore]` benchmarks) |
| `ahma_http_bridge` | `unit` (`SessionManager`/in-process), `e2e` (spawns a bridge server), `stress` (all `#[ignore]`) |
| `ahma_tui` | `connection` |
| `ahma_core`, `ahma_http_mcp_client`, `ahma_llm_monitor`, `ahma_simplify` | `integration` |

Rules:
* **Add a new integration test as `tests/<root>/<topic>.rs` plus a `mod <topic>;` line in the
  matching root file.** Never add a new top-level `tests/*.rs` file; `scripts/check-guardrails.sh`
  rejects one that is not on its allowlist.
* Put it in the binary that matches its harness: if it spawns the `ahma` binary or a bridge
  server it is `e2e`, otherwise `unit`. `.config/nextest.toml` throttles and grants CI retries
  **by binary id** (`binary_id(ahma_mcp::e2e)`, `binary_id(ahma_http_bridge::e2e)`), so a
  subprocess test filed under `unit` runs unthrottled and will flake on 2-CPU runners, and a
  unit test filed under `e2e` is needlessly serialised.
* Shared helpers: `ahma_mcp` keeps them in `src/test_utils` (compiled once into the rlib).
  `ahma_http_bridge`'s `tests/common/` cannot move into `src/` — it uses `ahma_mcp`, which
  depends on `ahma_http_bridge` (a dev-dependency cycle) — but it is now compiled once per root
  binary instead of once per file, which is the same win.
* Under `cargo nextest` every test is its own process, so merging files changes nothing about
  isolation (`env::set_var`, statics, `#[serial]` are all process-local). Plain `cargo test`
  shares a process across the whole binary; that is one more reason it is not the supported
  runner here.
* Test *names* are the stable interface for CI selection (`-E 'test(kotlin) or test(android)'`,
  `test(appcontainer_dacl_diagnostics)`); a file move must not rename a test function.

### Choosing the Right In-Process Helper

Both helpers live in `ahma_mcp::test_utils::in_process`:

| Helper | Sandbox | Use when |
|---|---|---|
| `create_in_process_mcp_from_dir(tools_dir)` | `Sandbox::new(Test)` — **path validation ENFORCED** | Tool dispatch, arg parsing, async lifecycle, schema tests |
| `create_in_process_mcp_with_scope(tools_dir, scopes)` | `Sandbox::new(Strict)` — **path validation ENFORCED** | Tests that assert a path or symlink is **rejected** |

Both helpers strictly enforce path validation since `new_test` and validation bypasses have been removed. Tests must ensure that input files and working directories are correctly scoped.

### Asserting on What the Client Receives

For anything ahma *pushes* — `notifications/progress` above all — assert on the wire, not on the router's bookkeeping:
* `test_utils::recording_client::RecordingClient` is a real `ClientHandler` that records notifications and answers `roots/list`; pass it to `create_in_process_mcp_with_client(client, configs, scopes)`.
* Its `clientInfo.name` is configurable because ahma keys real behaviour off it (`supports_progress`, `request_budget`).
* Passing **empty `scopes`** gives you a server whose sandbox scope never settles — the only way to observe the `tools/call` gate in-process.
* Progress tokens are the *client's* to mint: rmcp assigns one per request and overwrites anything you set, so use `call_tool_observing_token()` to learn the token a request actually carried rather than trying to choose it.

### Test File Isolation (CRITICAL)

* **ALL tests MUST use temporary directories** via the `tempfile` crate.
* **NEVER** create test files directly in repository structure.
* Use `tempfile::tempdir()` or `test_utils::test_project::create_rust_test_project()`. `TempDir` automatically cleans up on drop.

```rust
use tempfile::tempdir;

let temp_dir = tempdir().unwrap();
let test_file = temp_dir.path().join("test.txt");
fs::write(&test_file, "test content").unwrap();
```

### CLI Binary Integration Tests

All binaries (`ahma`, `generate-tool-schema`) **must** have integration tests covering `--help`, `--version`, and basic functionality (e.g. in `ahma_mcp/tests/e2e/cli_binary_integration_test.rs`).

### Centralized Binary Path Resolution (R-TEST-PATH)

All binary path resolution in tests **MUST** use centralized helpers:

* **R-TEST-PATH.1**: Use `ahma_mcp::test_utils::cli::get_binary_path(package, binary)` to get binary paths.
* **R-TEST-PATH.2**: Use `ahma_mcp::test_utils::cli::build_binary_cached(package, binary)` for builds with caching.
* **R-TEST-PATH.3**: **NEVER** manually access `std::env::var("CARGO_TARGET_DIR")` outside of `test_utils::cli`.

**Why**: CI environments may set `CARGO_TARGET_DIR` to relative paths (e.g. `target`). The centralized helpers correctly resolve these relative to the workspace root. Manual path resolution duplicates this logic and inevitably introduces bugs. Enforced by `scripts/lint_test_paths.sh`.

---

## 3. CI-Resilient Testing Patterns & Rules

### Rules That Break CI When Ignored

- **Every test uses `tempfile::tempdir()`.** Never create files in the repo tree.
- **Never hardcode timeouts.** Windows runners are 3–5× slower. Use `ahma_common::timeouts::{TestTimeouts, TimeoutCategory}` — semantic categories (`Handshake`, `ToolCall`, `SandboxReady`, …), `scale_secs()`, `poll_interval()`.
- **Retries are granted by mechanism, not by incident.** A suite gets `retries` on the `ci`/`coverage` profiles when its tests cross a **process or network boundary** and so depend on OS scheduling — today `binary_id(ahma_http_bridge::e2e)`, `binary_id(ahma_http_bridge::stress)`, `binary_id(ahma_mcp::e2e)`, `binary_id(ahma_mcp::latency_guard_test)` (asserts wall-clock budgets), `binary_id(ahma_tui::connection)`. The `unit`/`integration` binaries and lib unit tests never do: an in-process test that flakes is a real bug, and a retry would hide it. Add a new suite by structural filter, and justify it by the boundary it crosses, not by "it failed once". Full rationale in `.config/nextest.toml`; both rules are enforced by `scripts/check-guardrails.sh`.
- **In `.config/nextest.toml`, list narrow overrides above the broad ones they refine.** nextest resolves each setting from the **first** matching override in file order — not the most specific. A `binary_id()` override sitting below the `package()` override it refines is silently dead config.
- **Never hardcode `/tmp`, `/var/folders`, `/dev/null`.** Use `test_utils::path_helpers`: `test_temp_path`, `test_out_of_scope_path`, `test_blocked_device_path`, `test_abs`, `test_root`.
- **Never hardcode `/bin/sh`, `/bin/bash`, or bash redirection** (`>&2`, `2>&1`) in command strings sent through the tool pipeline — on Windows the shell is PowerShell.
- **Never `#[cfg(unix)]` a test that has a cross-platform equivalent.** Gating is correct only when the test genuinely needs a Unix-only API (e.g. `std::os::unix::fs::symlink`).
- **`Path::starts_with` is case-sensitive on Windows** though the filesystem isn't — `dunce::canonicalize` both sides before comparing.
- **Spawned test processes must die with the test.** Set `.process_group(0)` and kill with `kill(-pgid)`, never `child.kill()` alone — that signals one PID and orphans grandchildren. A test that SIGKILLed only its direct shell leaked a 100%-CPU busy loop on every single suite run; CI never noticed because runners are discarded. See `kill_process_tree`.
- **Don't add `test-threads` to `[profile.default]`.** Local runs are meant to use full parallelism; CI throttling belongs in `--profile ci` only.
- **The `skills/ahma/SKILL.md` symlink must resolve at the repo root (R-SK6).** A CI or
  pre-push check **must** assert it:
  ```bash
  # Cross-platform (macOS readlink does not support -f)
  test -L .agents/skills/ahma/SKILL.md && cat .agents/skills/ahma/SKILL.md > /dev/null
  ```
  If the symlink does not exist or does not resolve, the check fails.

### Async Testing & Concurrency Patterns (R15, R16)

* **Avoid Race Conditions in Async Tests (R15.1)**: Never use `tokio::select!` to race response completion against notification reception. When the response branch wins, the transport may already be closing. For stdio MCP tests that verify results, prefer either synchronous execution or the `await` tool.
* **Polling and Timeouts (R15.2)**: Never use fixed `sleep()` to wait for async conditions. Use `wait_for_condition()` or `wait_with_backoff()` from `test_utils`. For health checks and server readiness, poll with increasing backoff. When testing notifications, use channel-based communication with explicit timeouts.
* **Stdio Transport Gotchas (R15.3)**: `OperationMonitor` stores results via a `tokio::sync::watch` channel, making `wait_for_operation()` race-free. Push notifications remain best-effort for progress updates. For notification tests, prefer HTTP mode with SSE.
* **Coverage Overhead Mitigation (R15.4)**: `llvm-cov` instrumentation significantly slows execution (10x–20x), especially for process-heavy tests like stdio integration. Tests involving child processes or networks **must** use generous timeouts (30s+). A 10s timeout working in `release` mode will reliably fail under `coverage`.

#### Concurrent Test Helpers (`test_utils::concurrent_test_helpers`)

```rust
use ahma_mcp::test_utils::concurrent_test_helpers::*;

// Spawn tasks that start simultaneously via barrier
let results = spawn_tasks_with_barrier(5, |task_id| async move {
    perform_operation(task_id).await
}).await;
assert_all_unique(&results);

// Bounded concurrency for resource-limited CI
let results = spawn_bounded_concurrent(items, 4, |item| async move {
    process(item).await
}).await;

// Wrap operations with clear timeout diagnostics
let result = with_ci_timeout(
    "operation completion",
    CI_DEFAULT_TIMEOUT,
    async { monitor.wait_for_operation("op-1").await }
).await?;

// Wait with exponential backoff
wait_with_backoff("server ready", Duration::from_secs(10), || async {
    health_check().await.is_ok()
}).await?;
```

#### Async Assertions (`test_utils::async_assertions`)

```rust
use ahma_mcp::test_utils::async_assertions::*;

// Assert operation completes in time
let result = assert_completes_within(
    Duration::from_secs(5),
    "quick operation",
    async { fetch_data().await }
).await;

// Assert condition becomes true
assert_eventually(
    Duration::from_secs(10),
    Duration::from_millis(100),
    "operation becomes complete",
    || async { monitor.is_complete("op-1").await }
).await;
```

### Platform-Aware Timeouts (R-TIMEOUT)

Windows CI runners are 3–5× slower than Linux/macOS for process spawning, PowerShell startup, filesystem access, and socket operations. Hardcoded timeouts will reliably fail on Windows CI.

The `ahma_common::timeouts` module provides semantic timeout categories with platform-appropriate defaults and multipliers:

```rust
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};

// Semantic categories (Unix base, 4x on Windows, 8x under Coverage)
let timeout = TestTimeouts::get(TimeoutCategory::Handshake);

// Scale custom durations
let custom = TestTimeouts::scale_secs(5);

// Platform-appropriate polling interval (100ms Unix, 500ms Windows)
let interval = TestTimeouts::poll_interval();

// Delays after async operations
let delay = TestTimeouts::short_delay();
```

#### Timeout Categories Reference

| Category | Base (Unix) | Windows (4x) | Coverage Mode (8x) | Purpose |
|---|---|---|---|---|
| `ProcessSpawn` | 30s | 120s | 240s | Binary loading, process startup |
| `Handshake` | 60s | 240s | 480s | MCP initialize + roots exchange |
| `ToolCall` | 30s | 120s | 240s | Individual tool execution |
| `SandboxReady` | 60s | 240s | 480s | Post-roots sandbox activation |
| `HttpRequest` | 30s | 120s | 240s | HTTP request/response cycle |
| `SseStream` | 120s | 480s | 960s | SSE stream operations |
| `HealthCheck` | 15s | 60s | 120s | Server health polling |
| `Cleanup` | 10s | 40s | 80s | Test cleanup operations |
| `Quick` | 5s | 20s | 40s | Sub-second operations |

### Dual-Transport Test Coverage (HTTP Bridge R15.5)

The HTTP bridge exposes a single `/mcp` POST endpoint whose response format is content-negotiated via the `Accept` header:
- `application/json` → Single JSON-RPC response body
- `text/event-stream` → SSE stream: notifications + response event

**Requirement**: Every HTTP bridge test calling `tools/call` or `tools/list` MUST cover BOTH response modes.

**Implementation pattern**: Extract the test body into a shared `async fn run_<case>(mode: TransportMode)` and add `_json` / `_sse` entry points:

```rust
async fn run_my_tool_test(mode: TransportMode) {
    let Some((_server, mcp)) = common::setup_test_mcp(mode).await else { return; };
    // ... assertions ...
}

#[tokio::test]
async fn test_my_tool_json() { run_my_tool_test(TransportMode::Json).await; }

#[tokio::test]
async fn test_my_tool_sse()  { run_my_tool_test(TransportMode::Sse).await; }
```

**Exemptions**: `sse_*` (streaming protocol), `handshake_*` (protocol invariants), and `sandbox_*` (gating rules).

**CI concurrency limit**: Tests using `setup_test_mcp` spawn a server per test. They MUST be listed in the `threads-required = 2` override in `.config/nextest.toml` under `[profile.ci.overrides]`.

### Hard Invariants — Fix the Harness, Don't Weaken the Assertion

* **MCP Streamable HTTP handshake** (E2E HTTP tests only; unit/in-process exempt), in order:
  `initialize` (no session header) → open SSE stream **before** `notifications/initialized` →
  send `notifications/initialized` → answer the server's `roots/list` over SSE with the same id →
  only then `tools/call`.
* **Sandbox gating is observable**: `tools/call` before sandbox lock returns HTTP 409 with JSON-RPC `-32001`. Assert it explicitly.
* **No print-only integration tests**: Printing is fine; asserting on success/failure and key output patterns is mandatory.

### CI Anti-Patterns to Avoid

| Anti-Pattern | Problem | Solution |
|---|---|---|
| `tokio::time::sleep(Duration::from_secs(1))` | Flaky on slow CI runners | Use `wait_for_condition()` or `wait_with_backoff()` |
| `tokio::select!` racing response vs notification | Transport teardown wins | Use synchronous mode or `await` tool |
| `std::fs::create_dir("./test_dir")` | Pollutes repo, conflicts between tests | Use `tempdir()` or `create_rust_test_project()` |
| `Command::new("cargo").arg("build")` | Slow, skips cached binaries | Use `cli::build_binary_cached()` |
| Spawning 100+ concurrent tasks | OOM on CI, thread exhaustion | Use `spawn_bounded_concurrent()` |
| Expecting notification order | Async execution order is undefined | Collect notifications, assert set membership |
| Hard-coded ports | Port conflicts with parallel tests | Use port 0 for auto-assignment |
| Shared mutable state without locks | Data races under concurrent tests | Use `Arc<Mutex<_>>` or channels |
| Literal `Duration::from_secs(10)` in tests | Breaks on Windows/Coverage CI | Use `TestTimeouts::get(Category)` |

#### Concrete Anti-Pattern Examples

```rust
// WRONG: Fixed sleep for operation completion
async fn test_operation_completes() {
    let op_id = start_operation().await;
    tokio::time::sleep(Duration::from_secs(2)).await; // Flaky!
    assert!(is_complete(&op_id));
}

// CORRECT: Condition-based waiting with backoff
async fn test_operation_completes() {
    let op_id = start_operation().await;
    wait_with_backoff("operation complete", Duration::from_secs(10), || async {
        is_complete(&op_id).await
    }).await.unwrap();
}

// WRONG: Creating files directly in repo
let f = File::create("test.txt");

// CORRECT: Using temp directory
let t = tempdir().unwrap();
let f = File::create(t.path().join("test.txt"));
```

### Reproducing Failures

Capture full logs (`<cmd> 2>&1 | tee …`) and reduce concurrency to a single test (`RUST_TEST_THREADS=1 … --no-capture`) with narrow filters so only the failure prints.

---

## 4. Code Conventions & Canonical Reuse Patterns

### Code Conventions That Differ From Defaults

- **Never `std::fs` or blocking I/O in an async fn** — use `tokio::fs`/`tokio::io`. Reserve `spawn_blocking` for sync-only third-party APIs and CPU-bound work, not I/O. Tests exempt.
- **Never `println!`/`print!` for protocol data on stdout** (SPEC R5.6.1). They panic on write errors — a broken pipe on Windows is OS error 232, and in stdio server mode stdout *is* a pipe the bridge may close during shutdown. Use `crate::utils::stdio::emit_stdout_notification`, which downgrades broken-pipe to `debug` and returns other I/O errors. `println!` is fine in CLI mode, where stdout is a terminal.
- **Never commit `*.py`.** Temporary Python for local one-offs is fine — delete it before you finish. **Tests must never depend on `python3`**; Python is a worker-synthesis *target*, but the suite assumes only a Rust toolchain. Write Rust-native equivalents rather than relying on a silent skip that asserts nothing.
- **Errors**: `anyhow::Result` internally, converted to `rmcp::error::McpError` at the MCP boundary. Give actionable context ("Install with `cargo install cargo-nextest`").
- **Docs**: Don't hardcode directory trees or file lists in markdown — they rot, and agents can read the workspace.

### Production Helper Patterns (R-HELPER)

- **R-HELPER.1**: MCP handlers that return a single text response **should** use `mcp_service::handlers::common::text_result(...)` instead of inlining `CallToolResult::success(vec![Content::text(...)])`.
- **R-HELPER.2**: Common MCP error constructors without extra data **should** use `mcp_service::handlers::common::{mcp_internal, mcp_invalid_params}`.
- **R-HELPER.3**: JSON argument extraction in MCP handlers **should** use `mcp_service::handlers::common::{require_str, opt_str}` where applicable.
- **R-HELPER.4**: Tool-call readiness checks **must** use `sandbox::Sandbox::is_ready_for_tool_calls()` instead of duplicating `scopes().is_empty() && !is_test_mode()` checks.
- **R-HELPER.5**: Built-in tool input schemas (`await`, `status`, `run_terminal_command`) **must** be generated with `mcp_service::schema` helper builders (`string_property`, `path_property`, enum helpers, `object_input_schema`).

### Test Harness Reuse Patterns (R-HARNESS)

- **R-HARNESS.1**: HTTP bridge tool tests **should** use `tests/common/setup_test_mcp_for_tools(...)` for setup + required-tool gating, rather than open-coding availability checks.
- **R-HARNESS.2**: Reusable assertions in HTTP bridge tests **should** use `tests/common/assert_tool_success_with_output(...)` where output is required.
- **R-HARNESS.3**: Tests that need tempdir + `.ahma` tools dir + MCP client **should** use `ahma_mcp::test_utils::client::McpClientFixture`.
- **R-HARNESS.4**: Integration tests with custom bridge startup parameters **should** use `tests/common/server::spawn_server_guard_with_config(...)` instead of duplicating process startup/port/health polling code.
- **R-HARNESS.5**: Timeout values in integration tests **must** use `TestTimeouts` categories or scaling helpers.

### Guardrail Enforcement (R-GUARD)

- **R-GUARD.1**: Pre-commit and pre-push guardrail script: `./scripts/check-guardrails.sh`.
- **R-GUARD.2**: Guardrail scripts **must** reject newly added literal `Duration::from_secs(...)` / `Duration::from_millis(...)` patterns in timeout-sensitive handshake/bridge integration tests (`scripts/lint_test_paths.sh`).
- **R-GUARD.3**: Guardrail scripts **should** verify that custom HTTP bridge integration tests use shared startup helpers from `tests/common/server.rs`.
- **R-GUARD.4**: Guardrail scripts **must** run `scripts/check-dependency-graph.sh` (`cargo hakari verify` plus the single-crypto-provider check), so a dependency edit that was not followed by `cargo hakari generate && cargo hakari manage-deps`, or that drags `ring` back in through a default feature, fails before push. Fast Tier CI runs the same script on every PR.
- **R-GUARD.5**: Guardrail scripts **must** reject a new top-level `tests/*.rs` file that is not on the per-crate root-binary allowlist (see [Test Binary Layout](#test-binary-layout--one-binary-per-harness-class)).

---

## 5. Security Invariants & Platform Specifics

### Security Invariants

- **Sandbox scope cannot change during a session.** Validate every user-supplied path through `path_security`; the kernel enforces the scope.
- **ahma never silently disables enforcement.** Inside a host sandbox (Cursor, VS Code, Docker) it picks one authoritative sandbox per execution path and discloses which, loudly (SPEC R7): terminal hooks **defer to the host** (override with `AHMA_PREFER_OWN_SANDBOX=1`); the **MCP server stays authoritative** because the host does not wrap ahma's own executions, and fails loudly if it cannot sandbox. Detecting a host doesn't prove its sandbox is on — the disclosure says so.
- **`AHMA_*` configuration env vars are retired and ignored.** Use CLI flags or `~/.ahma/settings.toml`. Still live: `AHMA_HOOKS`, `AHMA_DISABLE_HOOKS`, `AHMA_PREFER_OWN_SANDBOX`.
- Tool configs are validated against the MTDF schema at startup; `format: "path"` triggers path security validation.

### Windows

Job Object enforcement is done. **AppContainer spawn isolation is written, was executed on `windows-latest`, and was disproved**: the scoped grant does not take effect, so a write *inside* the locked scope is denied along with one outside it. It is switched off — see `sandbox::windows::appcontainer_spawn_enabled`, the single place that verdict lives — so Windows currently has no OS filesystem boundary in either direction. Don't mark R6.3 done until a `windows-latest` run shows the boundary holding *both* ways; "blocks everything" is the failure mode, not the goal. `red_team_command_write_escape_blocked` is `#[cfg_attr(windows, ignore)]` for exactly this reason.

No root cause is known, and the only artefact so far is `Access to the path '...' is denied`, which names no path. Read the `AppContainer diagnostics` step's output (`appcontainer_dacl_diagnostics`, run on every Windows CI leg) before changing anything in `sandbox/windows.rs` — it dumps `icacls` for the scope and each ancestor, the container SID, and the child's own token groups.

- Root checks use `is_filesystem_root()` — never compare to `Path::new("/")`; `C:\` and UNC roots differ.
- Shell invocation goes through `platform_shell_program()` (`shell_pool.rs`). Do **not** reintroduce `is_shell_program_invocation()` — it caused a double `-c` bug.
- Use `MAIN_SEPARATOR`/`Path` APIs, never string separator assumptions.
- `expand_home` handles both `~/` and `~\` — test both when changing it.

---

## 6. CI Infrastructure & Caching Strategy

To maintain fast CI runtimes and prevent unbounded cache bloat across GitHub Actions:

### Daily Cache Rotation
- All caches **must** use a daily rotating key (e.g., `...-day${{ steps.day-number.outputs.day }}`) to ensure they contain only current files and do not grow indefinitely.
- `restore-keys` **must** fall back to the most recent previous cache (from earlier in the day or a previous day).

### Distributed Caching (sccache)
- **sccache** is used across all macOS and Linux CI jobs (Windows CI is exempt and relies on plain Cargo target caching).
- Uses the **GitHub Actions Backend** (`SCCACHE_GHA_ENABLED: "true"`) for atomic uploads of object files directly to the GHA cache API.
- Each CI job **must** use unique `SCCACHE_GHA_CACHE_TO` keys (`sccache-{OS}-{ARCH}-{JOB}-day{DAY}`) to prevent concurrent write conflicts.
- Each CI job **must** use `SCCACHE_GHA_CACHE_FROM` with comma-separated fallbacks. Debug-profile jobs (clippy, nextest, android, coverage) share cache; release-profile jobs must not include debug caches in `CACHE_FROM`.
- **Temporary note**: CI currently builds `sccache` from an experimental fork (`paulirotta/sccache@gha-retry-layer`) to validate a retry layer for upstream [mozilla/sccache#2821](https://github.com/mozilla/sccache/issues/2821). Reverts to stable once upstream lands or by January 2027.

### Cargo Registry & Action Versioning
- Cargo registry (`~/.cargo/registry`) and git database (`~/.cargo/git`) are cached with daily rotation.
- GitHub Actions **must** be referenced by version tags (`@v6`, `@v5`) rather than full commit hashes, targeting the latest major versions.

---

## 7. Commits, PRs & Release Workflows

Conventional commits (`feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`). PR titles are `[<crate>] <description>`. Squash-merge onto `main`; the squash body is built from your commit messages, so write them for the permanent log.

Before committing: `cargo fmt --all && cargo clippy --all-targets && cargo nextest run`.

Bump versions with `cargo xtask bump-version X.Y.Z` — updates every version-bearing file in one
step (see `xtask/SPEC.md` for the full list and its idempotence/atomicity guarantees). **The
version bump is what triggers a release**: a push to `main` publishes GitHub Release
`v<X.Y.Z>` only when it bumps to a version whose tag does not yet exist; an ordinary land that
doesn't change the version runs the test matrix but publishes nothing.

Install local git guardrail hook:
```bash
cp scripts/check-guardrails.sh .git/hooks/pre-push && chmod +x .git/hooks/pre-push
```

### Repo-Local Skills

`.agents/skills/` is repo-local, not shipped with releases (`.claude/skills/` symlinks into it).
- `/ahmadev help` covers dev workflows — land, release, bisect, coverage, dep updates.
- `/ahma help` covers ahma's own tooling — sandbox, livelog, `run_terminal_command`.
- **No duplication between the skill and this file (R-SK7)**: `skills/ahma/SKILL.md` targets
  **AI agents using Ahma**; `AGENTS.md` targets **AI contributors developing Ahma**. Don't
  copy developer-only content (testing rules, cross-platform checklist, commit format) into
  the skill, and don't copy agent usage recipes into `AGENTS.md`.
