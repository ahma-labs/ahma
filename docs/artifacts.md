# Interactive HTML Artifacts

> **Experimental** — introduced in v0.7.

Tools can emit self-contained HTML artifacts to `outputs/result.html` inside the vault. Each artifact renders the tool's output data, includes an embedded LLM chat widget powered by your local Ollama instance, and connects back to a per-task localhost API server — all without leaving the vault or touching the network.

## Why interactive artifacts?

Static output (JSON, text, CSV) requires the user to re-engage the AI agent to explore the data further. An artifact turns every tool result into a mini-application: the user opens the HTML file in their browser, asks follow-up questions in the embedded chat, and iterates without burning additional agent context or making cloud API calls.

The artifact is part of the vault, so it is auditable, version-tracked, and re-openable at any time.

## How it works

```
Tool run → outputs/result.html
                │
                │  open in browser
                ▼
         ┌────────────────────────┐
         │  Rendered data table   │
         │  or pre block          │
         │                        │
         │  [Chat with this data] │
         │  You: ...              │
         │  AI: ...               │
         └────────┬───────────────┘
                  │ POST /chat (localhost only)
                  ▼
         ArtifactServer (127.0.0.1:<random-port>)
                  │ relay with bearer token
                  ▼
         Ollama / local LLM
```

The artifact server is bound to `127.0.0.1` only and requires a short-lived bearer token embedded in the HTML. Requests without the token receive `401 Unauthorized`.

## Generating an artifact from Rust

Using the `ahma_core` crate:

```rust
use ahma_core::{ArtifactBuilder, ArtifactServer};
use serde_json::json;

// Start the per-task API server (relay to local Ollama)
let server = ArtifactServer::start("http://localhost:11434/v1").await?;

// Build the artifact HTML
let html = ArtifactBuilder::new("Q4 Revenue Analysis")
    .description("Key metrics from the Q4 financial report")
    .data(json!({"revenue": 42_000, "growth": "12%", "top_region": "EMEA"}))
    .chat_model("gemma4")
    .local_api_url(server.base_url())
    .api_token(&server.token)
    .build();

// Write to vault outputs
html.save(&vault.outputs.join("result.html"))?;
```

## Artifact anatomy

The generated HTML is a single self-contained file:

- **Data section** — renders `data` as a key/value table (object) or `<pre>` block (array/scalar).
- **Text output** — optional `<pre>` block for raw command output.
- **Chat widget** — textarea and send button; messages are sent to the relay endpoint using the `Fetch` API.
- **Dark-mode UI** — accessible colour scheme, no external CSS dependencies.

## Chat widget

The embedded chat sends `POST /chat` to the local relay with the full message history plus a system context that includes the artifact title and embedded data. The model has full context for follow-up questions without re-running any commands.

## Privacy

The chat endpoint is `localhost` only. The bearer token is a random 16-byte hex string generated fresh each run. It is embedded in the HTML and never sent over the network in the clear. No data reaches cloud services unless you configure a cloud endpoint explicitly.

## See also

- [docs/task-vault.md](task-vault.md) — where artifacts are stored
- [SPEC.md](../SPEC.md) — artifact server specification
