# Task Vaults

> **Experimental.** A vault is created and used through one flag, `--task-vault`; there is
> no management CLI yet, and the layout may change.

A **task vault** is a per-task working directory that ahma uses as the *entire* sandbox
scope. It turns the advice "give the agent a dedicated folder per task, never your whole
home directory" into something the kernel enforces.

## Why task vaults?

The safest scope is the smallest one, but people widen scopes for convenience. With
`--task-vault` there is no wider scope to accept: the agent can write only inside the
vault's `workdir/` (plus its own `trash/` and `audit.jsonl`), and deletions are recoverable.

## Quickstart

```bash
ahma serve stdio --task-vault ~/.ahma/tasks/summarise-q4-report
```

The directory is created with the layout below if it does not exist, and reused if it
does. `[sandbox] task_vault` in `settings.toml` sets the same thing. The vault takes
precedence over every other scope source (`--sandbox-scope`, `roots/list`).

## Directory layout

```
<vault>/
  inputs/       — copies of user-provided files (originals stay untouched)
  workdir/      — the sandbox scope; commands run here
  outputs/      — artifacts produced by tools
  trash/        — staged deletions
  audit.jsonl   — append-only event log
```

## Recoverable deletes

In vault mode, an `rm` issued through `run_terminal_command`, or through an MTDF tool whose
command is `rm`, does not delete: its targets are **moved** to `trash/<timestamp>_<name>`
and a `file_staged` event is appended to `audit.jsonl`. Nothing in ahma purges `trash/`;
you empty it yourself after review.

## Audit log

In vault mode, `audit.jsonl` receives one JSON line per event — `tool_call` and
`tool_complete` for each tool call, `file_staged` for each staged deletion:

```json
{"timestamp":"…","type":"tool_call","operation_id":"op_1","tool_name":"cargo_build","args_summary":"--release"}
{"timestamp":"…","type":"tool_complete","operation_id":"op_1","success":true,"duration_ms":3200}
{"timestamp":"…","type":"file_staged","original_path":"…","trash_path":"…"}
```

The same schema (`ahma_vault::audit::AuditEvent`) is the wire format of the
[execution audit log](execution-audit-log.md), which records every tool call under the
project log directory whether or not a vault is in use.

## Security properties

| Property | Detail |
|----------|--------|
| Kernel-enforced scope | Writes outside `workdir/`, `trash/` and `audit.jsonl` are OS-rejected on Linux and macOS; Windows has no OS filesystem boundary yet (SPEC R6.3). Reads are confined only on Linux (SPEC R6.2.2) |
| Recoverable deletes | `rm` targets are staged in `trash/`, never unlinked |
| Network | Unchanged by vault mode. Use [`--restrict-network`](network-egress.md) to gate egress |

## Embedding in Rust

```rust
use ahma_vault::TaskVault;

let vault = TaskVault::create_at("/path/to/vault".into())?;
println!("sandbox scope: {}", vault.sandbox_scope().display());
# Ok::<(), anyhow::Error>(())
```

## See also

- [security-sandbox.md](security-sandbox.md) — the kernel sandbox the vault scope feeds
- [network-egress.md](network-egress.md) — restricting outbound network access
- [execution-audit-log.md](execution-audit-log.md) — the always-on audit log
