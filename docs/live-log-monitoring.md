# Live Log Monitoring

Ahma's livelog feature turns any long-running streaming command into an LLM-powered monitoring tool. Instead of flooding the AI with raw log output, Ahma accumulates lines into time/size-bounded chunks and asks a local or cloud LLM to detect issues described in plain English. When an issue is found, a concise alert is pushed as an MCP progress notification. Between alerts, a configurable cooldown window prevents alert storms.

## How It Works

```
source_command  →  chunk accumulator  →  LLM  →  Alert event on the operation
  (adb logcat)      (50 lines / 30s)    detect      (pushed as notifications/progress)
```

1. `tools/call` on a livelog tool starts a long-running operation and returns its `operation_id` (a livelog tool never waits for completion, whatever `tools.execution_mode` is).
2. A background pipeline spawns the `source_command` inside Ahma's kernel sandbox.
3. Lines are buffered until `chunk_max_lines` is reached or `chunk_max_seconds` elapses.
4. The chunk is sent to the LLM with your `detection_prompt`.
5. If the LLM detects an issue (any response other than `"CLEAN"`), an `Alert` event is recorded on the operation and pushed to the MCP client as a progress notification (and shown in `ahma tui`) — but only if the `cooldown_seconds` window has elapsed since the last alert.
6. Use `cancel <operation_id>` to stop monitoring.
7. The operation ends `Completed` when it watched until it was cancelled or its source ended. When it never started watching (the source command could not be started, the log file could not be opened, or the LLM provider settings are unusable) it ends `Failed`, and its result says why.

## Three ways to monitor

| Need | Use |
|---|---|
| Watch a file already in the workspace | built-in `log_monitor`: `{"file_path": "logs/app.log", "detection_prompt": "…"}`, no tool JSON needed. Optional `llm_base_url`, `llm_model`, `llm_api_key`; with none, it uses the provider of a loaded livelog tool |
| Watch a streaming command (adb, docker, a remote tail) | a `tool_type: "livelog"` tool in `.ahma/` (below) |
| Error and warning lines from a build or test you run | `monitor_level` on `run_terminal_command` (pattern-based, no LLM) |

## Android Logcat Monitoring

### Prerequisites

- **ADB** installed and on `PATH` (`brew install android-platform-tools` or via Android Studio)
- Device or emulator connected (`adb devices` should show it)
- **Ollama** running locally: `brew install ollama && ollama serve`
- `lfm2.5:8b` model pulled: `ollama pull lfm2.5:8b` (the default; a modern 8B model triages crash-vs-noise far better than an older 3B one). To use a smaller/faster model, pull it and set `AHMA_LIVELOG_MODEL` (e.g. `llama3.2`) in the `env` block of the ahma entry in your MCP client config: it is a placeholder in the tool JSON, not an ahma setting.

### Setup

1. Copy the example tool definition into your project's `.ahma/` directory:

```bash
mkdir -p .ahma
cp /path/to/ahma/.ahma/android-logcat.json .ahma/
```

2. Ask the agent to call `restart` (or restart the MCP server). ahma never picks up `.ahma/` changes by itself.

### The tool definition

The definition is [`.ahma/android-logcat.json`](../.ahma/android-logcat.json).
The fields that matter:

- **`-b main,crash,system`** — monitors the dedicated `crash` and `system` buffers (where `FATAL`/native crashes and ANRs land), not just `main`.
- **`parameters` + `${...}` substitution** — `source_args` and `env` may reference declared parameters. When a parameter is omitted, the **whole argument token** referencing it is dropped, so `--pid=${pid}` cleanly disappears (full-device logcat) until you pass a `pid`.
- **`env: { "ANDROID_SERIAL": "${serial}" }`** — the clean way to target one of several connected devices; the entry is skipped entirely when no `serial` is passed.
- **`clear_command`** — runs `adb logcat -c` first so a stale crash from a previous run is not replayed as a fresh alert. Pass `clear: false` to keep the existing buffer.
- **`prefilter_regex`** — only lines matching this cheap regex are forwarded to the LLM (the full log is still recorded), keeping token cost and latency low. An invalid pattern is ignored rather than fatal.
- **`structured_output: true`** — the model answers JSON; alerts read `[ERROR] summary`, then `Exception:` and `at <frame>` lines. Prose answers still alert.
- **Env-driven provider** — `base_url`/`model` support `${VAR}` and `${VAR:-default}`, so you can repoint the endpoint/model via `AHMA_LIVELOG_BASE_URL` / `AHMA_LIVELOG_MODEL` without editing the file.

### Starting Monitoring

In your MCP client (VS Code, Cursor, etc.), call the `android-logcat` tool:

```
Use the android-logcat tool to start monitoring my device logs for crashes.
```

**With multiple devices / scoped to one app:**

```
List devices:           adb devices
Target one device:      call android-logcat with serial="emulator-5554"
Scope to your app:      adb shell pidof -s com.example.app   → call with pid="<that pid>"
Keep buffered history:  call with clear=false
```

The tool returns immediately with an operation ID, for example:

```
Live log monitoring started. Operation ID: op_abc123
Use `status` or `await` to check progress, `cancel` to stop.
```

### Stopping Monitoring

```
Cancel operation op_abc123
```

Or use the built-in `cancel` tool with the operation ID.

### What You'll See

When an issue is detected, you receive a progress notification like:

```
[ERROR] FATAL EXCEPTION in com.example.app — NullPointerException
  Exception: java.lang.NullPointerException
  at MainActivity.onCreate(MainActivity.kt:42)
```

