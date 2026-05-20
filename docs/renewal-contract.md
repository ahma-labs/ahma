# Renewal Contract for Long-Running Tasks

> **Experimental** — introduced in v0.7.

The renewal contract automatically halts any operation that runs unattended beyond a configurable timeout, writes a checkpoint to the vault, and requires explicit re-approval before the operation can continue. This closes a known vulnerability in cloud agent tools where a long unattended run can be re-steered by a prompt-injection payload in a later input without the user knowing.

## The problem

When a user approves a task at start time, that approval covers the inputs present at that moment. If the task runs for 30 minutes and processes new emails, web pages, or files during that time, the risk surface grows. A successful injection in a later input has far more runway with no one watching.

## How it works

```
Operation starts
      │  running...
      │  running...
      │  <T_renew seconds elapsed without a checkpoint>
      ▼
RenewalWatcher detects deadline exceeded
      │
      ├── write <op_id>.checkpoint.json to vault workdir/
      ├── emit audit.jsonl: renewal_checkpoint event
      ├── cancel CancellationToken (operation halts)
      ├── emit audit.jsonl: task_halted event
      └── signal TUI: ApprovalRequired prompt

User reviews checkpoint and re-approves
      │
      ▼
Operation resumes (new session, fresh approval)
```

## Configuration

The renewal watcher is configured when creating a session with a task vault:

```rust
use ahma_core::{RenewalWatcher, RenewalConfig};
use std::time::Duration;

let watcher = RenewalWatcher::new(RenewalConfig {
    renew_after: Duration::from_secs(300),  // 5 minutes
    checkpoint_dir: vault.workdir.clone(),
})
.with_audit(vault.audit_writer());

// Register operations as they start
watcher.register("op_1", "cargo_build", cancellation_token);

// Call periodically from a background task
let halted = watcher.tick().await;
for event in halted {
    println!("Halted: {} (elapsed {}s)", event.operation_id, event.elapsed_secs);
    // Signal TUI or notify user
}
```

## Checkpoint file

When an operation is halted, a checkpoint is written:

```json
{
  "operation_id": "op_1",
  "tool_name": "cargo_build",
  "elapsed_secs": 312,
  "halted_at": "2026-05-20T12:05:12Z",
  "reason": "Renewal contract exceeded — re-approval required"
}
```

The file lives at `<vault>/workdir/<op_id>.checkpoint.json`.

## Resetting the timer

Operations that produce regular progress can call `checkpoint()` to reset the renewal clock:

```rust
// Called when a meaningful intermediate result is produced
watcher.checkpoint("op_1");
```

This is useful for long-running but actively progressing tasks (large builds, bulk file processing) that genuinely need more than `T_renew` seconds but are making visible progress.

## Audit trail

Every renewal event is recorded in `audit.jsonl`:

```json
{"timestamp":"...","type":"renewal_checkpoint","operation_id":"op_1","elapsed_secs":312,"checkpoint_path":"..."}
{"timestamp":"...","type":"task_halted","operation_id":"op_1","reason":"Renewal contract exceeded — re-approval required"}
```

## Choosing T_renew

| Use case | Suggested value |
|----------|----------------|
| Interactive development tasks | 5 minutes (300s) |
| Build/test pipelines | 15 minutes (900s) |
| Bulk file operations | 10 minutes (600s) |
| Live log monitoring | Disabled (livelog is intentionally long-running) |

Set a value that reflects how long you are comfortable stepping away from a task without reviewing its inputs.

## See also

- [docs/task-vault.md](task-vault.md) — vault layout and audit log
- [docs/tui.md](tui.md) — approval gate interface
- [SPEC.md](../SPEC.md) — renewal contract design notes
