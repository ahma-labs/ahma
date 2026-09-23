# ahma_simplify Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_bin` only, as an optional dependency (feature `simplify`, on by default)

## 1. User Story / Problem Statement

*As a developer or agent, I want the most complex files in a project ranked, with concrete
fix instructions, so that refactoring effort goes where it matters.* User guide:
[docs/simplify.md](../docs/simplify.md).

## 2. Acceptance Criteria

- Scores every source file: Rust through `rust-code-analysis`; Kotlin, Swift and other
  languages through external analyzers with a Lizard fallback.
- Ranks hotspots and renders a Markdown or HTML report plus a structured AI fix prompt.
- `SimplifyArgs` lives in `ahma_common::simplify_args`, so the CLI parser in `ahma_mcp` can
  reserve the subcommand without depending on this crate.
- Built without the `simplify` feature, `ahma simplify` fails with a clear error naming the
  feature.

## 3. Non-Functional Requirements

- **Never a dependency of `ahma_mcp`**: the engine keeps one feature flavour and carries none
  of the analysis toolchain (AGENTS.md, one canonical build flavour).

## 4. Out of Scope

- Applying the fixes it suggests.
