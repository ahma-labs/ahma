# ahma_output_optimizer Crate Specification

* **Status**: Dormant — present, tested, not active in production
* **License**: MIT OR Apache-2.0
* **Depends on**: no workspace crate
* **Used by**: `ahma_mcp` (re-exported as `ahma_mcp::output_optimizer`)

## 1. User Story / Problem Statement

*As a token-constrained model, I want repetitive, decorated tool output condensed before it
reaches me, without losing anything I might need.*

## 2. Acceptance Criteria

- Pure building blocks: ANSI/carriage-return stripping, consecutive-line deduplication with
  a `[... repeated N more times]` marker, output fingerprinting, exit-code-aware head/tail
  truncation, command classification and a context-pressure estimate.
- The complete output always remains in the operation's spill file; minimization only
  shapes what is returned inline.

## 3. Current state

The adapter passes every streamed line through `OutputOptimizer::process_streaming_line`,
but the optimizer is never enabled, so output is unchanged. `--minimize-tokens` affects only
the TUI chat prompt. Enabling it correctly needs one optimizer per operation (dedup state
must not be shared across concurrent operations) and a flush of pending repeat counts at
end of stream; until then it stays off.

## 4. Out of Scope

- Changing what is written to the spill file.
