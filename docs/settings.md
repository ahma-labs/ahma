# Ahma Settings File (`~/.ahma/settings.toml`)

The settings file is the primary way to configure Ahma's behaviour.  
It replaces the old `AHMA_*` environment variables with a single, self-documenting file.

## Getting started

```bash
ahma settings init        # create with all defaults commented out
ahma settings show        # print effective configuration (with source annotations)
ahma settings show --origin  # print exact per-key provenance (cli / user / default + file path)
ahma --no-settings serve stdio  # ignore settings file for one invocation
```

`--origin` reports the *true* source of each setting: `cli` when a flag passed in
the same invocation overrides the key (e.g. `ahma --timeout 30 settings show --origin`),
`user (<path>)` when the settings file explicitly sets the key — even if it sets it
to the default value — and `default` otherwise. Both `--settings-path` and
`--no-settings` are honored.

## File location

| Platform | Default path |
|----------|--------------|
| macOS / Linux | `~/.ahma/settings.toml` |
| Windows | `%USERPROFILE%\.ahma\settings.toml` |

Override the path for a single invocation:
```bash
ahma --settings-path /path/to/my.toml serve stdio
```

---

## Priority order (highest wins)

1. **CLI flags** (`--timeout 600`, `--no-sandbox`, `--tmp`, …)
2. **`~/.ahma/settings.toml`**
3. **`AHMA_*` environment variables** _(deprecated — emit a warning)_
4. **Compiled-in defaults**

---

## Full schema

All options are commented out by default.  
Run `ahma settings init` to generate this file automatically.

```toml
# ~/.ahma/settings.toml — Ahma user settings
#
# All options are commented out.  Uncomment and edit any value to override
# the compiled-in default.  CLI flags always take highest priority, followed
# by this file, followed by deprecated AHMA_* environment variables.

# ── LM Studio (local OpenAI-compatible server) ──────────────────────────────
# Start the LM Studio Local Server (Developer tab), or headless: lms server start
#
# [lmstudio]
# base_url = "http://localhost:1234/v1"  # default: 1234 (LM Studio local server)
# model    = "openai/gpt-oss-20b"        # set to the model loaded in LM Studio

# ── Tool execution ───────────────────────────────────────────────────────────
# [tools]
# timeout_secs = 360      # default tool timeout (seconds)
# force_sync   = false    # run all tools synchronously instead of async-first
# hot_reload   = false    # reload tools from disk on change — INSECURE in production
# skip_probes  = false    # skip availability probes at startup

# ── Sandbox & filesystem security ────────────────────────────────────────────
# [sandbox]
# disable      = false    # UNSAFE: disable kernel sandbox entirely
# tmp_access   = false    # add system temp dir to sandbox scope
# disable_temp = false    # block all access to system temp dir (overrides tmp_access)
# defer        = false    # defer sandbox lock until client provides roots/list

# ── Logging ──────────────────────────────────────────────────────────────────
# [logging]
# target                  = "file"   # "file" (rolling) or "stderr"
# log_monitor             = false    # enable live log monitoring via LLM
# monitor_rate_limit_secs = 60       # min seconds between log-monitor alerts

# ── Progressive disclosure ────────────────────────────────────────────────────
# [disclosure]
# reveal_profile = "minimal"   # "minimal" | "balanced" | "full"

# ── HTTP server (ahma serve http only) ───────────────────────────────────────
# [http]
# handshake_timeout_secs = 45      # MCP handshake timeout
# disable_quic           = false   # disable HTTP/3 QUIC; fall back to HTTP/2 TCP
# disable_http1_1        = false   # reject HTTP/1.1; require HTTP/2+

# ── HTTP authentication & rate limiting ──────────────────────────────────────
# [auth]
# require_token_path = ""   # path to file containing required bearer token
# rate_limit_rps     = 0    # max requests/second (0 = no limit)
# rate_limit_burst   = 10   # burst allowance

# ── Instance identity ────────────────────────────────────────────────────────
# [instance]
# label = "ahma"   # instance name shown in TUI and daemon event stream
```

---

## LM Studio configuration

