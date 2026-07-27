# ahma_test_support Crate Specification

* **Status**: Approved
* **Date**: 2026-07-27

## 1. User Story / Problem Statement

*As a test anywhere in the workspace, I want platform-correct path helpers, so that tests do not hardcode `/tmp` or `/dev/null` and then fail on the Windows CI runner — the single most common source of platform-only test breakage.*

## 2. Acceptance Criteria

- **`test_temp_path(name)`**: A path inside `std::env::temp_dir()`, correct on all platforms. Tests MUST use this rather than a literal `/tmp` or `/var/folders` path.
- **`test_out_of_scope_path()`**: A path guaranteed to fall outside any sandbox scope, for asserting that confinement is enforced.
- **`test_blocked_device_path()`**: The platform device path — `/dev/null` on Unix, `NUL` on Windows.
- **`test_abs(&["a", "b"])` and `test_root()`**: Platform-rooted absolute paths, so tests never assume `/` is the filesystem root (`C:\` and UNC roots differ).
- **No Production Dependencies**: MUST NOT depend on production crate internals, so a refactor of `ahma_mcp` cannot break the test harness.

## 3. Non-Functional Requirements

- **Unpublished**: `publish = false`. This crate exists only to serve the workspace's own tests.
- **Cross-Platform By Construction**: Every helper's contract is defined in terms of platform-correct behaviour, so that a test written on macOS passes unchanged on Windows. Helpers MUST NOT be `#[cfg(unix)]`-gated when a cross-platform form exists.

## 4. Out of Scope

- MCP test clients and in-process service construction (`create_in_process_mcp_from_dir`, `ClientBuilder`, `spawn_http_bridge`) — those live with the crates whose wiring they exercise.
- Timeout scaling for slow CI runners — see `ahma_common::timeouts`.
