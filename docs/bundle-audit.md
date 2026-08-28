# Bundle Checksums and Supply-Chain Audit

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

Before distributing a bundle, checksum it:

```bash
ahma bundle checksum /path/to/bundle-dir
# Creates: /path/to/bundle-dir/bundle.manifest.json
```

The manifest records the SHA-256 of every `.json` file in the directory, except
its own (a file cannot contain its own digest). `ahma bundle sign` still works as
a deprecated alias.

## Checking a bundle against its manifest

```bash
ahma bundle verify /path/to/bundle-dir
# PASS: every file matches the manifest
# FAIL: one or more files differ from the manifest
```

> **This is a corruption check, not a signature — and the distinction is not a
> technicality.**
>
> The manifest is unsigned and lives *inside the bundle it describes*. Anyone who
> can change a bundle file can re-run `ahma bundle checksum` in the same motion,
> and `verify` will report PASS. What the check catches is a truncated download, a
> botched copy, a file that changed when nobody meant it to. What it cannot catch
> is anybody who meant it.
>
> Tamper-evidence requires a detached signature verified against a key the
> attacker cannot write. That is the roadmap item below; it is not implemented, so
> `verify` passing is not grounds for trusting a bundle whose origin you do not
> already trust. Run `ahma bundle audit` — which inspects what the tools actually
> *do* — rather than treating a manifest match as clearance.
>
> Earlier releases named this `sign`/`verify`, described the digest as SHA-256
> while computing a 64-bit DJB2 string hash, and carried a "trusted key ring"
> path that nothing read. The mechanism has not become weaker; the description
> has become accurate.

## First-party bundle index

Ahma ships a built-in index at `assets/bundle-index.json` that lists all first-party bundles (rust, python, git, fileutils, github). These are always trusted.

Third-party bundles not in the index require explicit `ahma bundle audit` before use.

## Programmatic use (`ahma_mcp`)

```rust
use ahma_mcp::bundle::{audit_bundle, BundleAuditSeverity, BundleChecksummer, BundleVerifier};
use std::path::Path;

// Audit
let result = audit_bundle(Path::new("/path/to/bundle"))?;
if !result.passed {
    for f in &result.findings {
        eprintln!("[{:?}] {}: {}", f.severity, f.file, f.description);
    }
}

// Write the SHA-256 content manifest
BundleChecksummer::write_manifest(Path::new("/path/to/bundle"))?;

// Re-hash and compare against it. `ok == true` means nothing was corrupted;
// it does not mean the bundle is the one its author published.
let ok = BundleVerifier::new().verify(Path::new("/path/to/bundle"))?;
```

## Roadmap

Real signing — a detached signature over the manifest, verified against a trusted
key the bundle cannot supply — is tracked in SPEC.md §11 as the v0.8 signed bundle
index. Until it lands, nothing in ahma provides tamper-evidence for a bundle;
`ahma bundle audit` (which reads what the tools do) is the control that exists.
Integration with a hosted first-party index and an `--allow-unsigned` flag follow
from it.

## See also

- [docs/custom-tools.md](custom-tools.md) — authoring your own MTDF tool bundles
- [SPEC.md §5](../SPEC.md) — MTDF schema and tool type reference
