# Bundle Signing and Supply-Chain Audit

> **Experimental** — introduced in v0.7.

Ahma can audit and verify MTDF tool bundles (directories of `.ahma/*.json` files) for supply-chain risks. This closes the "plugin marketplace contains malware" attack class by treating third-party bundles as untrusted by default.

## Why bundle auditing?

Third-party security analyses of cloud agent tool ecosystems consistently find a meaningful fraction of available plugins that contain embedded secrets, prompt-injection payloads, or malicious commands. Ahma's bundle auditor applies static analysis rules before any bundle is loaded.

## Auditing a bundle

```bash
ahma bundle audit /path/to/bundle-dir

# Exit non-zero on any finding (not just criticals):
ahma bundle audit /path/to/bundle-dir --strict
```

Example output:

```
Auditing bundle: /path/to/bundle-dir
Files checked: 3 | Findings: 2

[CRITICAL] bad-tool.json: Possible embedded secret matching prefix `sk-`
           Recommendation: Remove credentials from the bundle JSON.

[WARNING ] curl-tool.json: Path-like argument may be missing `format: "path"` — potential sandbox escape
           Recommendation: Add "format": "path" to all path-type arguments.

FAIL Bundle audit found critical issues.
```

## Audit findings

| Severity | Trigger | Risk |
|----------|---------|------|
| `critical` | Embedded secret prefix (`sk-`, `AKIA`, `ghp_`, `glpat-`, `AIzaSy`, …) | Credential exfiltration |
| `critical` | Prompt-injection markers in `description` or `hints` | Agent manipulation |
| `warning` | Path-like argument names without `format: "path"` | Sandbox path escape |
| `warning` | Description length > 512 characters | Possible injection payload |

## Creating a content manifest

Before distributing a bundle, sign it (creates a content-hash manifest):

```bash
ahma bundle sign /path/to/bundle-dir
# Creates: /path/to/bundle-dir/bundle.manifest.json
```

The manifest records the djb2 content hash of every `.json` file in the directory.

## Verifying a bundle

```bash
ahma bundle verify /path/to/bundle-dir
# PASS: all file hashes match the manifest
# FAIL: one or more files have been modified since signing
```

## First-party bundle index

Ahma ships a built-in index at `assets/bundle-index.json` that lists all first-party bundles (rust, python, git, fileutils, github). These are always trusted.

Third-party bundles not in the index require explicit `ahma bundle audit` before use.

## Programmatic use (`ahma_core`)

```rust
use ahma_core::{audit_bundle, BundleAuditSeverity, BundleSigner, BundleVerifier};
use std::path::Path;

// Audit
let result = audit_bundle(Path::new("/path/to/bundle"))?;
if !result.passed {
    for f in &result.findings {
        eprintln!("[{:?}] {}: {}", f.severity, f.file, f.description);
    }
}

// Sign
BundleSigner::sign(Path::new("/path/to/bundle"))?;

// Verify
let verifier = BundleVerifier::new("/home/user/.ahma/keys/trusted");
let ok = verifier.verify(Path::new("/path/to/bundle"))?;
```

## Roadmap

Full asymmetric (ed25519) signing of bundles, integration with a hosted first-party index, and `--allow-unsigned` flag are planned for a later release.

## See also

- [docs/custom-tools.md](custom-tools.md) — authoring your own MTDF tool bundles
- [SPEC.md §5](../SPEC.md) — MTDF schema and tool type reference
