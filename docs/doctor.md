# Doctor — `ahma doctor` and `/doctor`

**Status:** tests-pass. SPEC: [R-DOCTOR](../SPEC.md).

## Why

When ahma misbehaves, the cause is usually in its own state rather than in
your project: a grant for a folder that no longer exists, a hub still
running an older build, a settings file that stopped parsing, a warning that
repeats thousands of times in the log. You should not have to know where each
of those lives. The doctor looks, says what it found and what it costs, and
offers a fix — which it applies only after you have seen that exact fix and
said `y`. A model can help you understand the report; it can never apply
anything. Some findings are yours to change (another tool's allow-list, say),
so when there is no fix to offer, the report ends with "Nothing to fix." only if
nothing is wrong. Otherwise it counts the warnings and problems that remain
(SPEC R-DOCTOR.13).

## Quickstart

```bash
ahma doctor                 # report only; changes nothing
ahma doctor --fix           # offers each fix in turn: y applies it, anything else skips
ahma doctor --path ~/proj   # check a different folder (trust, logs)
ahma doctor turn temp off   # opens ahma tui and asks the doctor in your words
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
[!] Hub is a different build
    The hub is ahma 0.21.3+4437961, this is 0.21.3+7ffb5749. …
[i] This folder is not trusted
    Tools that change something ask first (always allowed here: list_dir, read_file). /settings trust to trust it.
[ok] Settings file reads cleanly
```

**From an agent** (`/ahma doctor`): a sandboxed command cannot read `~/.ahma`, so the agent calls
ahma's `ahma_command` tool and ahma runs `ahma doctor` itself, outside the sandbox, and hands the
agent the report. Only the report: `--fix` stays yours (SPEC R-PROC.7).

## What it checks

| Check | Level | Fix offered |
|---|---|---|
| Settings file parses | problem if not | none — the message names the line; ahma never overwrites a file it cannot read |
| Granted folders (`[sandbox] persistent_scopes`) that no longer exist | warn | remove them |
| Saved grants on the hard denylist, skipped at every start | warn | remove them |
| Granted folders with no workspace (they apply to every project) | warn | none — revoke and re-grant from the project (SPEC R5.4.11) |
| Tool approvals for folders that no longer exist | info | forget them |
| A grant letting a workspace write **another repository** that a dialog's yes saved and no human confirmed (SPEC R-PERM.11) | warn | revoke it (keep it instead with the `ahma sandbox grant … --workspace …` the finding names) |
| A write grant a dialog's yes saved on a folder whose contents **run later** as you, outside the sandbox — a folder on your `PATH` such as `~/.local/bin`, a git hooks folder, a shell start-up file (SPEC R-HANDOFF.1, R-PERM.10.3) | warn | revoke it (keep it instead with the `ahma sandbox grant … --workspace …` the finding names) |
| Grants into other repositories that you made yourself | info | none — a deliberate setup is listed, never questioned |
| Hub running a different build than this binary | warn | none — quit ahma sessions and the next one starts the new build |
| A harness allow-list auto-approves `sandbox_grant` or `network_grant` | warn | none |
| A client's shell is hooked but its native edits are not guarded | warn | none — `ahma hooks install` (SPEC R5.5.6) |
| Which clients' hooks apply ahma's sandbox | info | none |
| An old `target/sccache` cache | info | none — the `rm -rf` it names |
| Whether this folder is trusted, and what is always allowed here | info | none — `/settings trust` |
| Log size, and the most repeated warnings in the newest log | info / warn | none |
| Antigravity permission grants (`~/.gemini/antigravity-cli/settings.json`, `~/.gemini/config/config.json`) | warn / info | prune bloated one-off/malformed entries and install clean prefix token grants |
| Git authentication from inside the sandbox: a key the [SSH key broker](ssh-agent-broker.md) signs with, or an agent holding any other key; HTTPS credential helper not blocked by a sandbox setting | warn / info | none — the finding names the exact `ssh-add` or `ssh-keygen` command, or settings key |
| A build helper (sccache, a Gradle or Kotlin daemon) left running inside a sandbox, by shape: confined, orphaned, your own executable; or a host service a profile declares (adb's server), wherever it is installed ([security-sandbox.md](security-sandbox.md#host-services-adbs-server-finds-no-phone), SPEC R-PROC.6). New ones no longer arise: a sandboxed command's processes end with it ([security-sandbox.md](security-sandbox.md#helpers-left-running-inside-a-sandbox-sccache-gradle-and-kotlin-daemons), SPEC R-PROC.5) | warn | sccache: restart it (`--stop-server` / `--start-server`); a host service: restart it with its profile's commands (`adb kill-server` / `adb start-server`); Gradle/Kotlin: stop it. Refused if the doctor is itself sandboxed |
| How grant prompts are being answered: median time-to-decision, share under three seconds, advisor followed | info / warn | none — a habit, not a file |
| A session started in a subfolder of a git checkout whose root `.claude/settings.json` carries ahma's shell hook (Claude Code reads that file only from the folder a session starts in, so the hook never ran) | warn | none — start the session at the git root, or `ahma hooks install` for a user-level hook |
| Whether temp folders are open to your agents (`/tmp`, or `/private/tmp` and `/private/var/folders` on macOS; on by default, SPEC R5.2.5) | info | none — the finding names the command that closes temp (`ahma settings set sandbox.disable_temp true`), or reopens it |
| A session started in a folder that holds git checkouts (one or two folders below) whose own `.claude` settings declare hooks: Claude Code loads none of them there | info; warn when ahma's shell hook is among them and your own settings carry none | none — start the session in the checkout it works on, or `ahma hooks install` |

Every applied fix is written through the same strict read-modify-write the
rest of ahma uses (it will not rewrite a file it cannot parse) and appended to
`~/.ahma/permissions-audit.jsonl`.

## Asking the doctor

`ahma doctor <words>` (for example `ahma doctor turn temp off`) opens `ahma tui` on the
report and asks your words as `/doctor <words>` once the TUI is connected to the hub.
`ahma doctor fix 2` opens it on fix 2's confirmation. Without a terminal it prints the
report and says to ask in one.

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
- [docs/setup.md](setup.md) — `ahma setup --plan` proposes your doctor's fixes together with project-specific settings
- SPEC: R-DOCTOR, R-PERM, R5.4.8
