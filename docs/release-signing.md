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

This is about **provenance**. The macOS Developer-ID signature below is a separate layer that
Apple requires, and it does use long-lived credentials. They prove nothing about provenance
and `ahma` never checks them: holding them would let someone make a binary that macOS
accepts, but not one that `ahma update` or `ahma verify` accepts.

## macOS: Developer-ID signing and notarization

Status: **wired, inactive.** The release workflow signs and notarizes the macOS binary as
soon as the maintainer adds the secrets below. Until then, release binaries are signed ad hoc
by the linker, as before. SPEC R-SIGN.1 requires the Developer-ID signature, and SPEC §11
tracks it until the secrets are added.

**Why.** On Apple Silicon, ad-hoc signed code pages can fail re-validation when they fault
back in after eviction under memory pressure, and the kernel `SIGKILL`s the process (SPEC
R-SIGN). A Developer-ID signature with the hardened runtime and a secure timestamp is the
stable identity the OS expects. Notarization also lets a quarantined copy, such as a tarball
downloaded in a browser, run without the "cannot be verified" Gatekeeper block.

### What the workflow does

`job-release-binaries` in `.github/workflows/build.yml`, `darwin-arm64` leg only:

1. Builds the binary.
2. **If `APPLE_DEVELOPER_ID_P12` is set:**
   1. Imports the identity into a throwaway keychain.
   2. Runs `codesign --force --options runtime --timestamp` with the Developer ID.
   3. Checks the result with `codesign --verify --strict`, and confirms it has a
      `Developer ID Application` authority and the `runtime` flag.
   4. Zips the binary with `ditto` and submits it with `xcrun notarytool submit --wait`.
   5. Fails the leg, and prints the notarization log, unless Apple returns `Accepted`.
3. **If it is not set:** emits
   `::notice::Developer-ID signing skipped: APPLE_DEVELOPER_ID_P12 not set (SPEC R-SIGN.1)`
   and carries on.
4. Packages the binary, writes `SHA256SUMS`, uploads, and attests. Only then.
5. Deletes the keychain in an `always()` step.

**The order matters.** Signing rewrites the binary, which changes its SHA-256. The
attestations and `SHA256SUMS` are computed from whatever bytes exist when the "Package" step
runs, and `ahma update` / `ahma verify` find attestations **by digest**. Signing therefore
happens before packaging. That way the published archive, the published `SHA256SUMS` and the
attestation all describe the signed binary. Notarization does not change the file.

The workflow does not run `stapler staple`. A bare Mach-O executable has nowhere to hold a
ticket: only `.app`, `.pkg` and `.dmg` can be stapled. Gatekeeper looks the ticket up online,
using the binary's code-directory hash, the first time it launches a quarantined copy.

The secrets go only to the one step that signs, never to the job's environment. That keeps
the Developer ID private key away from `cargo build`, which runs the build scripts and
proc-macros of every third-party dependency.

### Turning on Developer-ID signing

