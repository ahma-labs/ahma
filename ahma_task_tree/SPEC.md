# ahma_task_tree Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a user or agent with a complex, multi-step problem, I want the system to dynamically decompose the goal into a recursive tree of subtasks, interleave tool calls with reasoning, and recover from intermediate failures automatically.*

## 2. Acceptance Criteria

- **Recursive Decomposition**: Evaluates whether a task is atomic or needs further decomposition, supporting tree depths up to `max_depth` (default 4).
- **Depth-First Traversal**: Traverses and executes tasks in depth-first order by default.
- **Controlled Parallelism**: Supports parallel execution of sibling subtasks when annotated by the planner.
- **Context Branching**: Packs only parent summaries and completed sibling results in the context window, keeping token counts low.
- **Summarisation Pipeline**: Automatically summarizes tool stdout/stderr exceeding threshold size before feeding to parent context.
- **Backtracking & Recovery**: If a leaf subtask fails, re-invokes the parent planner to retry, re-plan, escalate, or abort.
- **Dynamic Scoping**: Restricts child subtask sandbox scopes and network permissions to be narrower than (or equal to) parent scopes.

## 3. Non-Functional Requirements

- **Token Efficiency**: Keeps context size under 3,000 tokens even at depth 4.
- **Robustness**: Enforces safety limits against infinite decomposition loops.

## 4. Out of Scope

- Scheduling tasks to remote machines (handled by `ahma_cluster`).
