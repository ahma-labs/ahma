# Release Trust Model — SLSA Level 3 via Sigstore

Ahma releases are verified using **GitHub Build Provenance Attestations** backed by
[Sigstore](https://sigstore.dev). There is no private key to manage, no secret to rotate,
and no embedded public keys in the source tree.

## Trust model

```
ahma-labs/ahma main branch
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
        ahma_update::verify::verify_artifact(path)
          (single Rust implementation used by ahma verify and ahma update,
           built directly on the `sigstore` crate)
```

Every release archive and raw binary is attested by `actions/attest-build-provenance`.
The attestation is:

- **Keyless**: signed by an ephemeral X.509 certificate from Sigstore's Fulcio CA. The
  certificate is valid for ~10 minutes, exists only for the duration of the workflow run,
  and is cryptographically bound to the workflow's GitHub Actions OIDC identity
  (`ahma-labs/ahma`, `refs/heads/main`). There is no private key to leak or rotate.
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

The single Rust verification module is `ahma_update/src/verify.rs`.
It is called from three places:

1. **`ahma verify <path>`** — explicit CLI verification of any file.
2. **`ahma verify --self`** — verifies the running binary; the install scripts run it
   on the staged binary before it takes the install path. On macOS an installed binary
   may have been re-signed after verification; see
   [macOS: signing at install](#macos-signing-at-install-and-ahma-verify---self).
3. **`ahma update`** — verifies each downloaded archive before installing.

The verification flow:
1. Compute the artifact's SHA-256 digest.
2. Call the GitHub Attestation API:
   `GET /repos/ahma-labs/ahma/attestations/sha256:{digest}`
3. For each Sigstore bundle returned, require the signed in-toto statement to list
   that digest among its subjects. (A release attests the archive **and** the raw
   binary in one statement, so `ahma verify --self` matches a later subject than
   `ahma update` does. Both are accepted; a bundle for some other artifact is
   rejected here, before any network round trip to the trust root.)
4. Verify the bundle against the public-good Sigstore trust root, fetched over TUF:
   the signing certificate must chain to a Fulcio CA, its embedded Signed
   Certificate Timestamp must verify against the CT-log keys, and the DSSE
   signature must verify over the envelope's pre-authentication encoding.
5. Enforce the identity policy — all three of:
   - OIDC issuer (Fulcio OID `…57264.1.1`) is `https://token.actions.githubusercontent.com`,
   - workflow repository (OID `…57264.1.5`) is exactly `ahma-labs/ahma`,
   - the certificate SAN (`build_signer_uri`) is under `https://github.com/ahma-labs/ahma/`.
6. Bind the Rekor transparency-log entry to the bundle: it must record this payload
   hash, this signature and this signing certificate, and its `integratedTime` must
   fall inside the certificate's ~10-minute validity window.

Any one attestation satisfying all of the above verifies the artifact; if none does,
the error lists why each was rejected.

### Implementation notes

Verification is implemented on the [`sigstore`](https://crates.io/crates/sigstore)
crate (0.14) directly — `sigstore::bundle::verify::Verifier` plus the policy types in
`sigstore::bundle::verify::policy`. There is no wrapper crate: the previous
`sigstore-verification` dependency was removed in v0.19.8, which also removed
`oci-client`, `json-syntax`, an extra `syn` major and two `deny.toml` advisory
exceptions (see [security-advisories.md](security-advisories.md) and
[build-and-test-performance.md](build-and-test-performance.md)).

Two checks are performed by `ahma_update` rather than by `sigstore`, because
`sigstore` 0.14 cannot complete them for GitHub-issued bundles — the subject binding
(step 3), because `sigstore` inspects only `subject[0]`, and the transparency-log
binding (step 6), because `sigstore` recomputes Rekor's `envelopeHash` by
re-serialising the parsed envelope, which does not round-trip. `ahma_update/src/verify.rs`
documents exactly which upstream error is tolerated, and what it re-establishes
before doing so. Both replacements are stricter than the checks they stand in for.

## macOS: signing at install, and `ahma verify --self`

On macOS, after the release has passed attestation verification, `scripts/install.sh` and
`ahma update` decide whether to keep the binary's signature (SPEC R-SIGN.1):

| Release binary's signature | What the installer does |
|---|---|
| Passes `codesign --verify --strict`, leaf authority `Developer ID Application`, flags include `runtime` | Keeps it. The installed file is byte-identical to the release, so `ahma verify --self` finds its attestation. |
| Anything else, such as the linker's ad-hoc signature of a `cargo` build | Re-signs it ad hoc with the hardened runtime (`codesign --force --sign - --options runtime`), which prevents the `CODESIGNING` SIGKILL under memory pressure. This changes the file's SHA-256. |

Any Developer ID is kept: the decision is about runtime stability. Provenance has already
been established by the attestation. `ahma_update::install::keeps_existing_signature` and
`has_developer_id_runtime_signature` in `scripts/install.sh` implement the same check.

**Install receipt.** A re-signed file is no longer the file the attestation names, so a
lookup by its digest finds nothing. Without a record, that looks exactly like tampering. When
the installed bytes differ from the verified release bytes, the installer therefore writes
`ahma.install-receipt` beside the binary. It records:

- the artifact whose attestation passed, and its SHA-256: the release archive for
  `ahma update`, the release binary for `scripts/install.sh`;
- the SHA-256 of the binary as released;
- the SHA-256 of the binary as installed.

`ahma verify --self`, or `ahma verify <installed binary>`, on a file whose SHA-256 matches the
receipt's `installed_sha256` reports both facts and exits 0:

```text
Verified at install, then re-signed on this machine.
~/.local/bin/ahma (sha256:…) is not byte-identical to a released artifact,
so no attestation can name it. …
```

- **A receipt is a local record, not a signature.** Anything that can rewrite the binary can
  rewrite its receipt. For an independent check, verify the release archive
  [out of band](#out-of-band-verification).
- **A receipt for other bytes is ignored.** The binary is then verified strictly, as if there
  were no receipt.
- **Older installs have no receipt.** On macOS `ahma verify --self` then fails, with a note
  naming the likely cause. Run `ahma update --force` to reinstall with a receipt.
- **No receipt when verification was skipped.** `--insecure-skip-verify` (or a retired
  `AHMA_INSECURE_SKIP_*` variable) leaves none, and removes an old one.
- **Uninstall removes it.** `ahma uninstall` deletes the receipt with the binary.

On Linux and Windows the installed binary is never re-signed, so it is always byte-identical
to the release and no receipt is written.

## Out-of-band verification

Any user or security team can verify a release artifact without installing ahma:

**Using the `gh` CLI (recommended):**
```bash
gh attestation verify ahma-release-linux-x86_64.tar.gz \
  --repo ahma-labs/ahma
```

**Using `cosign`:**
```bash
cosign verify-blob \
  --bundle ahma-release-linux-x86_64.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/ahma-labs/ahma/.+@refs/heads/main$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ahma-release-linux-x86_64.tar.gz
```

**Programmatically** (for CI/CD pipelines):
```bash
# Download and check attestation metadata
gh api repos/ahma-labs/ahma/attestations/sha256:$(sha256sum ahma | awk '{print $1}') \
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
impersonate the exact OIDC identity of `ahma-labs/ahma` on `refs/heads/main` — which
GitHub's OIDC service refuses to issue outside of a legitimate workflow run.

## Offline / air-gapped use

Pass `--insecure-skip-verify` to bypass attestation verification:

```bash
ahma update --insecure-skip-verify
```

This skips the online Sigstore check entirely. Use only in air-gapped environments or
when the GitHub API and Rekor are unreachable.

Bypassing verification is deliberately a **CLI flag only** — `AHMA_INSECURE_SKIP_VERIFY`
and `AHMA_INSECURE_SKIP_SIGNATURE` are retired and ignored with a loud warning, because a
security-tier setting must not be reachable from an environment a config file can set
(SPEC R-CFG2.3).

Alternatively, build from auditable source:
```bash
cargo install --git https://github.com/ahma-labs/ahma ahma_bin --bin ahma --root ~/.local --locked
```

## AGPL + Sigstore: two-layer supply chain defence

The `ahma` binary and security-relevant crates are **AGPL-3.0**. AGPL requires
source disclosure for any distributed or network-accessible modification, closing the route
of shipping a backdoored binary without publishing the changes. The Sigstore attestation
verifies that what you install was built from the published, auditable source by the
official CI pipeline — making an unsigned impostor immediately detectable.

## Known advisories in the verification stack

The Sigstore verification path depends transitively on `tough` (TUF). Accepted/tracked
third-party advisories affecting this stack — including their analysis and the condition
for closing them — are recorded in [security-advisories.md](security-advisories.md).
