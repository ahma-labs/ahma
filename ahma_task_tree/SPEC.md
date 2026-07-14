# ahma_task_tree Crate Specification

* **Status**: Approved (reduced scope)
* **Date**: 2026-07-14

## 1. Purpose

`ahma_task_tree` provides the **planning-prompt builder** and the **LLM-plan
step parser** used by the `ahma tui` local-model planning flow. It contains no
execution machinery — it only turns a goal into a planning prompt and turns an
LLM's JSON response back into typed steps.

## 2. Acceptance Criteria

- **Prompt building**: Builds planning, output-summarisation, and
  failure-recovery prompts by filling the shared templates from
  `ahma_common::prompts::AhmaPrompts` (goal, task description, branch context,
  step budget, failed-step details, remaining steps).
- **Plan parsing**: Parses an LLM JSON plan response into typed steps
  (`ParsedStep`: task, type, optional command/instructions/subgoal, and
  optional sandbox-scope / allowed-tool / allowed-domain narrowing hints).
- **Recovery parsing**: Parses an LLM recovery response into a
  `RecoveryDecision` (`re_plan` with new steps, or `fail` with a reason).
- **Tolerant input, strict output**: Markdown code fences around the JSON are
  stripped before parsing; a response that still fails to parse returns an
  error with the cleaned payload for diagnosis (never a silent empty plan).

## 3. Out of Scope

- **Task execution of any kind.** The recursive task-tree execution
  orchestrator (recursive decomposition, depth-first traversal, controlled
  parallelism, backtracking/recovery execution, dynamic scoping) that once
  lived in this crate was **removed** because nothing in the shipped product
  invoked it — no `tool_type: task_tree` config ships and no handler is
  registered. See root [SPEC.md §5.6](../SPEC.md) ("Removed tool types") and
  recover the orchestrator from git history if that roadmap feature is
  revived.
- Scheduling tasks to remote machines (handled by `ahma_cluster`).

## 4. License

AGPL-3.0-or-later.
