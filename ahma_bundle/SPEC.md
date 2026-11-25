# ahma_bundle Crate Specification

* **Status**: Approved (Experimental feature)
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_mcp` (`ahma bundle audit|checksum|verify`, re-exported as `ahma_mcp::bundle`)

## 1. User Story / Problem Statement

*As someone about to load a third-party MTDF bundle, I want to see what its tools would
actually do before ahma runs them, because loading a bundle lets its author choose the
commands ahma executes.*

## 2. Acceptance Criteria

- `audit_bundle` scans every `*.json` tool definition in a directory and reports findings
  by severity: embedded secrets, prompt-injection text and over-long text in the top-level
  `description`, and a file that names a path-like key with no `format: "path"` anywhere.
  `--strict` fails the audit on any finding. Still open: injection text in `hints` and
  exfiltration-shaped command patterns are not checked.
- `BundleChecksummer` writes a SHA-256 manifest of the bundle's files; `BundleVerifier`
  re-hashes the files the manifest lists and compares; a `.json` file added after
  checksumming is not detected (open). The checksum detects **corruption only**; it is not a signature:
  the manifest is unsigned and lives inside the bundle, so anyone who can edit a file can
  regenerate it. No surface — CLI output, docs, type names — may call the manifest a
  signature or imply it establishes trust.
- There is no load-time gate: an unaudited bundle in a tools directory still loads.

## 3. Non-Functional Requirements

- Pure file inspection: the audit never executes a tool.

## 4. Out of Scope

- Signing, key rings, trusted indexes, load-time trust gates, and detached signature
  verification against a key the bundle cannot supply (the only form of tamper evidence).
