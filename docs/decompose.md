# Decompose: Local-LLM Question Orchestration

> **Experimental** — introduced in v0.7. API and reducer strategies may change before stabilisation.

The `decompose` tool type splits a complex business question into smaller sub-questions, dispatches each to a local LLM running on your machine (e.g. `gemma4` via Ollama), and aggregates the results using a deterministic Rust reducer. No cloud egress is required.

## Why decompose?

Large questions often exceed what a small local model handles well in a single call. Decompose acts as a conductor: it breaks the question into pieces that fit a 4B–7B parameter model, runs them concurrently, and stitches the answers together without an additional LLM call for aggregation.

This keeps data on your machine, costs nothing per-token, and scales horizontally across any peers you own (see [docs/cluster-scheduler.md](cluster-scheduler.md)).

## Quickstart

Install the built-in config into your project's `.ahma/` directory:

```bash
cp "$(ahma tool info decompose --format json | jq -r .path)" .ahma/decompose.json
# or just create .ahma/decompose.json — see the example below
```

Ensure Ollama is running with `gemma4` pulled:

```bash
ollama pull gemma4
ollama serve  # if not already running
```

Ask your agent:

```
Use the decompose tool to answer: "What are the main risks in our Q4 financial projections?"
```

The tool returns an `operation_id` immediately. The aggregated answer arrives as an MCP progress notification. Use `await <operation_id>` to retrieve it.

## MTDF configuration

Place this in `.ahma/decompose.json`:

```json
{
    "name": "decompose",
    "description": "Split a complex question into sub-questions, run each against a local LLM, and return a summarised answer.",
    "command": "decompose",
    "tool_type": "decompose",
    "enabled": true,
    "synchronous": false,
    "timeout_seconds": 300,
    "decompose": {
        "llm_provider": {
            "base_url": "http://localhost:11434/v1",
            "model": "gemma4"
        },
        "max_subtasks": 5,
        "max_concurrent": 3,
        "reduce_mode": "summarize",
        "llm_timeout_seconds": 60
    }
}
```

A ready-to-use copy ships in [`.ahma/decompose.json`](../.ahma/decompose.json).

## DecomposeConfig fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `llm_provider` | Yes | — | OpenAI-compatible endpoint. Use `http://localhost:11434/v1` for Ollama |
| `max_subtasks` | No | `5` | Maximum sub-questions to generate |
| `max_concurrent` | No | `3` | Sub-questions to dispatch simultaneously |
| `reduce_mode` | No | `summarize` | How to combine results (see table below) |
| `answer_prompt` | No | generic | System prompt injected when answering each sub-question |
| `llm_timeout_seconds` | No | `30` | Timeout per LLM call |

## Reduce modes

| Mode | Behaviour |
|------|-----------|
| `summarize` (default) | Each result under a numbered heading |
| `extract_fields` | Collect unique `key: value` pairs from all results |
| `classify` | Majority-vote label (useful for sentiment, category) |
| `concat` | Join non-empty results with double newlines |
| `first` | Return the first non-empty result |

## Pipeline

```
User question
      │
      ▼ split (LLM call)
Sub-questions [0..N]
      │
      ├── SubTask[0] ──► LlmClient ──► answer_0
      ├── SubTask[1] ──► LlmClient ──► answer_1
      └── SubTask[N] ──► LlmClient ──► answer_N
                                            │
                                    Reducer::reduce()
                                            │
                                    aggregated answer
```

1. `tools/call` returns an `operation_id` immediately.
2. The orchestrator asks the LLM to split the question into at most `max_subtasks` sub-questions.
3. Sub-questions are dispatched in batches of `max_concurrent`.
4. Results are aggregated by the deterministic `Reducer` (no extra LLM call).
5. The answer is pushed as a `ProgressUpdate` notification.

## Using a different local model

Edit `llm_provider.model` in `.ahma/decompose.json`:

```json
"llm_provider": {
    "base_url": "http://localhost:11434/v1",
    "model": "llama3.2:3b"
}
```

Any [Ollama](https://ollama.com/library)-compatible model works. Smaller models (`3b`–`7b`) are recommended for sub-tasks so they complete quickly in parallel.

## Privacy

All LLM calls are HTTP requests to `localhost` (or whatever `base_url` you configure). No data leaves your machine unless you explicitly point `base_url` at a cloud endpoint.

## See also

- [docs/cluster-scheduler.md](cluster-scheduler.md) — route sub-tasks to other machines you own
- [docs/task-vault.md](task-vault.md) — each sub-task runs in its own vault subdirectory
- [SPEC.md §5.6](../SPEC.md) — decompose tool type specification
