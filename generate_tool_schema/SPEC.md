# generate_tool_schema Crate Specification

* **Status**: Approved
* **Date**: 2026-07-27

## 1. User Story / Problem Statement

*As a tool author writing a `.ahma/*.json` definition, I want a published JSON Schema for the Multi-Tool Definition Format, so that my editor validates the file as I write it instead of the error surfacing as a server startup failure.*

## 2. Acceptance Criteria

- **Schema Generation**: Emits the JSON Schema for the Multi-Tool Definition Format (MTDF) derived from `ahma_mcp::config::ToolConfig`, so the schema cannot drift from the type that actually parses tool definitions.
- **Output Location**: Writes `[OUTPUT_DIR]/mtdf-schema.json`, where `OUTPUT_DIR` defaults to `docs`.
- **Invocation**: `cargo run -p generate_tool_schema -- [OUTPUT_DIR]`.

## 3. Non-Functional Requirements

- **Single Source Of Truth**: The schema MUST be generated from the Rust type rather than hand-maintained. A hand-written schema would accept configurations the parser rejects, and vice versa.
- **Deterministic Output**: The same `ToolConfig` produces byte-identical schema output, so a regenerated schema shows an empty diff when nothing changed.

## 4. Out of Scope

- Validating tool definitions at runtime — that is `ahma tool validate` and the startup validation in `ahma_mcp`, which reject invalid configs with actionable messages.
- Defining the MTDF itself — see root [SPEC.md](../SPEC.md) §5.
