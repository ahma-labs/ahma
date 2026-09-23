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
  by severity: embedded secrets, prompt-injection text in `description`/`hints`, path
  arguments without `format: "path"`, and exfiltration-shaped command patterns.
  `--strict` fails the audit on any finding.
- `BundleChecksummer` writes a SHA-256 manifest of the bundle's files; `BundleVerifier`
  re-hashes and compares. This detects **corruption only**. The manifest is unsigned and
  lives inside the bundle, so anyone who can edit a file can regenerate it.
- No surface — CLI output, docs, type names — may call the manifest a signature or imply it
  establishes trust.
- Nothing gates loading on trust: an unaudited bundle in a tools directory still loads.

## 3. Non-Functional Requirements

- Pure file inspection: the audit never executes a tool.

## 4. Out of Scope

- Signing, key rings, trusted indexes and load-time trust gates. Tamper evidence needs a
  detached signature verified against a key the bundle cannot supply; it is not implemented.
