# LLM Provider Configuration

Ahma speaks two wire formats. Every OpenAI-compatible server uses `/chat/completions`:
local ones (Ollama, vLLM, LM Studio, llama.cpp) and remote ones (OpenAI, Azure OpenAI,
OpenRouter, Together, Fireworks). Anthropic uses its native Messages API, chosen
automatically for `api.anthropic.com` or with `kind = "anthropic"` on a provider.

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
base_url      = "https://my-resource.openai.azure.com/openai/v1"
default_model = "my-gpt-4o-deployment"   # the deployment name
api_key       = "${AZURE_OPENAI_KEY}"
```

### Configuring the provider in a tool file

A `livelog` tool names its model inside its `livelog` block:

```json
{
  "name": "analyse-logs",
  "tool_type": "livelog",
  "livelog": {
    "llm_provider": {
      "base_url": "http://localhost:11434/v1",
      "model": "llama3.2"
    }
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
  "livelog": {
    "llm_provider": {
      "base_url": "https://api.openai.com/v1",
      "model": "gpt-4o-mini",
      "api_key": "${OPENAI_API_KEY}"
    }
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
| Azure OpenAI | `https://<resource>.openai.azure.com/openai/v1` | `api_key` required; the model is your deployment name. The older `/openai/deployments/…?api-version=` URLs are not supported. |
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
start reading again. A completion that is not streamed is never re-sent after a timeout
either, local or remote: the provider has probably done the work and billed it already,
and a second attempt would pay for it again. If every attempt fails, the message leads with which endpoint is down and
what to check, then the technical detail:

```text
Couldn't reach your local model server at localhost:11434.
Check the model server is running, then send your message again.
Details: error sending request for url (http://localhost:11434/v1/chat/completions): … Connection refused (gave up after 4 attempts over 3.4s)
```

In `ahma tui`, a turn that fails this way before any answer arrived is sent again once
automatically. The rules are in [SPEC.md](../SPEC.md) R-HTTP.

## Tool calling

- **External MCP tools work with every provider.** ahma names a tool on an external MCP
  server `server::tool`; OpenAI and Anthropic accept only letters, digits, `_` and `-` in a
  function name. Each request sends such a tool as `server__tool` and maps the model's calls
  back, so nothing changes in your configuration.
- **Parallel tool calls stay separate.** Some servers stream several tool calls in one turn
  without numbering them. ahma starts a new call at each new call id, so two calls never
  merge into one with both argument sets glued together.
- **The request fits the model.** OpenAI's own API and Azure OpenAI get
  `max_completion_tokens`; every other server gets `max_tokens`. Reasoning models (o1, o3,
  o4, gpt-5) are sent no `temperature`, since they accept only the default.
- **An error is reported as itself.** When a provider rejects a request, the chat shows the
  provider's own message. ahma retries a turn without tools only when the provider says the
  model does not support tools, and never once tools have already run in the conversation.

## See also

- [docs/settings.md](settings.md) — `[lmstudio]`, `[agent]` and the chat token budgets
- [docs/live-log-monitoring.md](live-log-monitoring.md) — `livelog` tools, the main provider consumer
