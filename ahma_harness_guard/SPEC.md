# ahma_harness_guard Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: no workspace crate
* **Used by**: `ahma_mcp` (always on in the MCP service), re-exported as `ahma_mcp::harness_guard`

## 1. User Story / Problem Statement

*As a small or local model calling ahma's tools, I want near-miss calls repaired and my own
retry loops stopped, so that one malformed call does not burn my whole turn budget.*

## 2. Acceptance Criteria

- `heal_tool_name` and `heal_tool_arguments` map other harnesses' tool and argument names
  as root R26.6 requires (`TOOL_ALIASES`, `rename_args`). Beyond that, `heal_tool_name` maps
  an unknown name to a known one within Levenshtein distance 2, and `heal_tool_arguments`
  repairs argument-shape mistakes (for example a string where an array is expected);
  `clean_json_trailing_commas` repairs trailing-comma JSON.
- `LoopDetector` blocks a call repeated identically after it has failed 3 times, answering
  with a `LOOP_DETECTED` message; a success resets the count.
- `HarnessGuard` runs these as an ordered pipeline of `ToolGuard`s (name healing, argument
  healing, loop detection) on every tool call. It is enabled for every client, not only
  under `--small-model-harness`: self-correction is safe for any model.

## 3. Non-Functional Requirements

- Pure: no I/O, no dependency on the MCP engine.

## 4. Out of Scope

- Coaching hints injected into the chat agent's conversation (`ahma_core`, governed by
  `--small-model-harness`).
