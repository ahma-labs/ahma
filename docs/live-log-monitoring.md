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

## Android Logcat Monitoring

### Prerequisites

- **ADB** installed and on `PATH` (`brew install android-platform-tools` or via Android Studio)
- Device or emulator connected (`adb devices` should show it)
- **Ollama** running locally: `brew install ollama && ollama serve`
- `lfm2.5:8b` model pulled: `ollama pull lfm2.5:8b` (the default; a modern 8B model triages crash-vs-noise far better than an older 3B one). To use a smaller/faster model, pull it and set `AHMA_LIVELOG_MODEL`, e.g. `AHMA_LIVELOG_MODEL=llama3.2`.

### Setup

1. Copy the example tool definition into your project's `.ahma/` directory:

```bash
mkdir -p .ahma
cp /path/to/ahma/.ahma/android-logcat.json .ahma/
```

Or create `.ahma/android-logcat.json` with the content below.

2. Reload your MCP server configuration (restart VS Code or run "MCP: Restart Server").

### Example Tool Definition

```json
{
    "name": "android-logcat",
    "description": "Monitor Android application logs via ADB logcat, using an LLM to detect crashes, exceptions, and ANR errors in real-time. Returns immediately with an operation ID; push alerts are delivered as MCP progress notifications whenever the LLM detects an issue. Use `cancel` with the operation ID to stop monitoring.",
    "command": "adb",
    "tool_type": "livelog",
    "enabled": true,
    "livelog": {
        "source_command": "adb",
        "source_args": ["logcat", "-b", "main,crash,system", "-v", "threadtime", "--pid=${pid}"],
        "parameters": [
            {"name": "serial", "description": "Target device serial (from `adb devices`). Sets ANDROID_SERIAL; required when more than one device is connected."},
            {"name": "pid", "description": "Restrict to one process id (logcat --pid). Resolve with `adb shell pidof -s <package>`."}
        ],
        "env": {"ANDROID_SERIAL": "${serial}"},
        "clear_command": ["logcat", "-c"],
        "prefilter_regex": "(?i)(FATAL|ANR\\b|Exception|SIGSEGV|SIGABRT|beginning of crash|tombstone|\\sE\\s|\\sF\\s)",
        "detection_prompt": "Look for crashes (FATAL EXCEPTION, NullPointerException, IllegalStateException), Application Not Responding (ANR) errors, native crashes (SIGSEGV, SIGABRT), or any log line at level E (ERROR) or F (FATAL) that indicates a real problem rather than a known-harmless library warning.",
        "llm_provider": {
            "base_url": "${AHMA_LIVELOG_BASE_URL:-http://localhost:11434/v1}",
            "model": "${AHMA_LIVELOG_MODEL:-lfm2.5:8b}"
        },
        "chunk_max_lines": 50,
        "chunk_max_seconds": 30,
        "cooldown_seconds": 60,
        "llm_timeout_seconds": 30
    },
    "hints": {
        "custom": {
            "usage": "Call this tool to start live monitoring of Android logs. The tool returns an operation ID immediately. You will receive progress notifications when the LLM detects crashes or errors. Optional params: `serial`, `pid`, `clear`.",
            "prerequisites": "ADB must be installed and on PATH. Device/emulator connected (`adb devices`). Ollama running locally on port 11434 with `lfm2.5:8b` available (`ollama pull lfm2.5:8b`).",
            "stopping": "Use `cancel <operation_id>` to stop monitoring gracefully."
        }
    }
}
```

Key fields that make this usable against a real device:

