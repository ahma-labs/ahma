# Ephemeral Worker Code Synthesis

> **Experimental** — introduced in v0.7.

The `worker` tool type compiles and runs synthesized Rust or Python programs inside a vault's sandbox. Because the synthesized program is deterministic compiled code — not an LLM in the execution loop — it cannot be re-injected mid-run.

## Why worker synthesis?

An AI agent running shell commands with `run_terminal_command` is safe at the filesystem level, but the command string itself is constructed by an LLM. A successful prompt-injection could alter the command at the last moment.

A worker sidesteps this: the agent generates the source code once, the code is reviewed (optionally) and then compiled or interpreted as a fixed program. Execution does not involve the LLM; the sandbox enforces filesystem boundaries; the source hash is recorded in `audit.jsonl`.

## Supported languages

| Language | Requirement | Compilation |
|----------|-------------|-------------|
| `rust` | `rustc` on PATH | `rustc -o <bin> <src>.rs` then runs the binary |
| `python` | `python3` on PATH | Runs directly with `python3 <src>.py` |

## MTDF configuration

```json
{
    "name": "file_renamer",
    "description": "Run a synthesized Rust program to rename files according to a rule.",
    "command": "worker",
    "tool_type": "worker",
    "enabled": true,
    "worker": {
        "language": "rust",
        "keep_source": false,
        "timeout_seconds": 30
    }
}
```

## WorkerConfig fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `language` | No | `rust` | `"rust"` or `"python"` |
| `extra_args` | No | `[]` | Additional compiler/interpreter flags |
| `keep_source` | No | `false` | Retain the synthesized source file after execution |
| `timeout_seconds` | No | `60` | Maximum execution time |

## Security model

- The worker process runs inside the vault's `workdir/` kernel sandbox scope (Landlock / Seatbelt).
- The source file is written to a temp path inside `workdir/` with a content-hash name, compiled or executed, and then deleted unless `keep_source: true`.
- The source hash (djb2) is recorded in `audit.jsonl` under a `worker_executed` event, providing a content fingerprint without storing the source itself.

## Lifecycle

```
agent generates source code
         │
         ▼
  write to workdir/<hash>.rs (or .py)
         │
         ▼
  compile (Rust) or skip (Python)
         │
         ▼
  execute inside kernel sandbox
         │
         ▼
  capture stdout+stderr → outputs/
         │
         ▼
  delete source (unless keep_source: true)
         │
         ▼
  record audit.jsonl: worker_executed
```

## Using via ahma_core

```rust
use ahma_core::{WorkerRunner, WorkerConfig, WorkerLanguage};

let runner = WorkerRunner::new(
    WorkerConfig {
        language: WorkerLanguage::Python,
        keep_source: false,
        timeout_seconds: Some(15),
        extra_args: None,
    },
    vault.workdir.clone(),
);

let result = runner.run(r#"
import os
for f in os.listdir('.'):
    print(f)
"#).await?;

println!("exit: {} output: {}", result.exit_code, result.output);
```

## See also

- [docs/task-vault.md](task-vault.md) — where workers run and outputs are stored
- [docs/security-sandbox.md](security-sandbox.md) — kernel sandbox details
- [SPEC.md §5.7](../SPEC.md) — worker tool type specification
