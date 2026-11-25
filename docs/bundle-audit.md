# Bundle Checksums and Supply-Chain Audit

> **Experimental.**

`ahma bundle audit` statically checks a directory of MTDF `.json` tool files for embedded secrets, prompt-injection text and missing path validation. You run it; nothing runs it when a bundle loads.

## Why bundle auditing?

Third-party analyses of agent tool ecosystems keep finding plugins with embedded secrets, prompt-injection payloads or malicious commands. The checks here are substring heuristics: a clean report means none matched, not that the bundle is safe. Read what each tool's `command` does.

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
         Recommendation: Add `"format": "path"` to all path-type arguments.

Error: Bundle audit found critical issues.
```

## Audit findings

| Severity | Trigger | Risk |
|----------|---------|------|
| `critical` | Embedded secret prefix (`sk-`, `AKIA`, `ghp_`, `glpat-`, `AIzaSy`, …) | Credential exfiltration |
| `critical` | Injection marker (e.g. `ignore previous`, `jailbreak`, `system:`, `assistant:`) in the top-level `description` | Agent manipulation |
| `warning` | File mentions a `path`/`file`/`dir` key but no `"format": "path"` anywhere (one warning per file) | Sandbox path escape |
| `warning` | Top-level `description` longer than 512 characters ("Unusually long description — possible prompt injection payload") | Possible injection payload |

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
# PASS: every file the manifest lists is unchanged
# FAIL: a listed file is missing or differs
```

A `.json` file added after `checksum` is **not** detected.

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

## Trust

Nothing in ahma gates loading on trust: a bundle placed in a tools directory is loaded
whether or not it was audited. Run `ahma bundle audit` on any third-party bundle before
you put it there.

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
key the bundle cannot supply — is not implemented. Until it is, nothing in ahma provides tamper-evidence for a bundle;
`ahma bundle audit` (which reads what the tools do) is the control that exists.

## See also

- [docs/custom-tools.md](custom-tools.md) — authoring your own MTDF tool bundles
- [SPEC.md §5](../SPEC.md) — MTDF schema and tool type reference