---

## Other sources

The same shape works for any command that streams lines; change `source_command`,
`source_args` and `detection_prompt`:

| `source_command` | `source_args` | Note |
|---|---|---|
| `tail` | `["-F", "logs/app.log"]` | for a file in the workspace, the built-in `log_monitor` needs no JSON |
| `docker` | `["logs", "-f", "my-container"]` | |
| `ssh` | `["user@host", "tail", "-f", "/var/log/app/error.log"]` | `ssh` inside the sandbox cannot read `~/.ssh`; this works only through the [SSH key broker](ssh-agent-broker.md), which livelog does not use yet |

---

## Log files that link outside the workspace

A project's `.ahma/logs/` may hold `*.log` symlinks to files elsewhere on disk — an
app's log under `/var/log`, a simulator's log in a cache directory. Live-log
monitoring reads such a target only when it is inside the sandbox scope or has been
approved; anything else stays blocked, because a symlink is otherwise a way for a
cloned repository to read any file you can.

This needs `--log-monitor` (or `log_monitor = true` under `[logging]` in settings.toml);
without it no symlink target is read. Targets are resolved once, at startup (R9.4): a link
added later grants nothing until the server restarts.

1. `logs_list` lists every log, with the symlink target and `is_approved`. A link
   that points outside the workspace starts out `is_approved: false`.
2. `logs_approve` with `{"file": "app.log", "reason": "…"}` **asks a human**; it
   approves nothing itself, whichever client calls it and however that client's
   tool-permission settings are set — an agent that could approve its own link could read
   any file you can. The question goes through the
   [permission ladder](permissions.md): your client's own prompt when it supports
   elicitation, else an attached `ahma tui`, else nobody (the agent is told so, and how
   you can approve it yourself). The prompt says *a log file in .ahma/logs links to this
   file outside the workspace; approving lets ahma's log tools read it*, shows the
   resolved target and the agent's reason, and offers:

   | Answer | Effect |
   |---|---|
   | **deny** (default) | nothing recorded; not asked again this session |
   | **read-only, this session** | readable now, until the session ends; never written |
   | **read-only, always** | a **`log-target`** grant for that exact file, in this workspace, in `~/.ahma/settings.toml` (`[log_targets]`) — the ledger no sandboxed command can write — with who asked and the surface that answered, appended to `~/.ahma/permissions-audit.jsonl`; readable now and in every later session |

   There is no once or 24-hour answer (a log is read for as long as it is monitored), and
   the target is never writable. A client that cancels the prompt has not answered: nothing
   is recorded and the next call asks again. A target on the hard denylist (credential
   directories such as `~/.ssh`, `~/.ahma`, system directories) is refused before anyone is
   asked.
3. In `ahma tui`, pressing `a` on a blocked log (the banner shows where the link points)
   is the same decision made by you directly: it records the `log-target` grant at once,
   with `tui` as its surface, provided the link still points at the file shown. A
   sandboxed log monitor picks it up from the next session; `logs_list` shows it as
   approved straight away.
4. Review and withdraw it like any other permission:

```bash
ahma permissions list --kind log-target
ahma permissions revoke log-target /var/log/app.log --workspace ~/code/proj   # previews; add --yes
```

Approvals made by an older ahma, in `<config dir>/ahma/log_exceptions.json`, are moved
into the ledger once at startup; the old file is kept as `log_exceptions.json.migrated`.

---

## Using a Cloud LLM Provider

If you prefer a cloud API (e.g. OpenAI) instead of Ollama, add an `api_key` and point `base_url` to the provider:

```json
"llm_provider": {
    "base_url": "https://api.openai.com/v1",
    "model": "gpt-4o-mini",
    "api_key": "${OPENAI_API_KEY}"
}
```

`${VAR}` is read from ahma's own environment (set it in your MCP client's `env` for the
server); never put a key in the file.

> **Privacy note**: Using a cloud provider sends your log content to that provider. For sensitive production logs, prefer a local model (Ollama, LM Studio, etc.) or ensure your cloud provider has appropriate data handling agreements.

---

## Configuration Reference

`livelog` fields (`*` = required):

| Field | Default | Purpose |
|---|---|---|
| `source_command`* | — | Program that streams lines |
| `source_args` | `[]` | Its arguments; may reference `${param}` |
| `parameters[]` | `[]` | `{name, description, required}` the caller may pass |
| `env` | `{}` | Environment for the source; may reference `${param}` |
| `clear_command` | — | Run first (e.g. `logcat -c`); caller passes `clear: false` to skip |
| `prefilter_regex` | — | Only matching lines reach the model |
| `detection_prompt`* | — | What counts as an issue, in plain words |
| `llm_provider`* | — | `{base_url, model, api_key}`; `${VAR}` and `${VAR:-default}` allowed |
| `structured_output` | `false` | Ask the model for JSON verdicts |
| `chunk_max_lines` | `50` | Lines per chunk |
| `chunk_max_seconds` | `30` | Longest wait before a chunk is sent |
| `cooldown_seconds` | `60` | Minimum gap between alerts |
| `llm_timeout_seconds` | `30` | Per-request model timeout |

How `log-target` grants fit with every other permission: [docs/permissions.md](permissions.md).

Schema: [mtdf-schema.json](mtdf-schema.json). Requirements: [SPEC.md §5.5](../SPEC.md#55-livelog-tool-type).
