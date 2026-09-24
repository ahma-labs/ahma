# LLM Provider Configuration

Ahma supports any OpenAI-compatible LLM API. All providers — local (Ollama, vLLM,
LM Studio, llama.cpp) and remote (OpenAI, Anthropic-compatible, Azure OpenAI) — share
the same `/v1/chat/completions` interface.

## Named providers in `~/.ahma/config.toml`

Define providers once and reference them by name instead of repeating connection
details (and risking literal API keys) in every tool file:

```toml
# ~/.ahma/config.toml

[[providers]]
name          = "ollama-local"
base_url      = "http://localhost:11434/v1"
default_model = "llama3.2"
# No api_key — local Ollama does not require authentication

[[providers]]
name          = "workstation"
base_url      = "http://workstation.local:11434/v1"
default_model = "gemma4"

[[providers]]
name          = "openai"
base_url      = "https://api.openai.com/v1"
default_model = "gpt-4o-mini"
api_key       = "${OPENAI_API_KEY}"   # ← env-var reference, not a literal key

[[providers]]
name          = "azure-openai"
base_url      = "https://my-resource.openai.azure.com/openai/deployments/gpt-4o/v1"
default_model = "gpt-4o"
api_key       = "${AZURE_OPENAI_KEY}"
```

### Referencing a named provider in a tool file

```json
{
  "name": "analyse-logs",
  "tool_type": "livelog",
  "llm_provider": {
    "base_url": "http://localhost:11434/v1",
    "model": "llama3.2"
  }
}
```

> **Note**: tool files cannot yet reference a named provider; inline the connection
> details and use `${ENV_VAR}` for any API key field.

## API key security

**Never write a literal API key in a tool definition file.**  
The `api_key` field in any `llm_provider` block supports `${ENV_VAR}` interpolation.
Ahma will expand the placeholder at runtime from the process environment and will
**warn** (via the log) if it detects a value that looks like a real key (starts with
`sk-`, `AKIA`, `ghp_`, etc.) written literally.

```json
{
  "llm_provider": {
    "base_url": "https://api.openai.com/v1",
    "model": "gpt-4o-mini",
    "api_key": "${OPENAI_API_KEY}"
  }
}
```

Set the key in your shell before starting ahma:

```bash
export OPENAI_API_KEY="$(cat ~/.secrets/openai-key)"
ahma serve stdio
```

Or in a `.env` file that is **not committed to version control**.

## LM Studio (`[lmstudio]` in `~/.ahma/settings.toml`)

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

## Provider compatibility table

| Provider | `base_url` pattern | Notes |
|----------|--------------------|-------|
| Ollama | `http://localhost:11434/v1` | No auth needed |
| vLLM | `http://localhost:8000/v1` | Token optional (set `--api-key` on server) |
| LM Studio | `http://localhost:1234/v1` | No auth needed |
| llama.cpp server | `http://localhost:8080/v1` | No auth needed |
| OpenAI | `https://api.openai.com/v1` | `api_key` required |
| Azure OpenAI | `https://<resource>.openai.azure.com/openai/deployments/<deploy>/v1` | `api_key` required |
| Together AI | `https://api.together.xyz/v1` | `api_key` required |
| Fireworks | `https://api.fireworks.ai/inference/v1` | `api_key` required |

## Testing connectivity

```bash
# Quick reachability check (replace URL and model as needed)
curl http://localhost:11434/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"ping"}],"max_tokens":5}'
```

`ahma llm test <name>` does the same against a named provider's `/models` endpoint.

### When a provider is unreachable

Requests to a provider retry transient failures — a refused connection, a dropped connection,
HTTP 429 or 5xx — up to three times with backoff, honouring `Retry-After`. A model on this
machine is not re-sent its prompt after a timeout: it is slow, not gone, and a re-send makes it
start reading again. If every attempt fails, the message leads with which endpoint is down and
what to check, then the technical detail:

```text
Couldn't reach your local model server at localhost:11434.
Check the model server is running, then send your message again.
Details: error sending request for url (http://localhost:11434/v1/chat/completions): … Connection refused (gave up after 4 attempts over 3.4s)
```

In `ahma tui`, a turn that fails this way before any answer arrived is sent again once
automatically. The rules are in [SPEC.md](../SPEC.md) R-HTTP.

## See also

- [docs/settings.md](settings.md) — `[lmstudio]`, `[agent]` and the chat token budgets
- [docs/live-log-monitoring.md](live-log-monitoring.md) — `livelog` tools, the main provider consumer
