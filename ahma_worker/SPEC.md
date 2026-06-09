# ahma_worker Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As an agent performing custom data manipulations or complex calculations, I want to compile and execute synthesized code (Rust or Python) in the sandbox so that calculations are fast, deterministic, and isolated.*

## 2. Acceptance Criteria

- **Synthesized Execution**: Compiles and runs Rust code (via `rustc`) or Python code (via `python3`).
- **Sandbox Isolation**: Code executes strictly inside the task vault's `workdir/` directory under kernel sandbox rules.
- **Audit Logging**: Saves a SHA-256 digest of the synthesized code into the vault's `audit.jsonl` log.
- **Ephemeral Cleanups**: Automatically deletes the synthesized source file after execution unless `keep_source: true` is configured.
- **Execution Timeout**: Enforces a configurable execution timeout (default 60 seconds).

## 3. Non-Functional Requirements

- **No Interactive Execution**: Synthesized programs must be deterministic and run completely non-interactively.
- **Error Propagation**: Standard output, standard error, and exit codes must be captured and returned to the parent.

## 4. Out of Scope

- Exposing code compilation as a public web API.
