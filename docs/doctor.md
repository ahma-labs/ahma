# Doctor — `ahma doctor` and `/doctor`

**Status:** Stable (v0.22). SPEC: [R-DOCTOR](../SPEC.md).

## Why

When ahma misbehaves, the cause is usually in its own state rather than in
your project: a grant for a folder that no longer exists, a daemon still
running an older build, a settings file that stopped parsing, a warning that
repeats thousands of times in the log. You should not have to know where each
of those lives. The doctor looks, says what it found and what it costs, and
offers a fix — which it applies only after you have seen that exact fix and
said `y`. A model can help you understand the report; it can never apply
anything.

## Quickstart

```bash
ahma doctor                 # report only; changes nothing
ahma doctor --fix           # offers each fix in turn: y applies it, anything else skips
ahma doctor --path ~/proj   # check a different folder (trust, logs)
```

In `ahma tui`:

```
/doctor                           the report, fixes numbered
/doctor fix 1                     shows fix 1 again; y applies, n/Enter/Esc leaves it
/doctor why is my model so slow?  the chat model answers, with the report as context
```

Example report:

```
[!] Granted folders that no longer exist
    /opt/two — every ahma session tries to add these to its sandbox and logs a warning.
    fix 1: Remove 1 granted folder(s) that no longer exist from ~/.ahma/settings.toml: /opt/two
[!] Daemon is a different build
    The daemon is ahma 0.21.3 (4437961), this is 0.21.3 (7ffb5749). …
[i] This folder is not trusted
    Tools that change something ask first (always allowed here: list_dir, read_file). /settings trust to trust it.
[ok] Settings file reads cleanly
```

## What it checks

| Check | Level | Fix offered |
|---|---|---|
| Settings file parses | problem if not | none — the message names the line; ahma never overwrites a file it cannot read |
| Granted folders (`[sandbox] persistent_scopes`) that no longer exist | warn | remove them |
| Tool approvals for folders that no longer exist | info | forget them |
| Daemon running a different build than this binary | warn | none — quit ahma sessions and the next one starts the new build |
| Whether this folder is trusted, and what is always allowed here | info | none — `/settings trust` |
| Log size, and the most repeated warnings in the newest log | info / warn | none |

Every applied fix is written through the same strict read-modify-write the
rest of ahma uses (it will not rewrite a file it cannot parse) and appended to
`~/.ahma/permissions-audit.jsonl`.

## Asking the doctor

`/doctor <question>` sends your question to the chat model together with the
current report and rules it must keep: it cannot change ahma's settings (they
live in `~/.ahma`, outside every sandbox), so it tells you the `/settings` row,
`/doctor fix <n>` or `ahma` command to use; and it never suggests widening
access — trusting a folder, granting a path, allowing a domain — without
saying what that would let tools do.

## See also

- [docs/tui.md](tui.md) — `/intro`, `/settings`, and the rest of the TUI
- [docs/permissions.md](permissions.md) — the permission ledger and audit log
- [docs/settings.md](settings.md) — every setting
- [docs/system-assistant-plan.md](system-assistant-plan.md) — the plan for
  letting a local model change system settings under the same
  "model proposes, ahma disposes" rule
- SPEC: R-DOCTOR, R-PERM, R5.4.8
