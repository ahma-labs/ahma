# ahma_vault Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a system administrator or security-conscious developer, I want all tasks to run in a structured, isolated filesystem vault so that inputs, temporary files, outputs, and deletion attempts are separated and audited.*

## 2. Acceptance Criteria

- **Vault Structure**: Creates a per-task directory tree at `~/.ahma/tasks/<utc-date>-<slug>-<hex>/` containing:
  - `inputs/` — copies of input files
  - `workdir/` — kernel sandbox scope root
  - `outputs/` — task results/artifacts
  - `trash/` — two-phase deletion staging area
  - `audit.jsonl` — append-only event log
  - `egress.allowlist` — outbound network domain allowlist
- **Vault Lifecycle**: Exposes APIs to create, list, and verify vaults.
- **Two-Phase Delete**: Deletions move files to `trash/` before physical deletion, preventing accidental data loss by AI.
- **Egress Proxy Support**: Supplies the outbound network proxy config from `egress.allowlist`.

## 3. Non-Functional Requirements

- **Audit Security**: `audit.jsonl` must be append-only with synchronous writes to prevent tampering.
- **File Isolation**: Work directory (`workdir/`) must remain isolated at the kernel level.

## 4. Out of Scope

- Implementing the network proxy daemon (handled by workspace egress scheduling).
