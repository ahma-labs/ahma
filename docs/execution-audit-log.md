# Execution audit log

ahma writes an append-only record of every command it runs, and of every write it
makes that hands execution to something outside the sandbox.

```text
<project log dir>/audit.jsonl
```

That is the sibling of `operations/`, so the audit log inherits the same
per-project isolation and the same one-time "ahma is writing logs here"
disclosure. `ahma serve` prints the directory on startup; `--log-dir` and the
`log_dir` setting move both together.

## Why it exists

Per-operation output already lands in `<project log dir>/operations/<id>.log`.
Output is not provenance. It tells you what a command printed — not that the
command happened, when, in which directory, what it actually was after argument
construction, or what it wrote that a trusted component will execute later.

The question this answers is the fifth one the Pillar Security trust-handoff
research puts to anyone running agents: *when trusted components execute
agent-influenced content, is it auditable?* For a task vault the answer was
already yes. For `run_terminal_command` — the overwhelmingly common path — it was
no.

## Format

One JSON object per line, `type`-tagged, with an RFC 3339 `timestamp`. It is the
same envelope and the same field names the task-vault audit log uses, so a single
reader parses both.

### `tool_call`

Written **before** the process is spawned. A crash, a kill, a hang, or a machine
losing power still leaves this record — a `tool_call` with no matching
`tool_complete` is evidence, not a gap.

```json
{
  "timestamp": "2026-08-07T18:22:41.113+00:00",
  "type": "tool_call",
  "operation_id": "op_41_cargo",
  "tool_name": "run_terminal_command",
  "args_summary": "{\"command\":\"cargo nextest run\"}",
  "working_dir": "/Users/me/github/project",
  "command": "/bin/zsh -c cargo nextest run"
}
```

`command` is the command **as it will actually run** — after subcommand aliasing,
argument construction, and shell selection — not the request that produced it.

### `tool_complete`

Written on every terminal path: exit, failure, timeout, cancellation.

```json
{
  "timestamp": "2026-08-07T18:23:09.884+00:00",
  "type": "tool_complete",
  "operation_id": "op_41_cargo",
  "success": false,
  "duration_ms": 28771,
  "exit_code": 101,
  "outcome": "failed"
}
```

`outcome` is one of `completed`, `failed`, `timed_out`, `cancelled`. `exit_code`
is absent when the process was killed rather than allowed to exit.

### `trust_handoff_disclosure`

The highest-value entry in the log. ahma's `Disclose` tier (SPEC R-HANDOFF) lets
the agent write files users genuinely ask it to edit — `.vscode/tasks.json`,
`.git/config`, harness configuration — and warns in the tool result. That warning
is transient, and the whole shape of a trust-handoff attack is that execution
happens *later*, when nobody is looking at the transcript any more. This is the
durable half.

```json
{
  "timestamp": "2026-08-07T18:25:02.401+00:00",
  "type": "trust_handoff_disclosure",
  "path": ".vscode/tasks.json",
  "trigger": "VS Code runs `folderOpen` tasks automatically when the folder is opened",
  "tool_name": "write_file"
}
```

Recorded only after the bytes reach disk. A refused (`DenyWrite`) or failed write
produces no entry — the log must never report a handoff that did not happen.

### `sandbox_denial`

The durable copy of the structured `sandbox_denial` payload (SPEC R5.4.7): a
working directory rejected up front, or an out-of-scope path the kernel refused
at runtime.

```json
{
  "timestamp": "2026-08-07T18:26:11.007+00:00",
  "type": "sandbox_denial",
  "operation_id": "op_44_sh",
  "path": "/etc/hosts",
  "access": "read+write",
  "tool_name": "run_terminal_command"
}
```

## Redaction

Argument summaries, command strings, and paths pass through the same secret
redaction operation output already goes through before it is spilled — the same
rules, the same `[REDACTED]` marker. An audit log that captures secrets is a
liability, not a control.

Values are flattened to one line and length-bounded before they are written, so a
multi-line argument cannot smuggle a secret past a line-oriented rule or break
the one-line-per-event invariant.

## Retention

The audit log is **not** swept by log retention. `cleanup_old_logs` prunes
managed rolling logs (`ahma.log*`, `ahma_bridge.*`) after 24 hours and a
background sweep prunes `operations/*.log` on the same window; neither matches
`audit.jsonl`. A trail that silently deletes its own oldest entries is not an
audit trail.

The consequence is that the file grows for as long as the project is worked on.
Entries are small (a few hundred bytes; hard-capped well under 4 KiB) and there
are two per command plus one per disclosed write, so a heavy day of ten thousand
commands costs single-digit megabytes. If you want a retention policy, apply your
own — deliberately, and ideally by archiving rather than deleting.

## Failure behaviour

An audit write can never fail a command. If the log cannot be written the error
is reported at `warn` with the path and execution continues. It is never silent:
an audit trail that stops without saying so is worse than one that was never
there, because it is still believed.

## Configuration

There is none, by design. Auditing that has to be switched on is auditing that is
off on the machines where it would have mattered, and an off switch on an audit
log is a feature request from the wrong side of the threat model. The location
follows `--log-dir` / the `log_dir` setting with everything else.

## Related

- [security-sandbox.md](security-sandbox.md) — the sandbox this log records the
  decisions of, including the trust-handoff tiers.
- [permissions.md](permissions.md) — how a denial becomes a question, and how a
  grant is made.
- [task-vault.md](task-vault.md) — the task-vault audit log, which uses the same
  event envelope and field names for `tool_call` / `tool_complete`.