You need an [Apple Developer Program](https://developer.apple.com/programs/) membership. Only
the **Account Holder** can create a Developer ID certificate.

Add these six **repository secrets** (*Settings → Secrets and variables → Actions*). Set all
six or none. If `APPLE_DEVELOPER_ID_P12` is set and any of the others is missing, the release
leg fails and names every missing secret. It never ships a signed but un-notarized binary.

| Secret | Value |
|---|---|
| `APPLE_DEVELOPER_ID_P12` | Base64 of a `.p12` export of the **Developer ID Application** certificate *with its private key* |
| `APPLE_DEVELOPER_ID_P12_PASSWORD` | The password you set when exporting the `.p12` |
| `APPLE_DEVELOPER_ID_NAME` | The full identity name, e.g. `Developer ID Application: Example Ltd (ABCDE12345)` |
| `APPLE_NOTARY_KEY_P8` | Base64 of the App Store Connect API key file `AuthKey_<KEY_ID>.p8` |
| `APPLE_NOTARY_KEY_ID` | That key's Key ID (10 characters) |
| `APPLE_NOTARY_ISSUER_ID` | The Issuer ID (a UUID) shown above the key list in App Store Connect |

**1. Create and export the Developer ID certificate.**

1. In Xcode, open *Settings → Accounts*, select the team, then *Manage Certificates… → + →
   Developer ID Application*. Or create it at
   [developer.apple.com → Certificates](https://developer.apple.com/account/resources/certificates/list)
   from a CSR made in Keychain Access.
2. In Keychain Access, open *login → My Certificates*. Expand the certificate to confirm it
   has its private key, right-click it, and choose *Export… → Personal Information Exchange
   (.p12)*. Set a strong password.
3. Store the secrets:

```bash
security find-identity -v -p codesigning     # copy the "Developer ID Application: …" name
base64 -i DeveloperID.p12 | gh secret set APPLE_DEVELOPER_ID_P12 --repo ahma-labs/ahma
gh secret set APPLE_DEVELOPER_ID_P12_PASSWORD --repo ahma-labs/ahma    # prompts
gh secret set APPLE_DEVELOPER_ID_NAME --repo ahma-labs/ahma \
  --body 'Developer ID Application: Example Ltd (ABCDE12345)'
rm DeveloperID.p12
```

**2. Create the notarization API key.**

1. In [App Store Connect → Users and Access → Integrations → App Store Connect
   API](https://appstoreconnect.apple.com/access/integrations/api), under *Team Keys*,
   generate a key with the **Developer** role.
2. Download `AuthKey_<KEY_ID>.p8`. Apple lets you download it **only once**.
3. Note the Key ID and the Issuer ID.
4. Check the key works, then store it:

```bash
xcrun notarytool history --key AuthKey_ABC123DEFG.p8 \
  --key-id ABC123DEFG --issuer 00000000-0000-0000-0000-000000000000
base64 -i AuthKey_ABC123DEFG.p8 | gh secret set APPLE_NOTARY_KEY_P8 --repo ahma-labs/ahma
gh secret set APPLE_NOTARY_KEY_ID --repo ahma-labs/ahma --body ABC123DEFG
gh secret set APPLE_NOTARY_ISSUER_ID --repo ahma-labs/ahma \
  --body 00000000-0000-0000-0000-000000000000
```

The next version bump that releases will sign and notarize. Look for
`Developer-ID signed and notarized …` in the `Release Binaries (darwin-arm64)` log, and check
a published binary:

```bash
tar -xzf ahma-release-darwin-arm64.tar.gz ahma
codesign --verify --strict --verbose=2 ahma
codesign --display --verbose=4 ahma 2>&1 | grep -E '^(Authority|Timestamp|CodeDirectory)'
# Authority=Developer ID Application: …   flags=0x10000(runtime)
```

**Rotation.** A Developer ID Application certificate is valid for five years. To replace the
certificate or the API key, overwrite the matching secrets; the workflow needs no change. To
turn signing off, delete `APPLE_DEVELOPER_ID_P12`.

### What changes for users

| | Before secrets are set (today) | After |
|---|---|---|
| macOS binary in the release archive | Linker ad-hoc signature | Developer ID, hardened runtime, secure timestamp, notarized |
| Browser-downloaded (quarantined) tarball | Gatekeeper blocks it; you need `xattr -d com.apple.quarantine` | Runs. Gatekeeper checks the ticket online on first launch |
| `ahma update`, `ahma verify`, `gh attestation verify`, `SHA256SUMS` | Work | Work unchanged. They are computed over the signed bytes |
| `scripts/install.sh`, `ahma update` | Re-sign the installed copy ad hoc with `--options runtime` (R-SIGN.1, local part) | Keep the Developer ID signature, so the installed file is byte-identical to the release and `ahma verify --self` finds its attestation; anything else is still re-signed ad hoc (see "macOS: signing at install") |
| Linux, Windows | — | No change |

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
