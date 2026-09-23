# ahma_mcp Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common` and the feature crates `ahma_bundle`, `ahma_harness_guard`,
  `ahma_harness_tools`, `ahma_http_bridge`, `ahma_http_mcp_client`, `ahma_llm_monitor`,
  `ahma_log_monitor`, `ahma_output_optimizer`, `ahma_update`, `ahma_vault`
* **Used by**: `ahma_core`, `ahma_tui`, `ahma_bin`, `generate_tool_schema`

## 1. User Story / Problem Statement

*As an AI coding agent, I want to run a project's real command-line tools through MCP,
confined to the project by the kernel and tracked as operations I can watch and cancel, so
that I get terminal-grade capability without terminal-grade risk.*

This crate is the engine behind every ahma surface. It owns:

- tool definitions (MTDF), loaded once and validated against the schema;
- sandboxed execution (`sandbox`, `adapter`) and operation tracking (`operation_monitor`);
- the MCP service and its built-in tools (`mcp_service`, `builtin_tool`);
- the `ahma` command line itself (`shell::cli`, every subcommand and server mode), plus
  `setup`, `uninstall`, terminal `hooks`, `update` orchestration and the daemon reporter.

`ahma_bin` is a thin `main` over `shell::cli`. The root [SPEC.md](../SPEC.md) states the
product rules this crate implements; the list below is what this crate must guarantee.

## 2. Acceptance Criteria

**Tools and configuration**
- Loads MTDF tool definitions from `--tools-dir` (and, once, from a connecting client's
  `<root>/.ahma/`) and validates them against the schema at startup (R1, R4).
- Definitions are **never re-read** while the server runs. The tools directory is
  agent-writable, and MTDF `command` is free-form, so reloading would let a sandboxed agent
  repoint an approved tool at an arbitrary command. The deliberate reload path is the
  `restart` built-in (R1.4, R-HANDOFF.7).
- Built-in tool names are declared once in `builtin_tool::BuiltinTool` and are reserved:
  an MTDF config may not reuse one (R1.5). Every classification of a built-in (sandbox
  gate, agent loop, harness file tool) is an exhaustive `match`, so adding a tool forces
  each decision.
- CLI configuration resolves once into an immutable `shell::cli::AppConfig` (R-CFG4).
  Retired `AHMA_*` variables are warned about and ignored; `--no-sandbox` and
  `--insecure-skip-verify` are CLI-flag-only — a settings file cannot set them (R-CFG2.3).

**Execution**
- Every call is an operation in `OperationMonitor`, in both execution modes. `sync` (the
  default) waits for the result within the client's request budget; `async` returns the
  operation id after a short window for collection with `await` (R2.1, R2.6).
- `OperationMonitor` is the single lifecycle emitter onto the one `OperationEvent` stream
  (`ahma_common::event_dispatcher`) and emits exactly one terminal event per operation.
  Subscribers — MCP progress push (`mcp_service::progress_push`), daemon hub, audit, TUI —
  only consume it (root SPEC §2.4).
- The complete redacted output of every operation is written to
  `<project log dir>/operations/<id>.log` and advertised as `output_file`; the inline result
  window is bounded.
- Every command runs inside the platform sandbox (Landlock, Seatbelt; Job Objects on
  Windows) with the session's locked scope, or not at all (R5–R7). Spawned children use
  `kill_on_drop(true)` and die with their process group (R-PROC).
- Every execution path (sync, async, PTY, session) appends to the execution audit log,
  `<log dir>/audit.jsonl`, before spawning and once on completion; a write failure warns
  and never fails the operation (R-HANDOFF.10).
- With `--task-vault`, an `rm` issued through `run_terminal_command` or an `rm` MTDF tool
  stages its targets into the vault's `trash/` instead of deleting them.

**Built-in tools** (the full set is `BuiltinTool::ALL`)
- Always present: `status`, `await`, `cancel`, `run_terminal_command`, `restart`,
  `sandbox_grant`, `logs_list`/`logs_read`/`logs_search`/`logs_approve`, `log_monitor`,
  `fetch_webpage`, `agent`.
- Harness file tools — `read_file`, `write_file`, `replace_in_file`, `multi_edit`,
  `apply_patch`, `list_dir`, `file_search`, `grep_search`, `todo_write` — are **withheld
  from clients that ship native equivalents** (`client_type::has_native_file_tools`), so a
  client never has two ways to edit a file (R26).
- `status`, `await`, `cancel`, `sandbox_grant`, `restart` and `todo_write` may run before
  the sandbox scope settles; every other tool waits for it (R5.1.2).
- `agent` is not offered inside ahma's own agent loop, so a run cannot recurse into itself.
- Tool-call self-correction (`ahma_harness_guard`: tool-name and argument healing,
  failure-loop detection) is always on.

**Supply chain**
- `ahma bundle audit|checksum|verify` expose `ahma_bundle`. The checksum manifest detects
  corruption only; it is unsigned and **must not** be described as a signature anywhere.

## 3. Non-Functional Requirements

- **Latency**: trivial async dispatch (start → terminal event) stays well under 1 s
  (~6 ms median today). Guarded by the ignored `latency_guard_test` benchmarks.
- **Streaming cost**: per-line handling (redaction, bounded collection, spill, events)
  handles 5 000 lines in under 10 s (~37 ms today).
- **Path security**: every user-supplied path is validated through `path_security`
  (`dunce::canonicalize`, no traversal or symlink escape) before the kernel ever sees it.
- **Async hygiene**: no blocking I/O in async code (AGENTS.md).
- **Protocol stdout**: nothing on the protocol path uses `println!` (R5.6.1).

## 4. Out of Scope

- Serving MCP over a network socket — that is `ahma_http_bridge`, which this crate embeds
  for `ahma serve http|unix`.
- The TUI and the chat agent loop (`ahma_tui`, `ahma_core`).
- Server-side output minimization. `--minimize-tokens` affects only the TUI chat prompt;
  `ahma_output_optimizer`'s streaming stage is present in the adapter but never enabled.
