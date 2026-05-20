# Task Vaults

> **Experimental** — introduced in v0.7. API and directory layout may change before stabilisation.

A **task vault** is a per-question isolated working directory that promotes the "dedicated folder per task" security principle from user discipline to a kernel-enforced guarantee. Every vault gets its own sandbox scope, two-phase trash, and append-only audit log.

## Why task vaults?

Cloud agent tools give users guidance like _"create a per-task folder; never grant your whole Documents tree."_ That works, but it relies on users remembering every time. Ahma vaults invert the default: there is no way to start a session that accesses arbitrary paths — the only scope available is the vault's `workdir/`.

This closes the most common practical failure mode: an agent granted convenient broad access running amok because the user accepted a wide scope for comfort.

## Directory layout

```
~/.ahma/tasks/<utc-date>-<slug>-<hex>/
  inputs/        — copies of user-provided files (read intent; originals untouched)
  workdir/       — kernel sandbox scope root for all agent commands
  outputs/       — artifacts produced by tools (HTML reports, CSV exports, etc.)
  trash/         — staged-deletion holding area (two-phase delete)
  audit.jsonl    — append-only JSONL event log
  egress.allowlist  — per-task outbound network domain allowlist
```

## Quickstart

```bash
# Create a vault and note its path
VAULT=$(ahma vault create summarise-q4-report)
echo $VAULT
# ~/.ahma/tasks/20260520T120000Z-summarise-q4-report-a1b2c3d4/

# Start an ahma stdio server scoped to that vault
ahma serve stdio --task-vault "$VAULT"

# Or HTTP bridge
ahma serve http --task-vault "$VAULT"

# List all existing vaults
ahma vault list
```

## Two-phase delete

The AI cannot permanently delete a file in a single step.

1. **Stage** — the file moves to `trash/<timestamp>_<filename>`; the original location is immediately empty, but the data survives.
2. **Review** — an agent or human can list what is staged before committing.
3. **Purge** — only after explicit per-batch confirmation does `purge()` permanently remove staged entries.

This limits the blast radius of a confused deletion command to a recoverable staging operation.

## Audit log

Every significant action inside a vault is recorded in `audit.jsonl` as a single JSON line:

```json
{"timestamp":"2026-05-20T12:00:01Z","type":"vault_created","vault_path":"...","slug":"summarise-q4-report"}
{"timestamp":"2026-05-20T12:00:04Z","type":"tool_call","operation_id":"op_1","tool_name":"cargo_build","args_summary":"--release"}
{"timestamp":"2026-05-20T12:00:07Z","type":"tool_complete","operation_id":"op_1","success":true,"duration_ms":3200}
{"timestamp":"2026-05-20T12:00:08Z","type":"artifact_written","path":"outputs/result.html","size_bytes":4096}
```

The log is append-only. Individual events cannot be silently removed after the fact.

Event kinds: `vault_created`, `tool_call`, `tool_complete`, `artifact_written`, `file_staged`, `trash_purged`, `elevation_requested`, `sub_task_dispatched`, `sub_task_completed`, `renewal_checkpoint`, `task_halted`, `worker_executed`, `egress_decision`.

## Security properties

| Property | Detail |
|----------|--------|
| Kernel-enforced scope | Sandbox scope is `workdir/` — writes outside it are OS-rejected |
| Inputs are copies | Agent never touches originals — only copies in `inputs/` |
| Two-phase delete | `trash/` holds staged deletions; permanent removal requires explicit confirmation |
| Append-only audit | `audit.jsonl` records every operation; events cannot be silently deleted |
| Network allowlist | `egress.allowlist` controls outbound connections — default deny-all |

## Embedding in Rust

The `ahma_core` crate exposes vault primitives directly:

```rust
use ahma_core::{TaskVault, AuditWriter, TrashManager};

let vault = TaskVault::create("my-task")?;
let audit = vault.audit_writer();
audit.vault_created(&vault.path().display().to_string(), "my-task").await?;
```

## See also

- [docs/security-sandbox.md](security-sandbox.md) — kernel sandbox and egress proxy details
- [docs/egress-sandbox.md](egress-sandbox.md) — per-task network allowlist
- [docs/renewal-contract.md](renewal-contract.md) — automatic halt for unattended sessions
- [SPEC.md §5.8](../SPEC.md) — MTDF vault integration specification
