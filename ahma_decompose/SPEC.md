# ahma_decompose Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a user with a complex question, I want the system to decompose it into independent subtasks, execute them in parallel on local LLMs, and aggregate the results so that I get a fast and private answer.*

## 2. Acceptance Criteria

- **Decomposition**: Splitting a business/technical question into up to `max_subtasks` subtasks.
- **Parallel Dispatch**: Running subtask LLM queries concurrently, bounded by `max_concurrent`.
- **Deterministic Reducer**: Combines outputs using Rust-native reduction algorithms (e.g. `summarize`, `extract_fields`, `classify`, `concat`, `first`) without requiring additional LLM calls.
- **Zero Cloud Egress**: Designed to run primarily against local LLM backends (like Ollama).

## 3. Non-Functional Requirements

- **Concurrency Limits**: Must prevent local machine starvation by limiting concurrent tasks.
- **Robustness**: If a subtask LLM call fails or times out, the reducer handles it gracefully.

## 4. Out of Scope

- Recursive subtask creation (handled by `ahma_task_tree`).