[LM Studio](https://lmstudio.ai/) runs local LLMs and exposes an OpenAI-compatible
API through its built-in **Local Server**. Ahma auto-registers an `lmstudio`
provider from these settings.

### Starting the server

Open LM Studio, load a model, then go to the **Developer** tab and click
**Start Server**. Or start it headless:

```bash
# Start the server (loads the last-used model)
lms server start
```

The server listens on `http://localhost:1234/v1` by default.

### Changing the model in settings.toml

Set `model` to the identifier of the model loaded in LM Studio (shown next to the
loaded model in the app):

```toml
[lmstudio]
model = "openai/gpt-oss-20b"
```

### Using LM Studio as a named provider in tool definitions

The LM Studio settings are exposed as a named provider available in `livelog`
tools:

```json
{
  "tool_type": "livelog",
  "livelog": {
    "llm_provider": {
      "base_url": "http://localhost:1234/v1",
      "model": "openai/gpt-oss-20b"
    }
  }
}
```

> **Tip**: You can reference the LM Studio base URL and model from `settings.toml`
> directly — the `ahma settings show` command prints the currently configured values.

---

## Migration from environment variables

If you previously used `AHMA_*` environment variables, Ahma will emit a
`WARN` log entry for each one it reads as a fallback, guiding you to move
it to settings.

| Old env var | New settings.toml key |
|-------------|----------------------|
| `AHMA_TIMEOUT` | `[tools] timeout_secs` |
| `AHMA_SYNC` | `[tools] force_sync` |
| `AHMA_HOT_RELOAD` | `[tools] hot_reload` |
| `AHMA_SKIP_PROBES` | `[tools] skip_probes` |
| `AHMA_DISABLE_SANDBOX` | `[sandbox] disable` |
| `AHMA_TMP_ACCESS` | `[sandbox] tmp_access` |
| `AHMA_DISABLE_TEMP` | `[sandbox] disable_temp` |
| `AHMA_SANDBOX_DEFER` | `[sandbox] defer` |
| `AHMA_LOG_TARGET` | `[logging] target` |
| `AHMA_LOG_MONITOR` | `[logging] log_monitor` |
| `AHMA_MONITOR_RATE_LIMIT` | `[logging] monitor_rate_limit_secs` |
| `AHMA_REVEAL_PROFILE` | `[disclosure] reveal_profile` |
| `AHMA_HANDSHAKE_TIMEOUT` | `[http] handshake_timeout_secs` |
| `AHMA_DISABLE_QUIC` | `[http] disable_quic` |
| `AHMA_DISABLE_HTTP1_1` | `[http] disable_http1_1` |
| `AHMA_REQUIRE_TOKEN_PATH` | `[auth] require_token_path` |
| `AHMA_RATE_LIMIT_RPS` | `[auth] rate_limit_rps` |
| `AHMA_RATE_LIMIT_BURST` | `[auth] rate_limit_burst` |
| `AHMA_INSTANCE_LABEL` | `[instance] label` |

### Env vars that are NOT migrated (still required)

These are system/process-level conventions that belong in the environment, not a user file:

| Variable | Purpose |
|----------|---------|
| `RUST_LOG` | Standard Rust log filter (e.g. `debug`, `info`) |
| `OTEL_*` | OpenTelemetry standard variables |
| `AHMA_TOOLS_DIR` | Override the tools directory (useful in CI scripts) |
| `AHMA_SANDBOX_SCOPE` | Colon-separated sandbox scope paths (multi-path lists don't fit well in TOML) |
| `AHMA_WORKING_DIRS` | Fallback working directories for deferred sandbox |
| `AHMA_DAEMON_SOCK` | Unix socket path for daemon IPC (low-level override) |
| `AHMA_TASK_VAULT` | Task vault root path (typically set by orchestration scripts) |

---

## Relationship with `~/.ahma/config.toml`

`~/.ahma/config.toml` is the **provider registry** file — it stores named LLM providers and cluster peers.  
`~/.ahma/settings.toml` is the **behaviour configuration** file — it stores runtime options.

Both files coexist independently.

| File | Purpose | Managed with |
|------|---------|-------------|
| `~/.ahma/config.toml` | LLM providers, cluster peers | `ahma llm add/remove`, `ahma cluster add-peer` |
| `~/.ahma/settings.toml` | Runtime behaviour defaults | `ahma settings init` + text editor |

---

## Permissions

Everything ahma has been granted — filesystem scopes, web domains, per-workspace
tool approvals — lives in this one file, and is managed with one command:

```bash
ahma permissions list           # every grant, with where it came from
ahma permissions revoke ...     # previews the change; --yes applies it
```

Two `[sandbox]` keys are worth knowing:

- **`profiles`** — the shipped toolchain carve-outs (`rust`, `node`, `go`,
  `common`). These used to be hard-coded in the sandbox backends, invisible and
  un-refusable; they are now data you can inspect and disable. Set to `[]` for the
  strictest isolation.
- **`persistent_scopes`** — the directories you have granted, surviving every
  `roots/list` update. Written by `ahma sandbox grant` or by an approved prompt,
  never by a sandboxed command (this file is outside every sandbox scope, by
  design — SPEC R5.4.8).

See [permissions.md](permissions.md) for the full model.

## See also

- [environment-variables.md](environment-variables.md) — remaining env vars reference
- [llm-providers.md](llm-providers.md) — LLM provider configuration
- [security-sandbox.md](security-sandbox.md) — sandbox security details
- [live-log-monitoring.md](live-log-monitoring.md) — log monitor setup