- **`-b main,crash,system`** — monitors the dedicated `crash` and `system` buffers (where `FATAL`/native crashes and ANRs land), not just `main`.
- **`parameters` + `${...}` substitution** — `source_args` and `env` may reference declared parameters. When a parameter is omitted, the **whole argument token** referencing it is dropped, so `--pid=${pid}` cleanly disappears (full-device logcat) until you pass a `pid`.
- **`env: { "ANDROID_SERIAL": "${serial}" }`** — the clean way to target one of several connected devices; the entry is skipped entirely when no `serial` is passed.
- **`clear_command`** — runs `adb logcat -c` first so a stale crash from a previous run is not replayed as a fresh alert. Pass `clear: false` to keep the existing buffer.
- **`prefilter_regex`** — only lines matching this cheap regex are forwarded to the LLM (the full log is still recorded), keeping token cost and latency low. An invalid pattern is ignored rather than fatal.
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
Started live log monitoring. Operation ID: op_abc123
You will be notified of any detected issues.
```

### Stopping Monitoring

```
Cancel operation op_abc123
```

Or use the built-in `cancel` tool with the operation ID.

### What You'll See

When an issue is detected, you receive a progress notification like:

```
[android-logcat] LLM alert: FATAL EXCEPTION in com.example.app — NullPointerException at
MainActivity.onCreate(MainActivity.kt:42). Triggered by 3 lines at 14:32:06.
```

---

## Custom Rust Log Monitoring

You can create a custom `rust-log-monitor` livelog tool to watch your application's tracing output or tail logs during development.

### Example Tool Definition (`.ahma/rust-log-monitor.json`)

```json
{
    "name": "rust-log-monitor",
    "description": "Monitor Rust application logs.",
    "command": "tail",
    "tool_type": "livelog",
    "enabled": true,
    "livelog": {
        "source_command": "tail",
        "source_args": ["-F", "./logs/my-app.log"],
        "detection_prompt": "Look for ERROR or WARN level tracing entries, thread panics, unwrap failures on Option::None or Result::Err, stack traces/backtraces, or signals (SIGSEGV, SIGABRT).",
        "llm_provider": {
            "base_url": "http://localhost:11434/v1",
            "model": "llama3.2"
        },
        "chunk_max_lines": 50,
        "chunk_max_seconds": 30,
        "cooldown_seconds": 60
    }
}
```

### Setup

1. Create the file `.ahma/rust-log-monitor.json` with the definition above.
2. Ensure Ollama is running and `llama3.2` is pulled:
```bash
ollama pull llama3.2
```

---

## Other Use Cases

### Remote Server Logs (SSH + tail)

```json
{
    "name": "server_error_monitor",
    "description": "Monitor remote server error logs for critical issues.",
    "command": "ssh",
    "tool_type": "livelog",
    "enabled": true,
    "livelog": {
        "source_command": "ssh",
        "source_args": ["user@myserver.example.com", "tail", "-f", "/var/log/app/error.log"],
        "detection_prompt": "Look for ERROR or FATAL log entries, stack traces, out-of-memory messages, or database connection failures. Ignore INFO and WARN level messages.",
        "llm_provider": {
            "base_url": "http://localhost:11434/v1",
            "model": "llama3.2"
        },
        "chunk_max_seconds": 15,
        "cooldown_seconds": 30
    }
}
```

### Docker Container Logs

```json
{
    "name": "docker_logs",
    "description": "Monitor a Docker container for errors.",
    "command": "docker",
    "tool_type": "livelog",
    "enabled": true,
    "livelog": {
        "source_command": "docker",
        "source_args": ["logs", "-f", "my-container"],
        "detection_prompt": "Look for unhandled exceptions, panic messages, connection refused errors, or out-of-memory kills.",
        "llm_provider": {
            "base_url": "http://localhost:11434/v1",
            "model": "llama3.2"
        }
    }
}
```

### Local File (tail -f)

```json
{
    "name": "app_log_monitor",
    "description": "Monitor a local application log file.",
    "command": "tail",
    "tool_type": "livelog",
    "enabled": true,
    "livelog": {
        "source_command": "tail",
        "source_args": ["-f", "logs/app.log"],
        "detection_prompt": "Look for ERROR level log entries, uncaught exceptions, or timeout messages.",
        "llm_provider": {
            "base_url": "http://localhost:11434/v1",
            "model": "llama3.2"
        }
    }
}
```

---

## Using a Cloud LLM Provider

If you prefer a cloud API (e.g. OpenAI) instead of Ollama, add an `api_key` and point `base_url` to the provider:

```json
"llm_provider": {
    "base_url": "https://api.openai.com/v1",
    "model": "gpt-4o-mini",
    "api_key": "sk-..."
}
```

> **Privacy note**: Using a cloud provider sends your log content to that provider. For sensitive production logs, prefer a local model (Ollama, LM Studio, etc.) or ensure your cloud provider has appropriate data handling agreements.

---

## Configuration Reference

See [SPEC.md Section 5.5](../SPEC.md) for the full `LivelogConfig` and `LlmProviderConfig` field reference.

The MTDF JSON schema (which includes `LivelogConfig`) is at [docs/mtdf-schema.json](mtdf-schema.json).
