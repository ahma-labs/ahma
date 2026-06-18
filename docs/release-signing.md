# Release Trust Model — SLSA Level 3 via Sigstore

Ahma releases are verified using **GitHub Build Provenance Attestations** backed by
[Sigstore](https://sigstore.dev). There is no private key to manage, no secret to rotate,
and no embedded public keys in the source tree.

## Trust model

```
paulirotta/ahma main branch
        │
        ▼
GitHub Actions: build.yml
        │
        ├──► Release binary / archive
        │
        └──► actions/attest-build-provenance@v2
                │
                ▼
        Sigstore bundle:
          - Ephemeral X.509 cert from Fulcio CA
            (bound to this repo's GitHub OIDC identity)
          - Rekor transparency log entry
                │
                ▼
        ahma_mcp::update::verify::verify_artifact(path)
          (single Rust implementation used by ahma verify and ahma update)
```

Every release archive and raw binary is attested by `actions/attest-build-provenance`.
The attestation is:

- **Keyless**: signed by an ephemeral X.509 certificate from Sigstore's Fulcio CA. The
  certificate is valid for ~10 minutes, exists only for the duration of the workflow run,
  and is cryptographically bound to the workflow's GitHub Actions OIDC identity
  (`paulirotta/ahma`, `refs/heads/main`). There is no private key to leak or rotate.
- **Transparency-logged**: every attestation is recorded in Sigstore's Rekor append-only
  log. Anyone can audit it independently.
- **Verifiable by standard tooling**: `gh attestation verify`, `cosign verify-attestation`,
  or any Sigstore-compatible client.

## What is attested

Every release from v0.10.0 onwards attests:
- `ahma-release-{platform}.tar.gz` — the release archive (Linux/macOS)
- `ahma-release-{platform}.zip` — the release archive (Windows)
- `dist/ahma` / `dist/ahma.exe` — the raw binary extracted from the archive

Attestations are stored in GitHub's Attestation API, queryable by artifact SHA-256.

## How `ahma update` and `ahma verify` work

The single Rust verification module is `ahma_mcp/src/update/verify.rs`.
It is called from three places:

1. **`ahma verify <path>`** — explicit CLI verification of any file.
2. **`ahma verify --self`** — verifies the running binary; called by install scripts
   after download+extract as a post-install smoke test.
3. **`ahma update`** — verifies each downloaded archive before installing.

The verification flow:
1. Compute the artifact's SHA-256 digest.
2. Call the GitHub Attestation API:
   `GET /repos/paulirotta/ahma/attestations/sha256:{digest}`
3. Verify the returned Sigstore bundle against GitHub's trust root (Fulcio + Rekor).
4. Confirm the certificate's identity encodes `paulirotta/ahma` as the issuing workflow.

## Out-of-band verification

Any user or security team can verify a release artifact without installing ahma:

**Using the `gh` CLI (recommended):**
```bash
gh attestation verify ahma-release-linux-x86_64.tar.gz \
  --repo paulirotta/ahma
```

**Using `cosign`:**
```bash
cosign verify-blob \
  --bundle ahma-release-linux-x86_64.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/paulirotta/ahma/.+@refs/heads/main$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ahma-release-linux-x86_64.tar.gz
```

**Programmatically** (for CI/CD pipelines):
```bash
# Download and check attestation metadata
gh api repos/paulirotta/ahma/attestations/sha256:$(sha256sum ahma | awk '{print $1}') \
  | jq '.attestations[0].bundle.verificationMaterial.certificate.rawBytes' | base64 -d | openssl x509 -noout -text
```

## Why there are no keys to rotate

Key rotation is a non-concept with this trust model. Each release gets a **new ephemeral
Fulcio certificate** that:
- Lives for ~10 minutes (the duration of one workflow run)
- Is bound to the exact workflow run's OIDC identity
- Is recorded in Rekor's transparency log

An attacker who compromises a developer machine gains nothing, because there is no
long-lived private key anywhere. An attacker who compromises GitHub Actions would need to
impersonate the exact OIDC identity of `paulirotta/ahma` on `refs/heads/main` — which
GitHub's OIDC service refuses to issue outside of a legitimate workflow run.

## Offline / air-gapped use

Set `AHMA_INSECURE_SKIP_VERIFY=1` to bypass attestation verification:

```bash
AHMA_INSECURE_SKIP_VERIFY=1 ahma update
```

Or use `--insecure-skip-verify` with `ahma update`. This skips the online Sigstore check
entirely. Use only in air-gapped environments or when the GitHub API and Rekor are
unreachable.

Alternatively, build from auditable source:
```bash
cargo install --git https://github.com/paulirotta/ahma ahma_bin --bin ahma --root ~/.local --locked
```

## AGPL + Sigstore: two-layer supply chain defence

The `ahma` binary and security-relevant crates are **AGPL-3.0-or-later**. AGPL requires
source disclosure for any distributed or network-accessible modification, closing the route
of shipping a backdoored binary without publishing the changes. The Sigstore attestation
verifies that what you install was built from the published, auditable source by the
official CI pipeline — making an unsigned impostor immediately detectable.

## Known advisories in the verification stack

The Sigstore verification path depends transitively on `tough` (TUF). Accepted/tracked
third-party advisories affecting this stack — including their analysis and the condition
for closing them — are recorded in [security-advisories.md](security-advisories.md).
