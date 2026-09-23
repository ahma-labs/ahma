# ahma_vault Crate Specification

* **Status**: Approved (Experimental feature)
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_mcp` (`--task-vault`, re-exported as `ahma_mcp::vault`)

## 1. User Story / Problem Statement

*As a user giving an agent one task, I want that task confined to its own directory with
recoverable deletes and a record of what ran, so that the smallest safe scope is also the
easy one.* User guide: [docs/task-vault.md](../docs/task-vault.md).

## 2. Acceptance Criteria

- `TaskVault::create_at(root)` creates (or reuses, never overwriting the audit log) the
  layout `inputs/`, `workdir/`, `outputs/`, `trash/`, `audit.jsonl`. `TaskVault::create`
  picks `~/.ahma/tasks/<utc>-<slug>-<hex>/`.
- With `--task-vault <root>`, `workdir/` is the primary sandbox scope, and `trash/` and
  `audit.jsonl` are the only other writable paths (`ahma_mcp::shell::cli`).
- `rm_interceptor::RmInterceptor` recognises `rm` commands and moves their targets into
  `trash/` instead of deleting them; `trash::TrashManager` stages, lists and purges.
- `audit::AuditEvent` is the append-only JSONL wire format. The execution audit log
  (R-HANDOFF.10) uses the same format field for field.

## 3. Non-Functional Requirements

- Append-only: nothing in ahma rewrites or truncates `audit.jsonl`.

## 4. Out of Scope

- A vault management CLI, and per-vault network policy. Egress is governed by
  `--restrict-network` regardless of vault mode.
