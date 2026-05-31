# Release Signing and Supply Chain Security

Ahma prebuilt binaries are **cryptographically signed** during the CI release pipeline.
Every release asset is accompanied by a `SHA256SUMS` manifest and a `SHA256SUMS.sig`
signature file. Installers verify both before writing anything to disk.

This document explains the architecture, why it matters, and how to rotate keys.

---

## Why Signing Matters: AGPL + Cryptographic Provenance

The shipped `ahma` binary and all security-relevant product crates
(`ahma_bin`, `ahma_vault`, `ahma_tui`, `ahma_worker`, `ahma_cluster`, etc.)
are licensed **AGPL-3.0-or-later**.

AGPL requires that anyone distributing a modified version — or running a modified
version over a network — must make the corresponding source code available.
This is intentional: it **closes the backdoor route** of shipping a modified binary
without disclosing the changes.

Binary signing adds the final link in that chain:

```
Published source (GitHub, auditable)
        ↓  AGPL: anyone can verify source matches what you distribute
Official CI pipeline (GitHub Actions, build.yml)
        ↓  RSA-2048 signature: proves binary came from this exact pipeline
SHA256SUMS + SHA256SUMS.sig (released as GitHub assets)
        ↓  install.sh / install.ps1 / ahma update: verify before installing
Binary on your machine
```

Together, AGPL and cryptographic signing protect against several **supply chain
attack vectors**:

| Attack vector | Mitigation |
|---------------|------------|
| Backdoored binary from unofficial mirror | Signature verification rejects any binary not produced by the official pipeline |
| Closed-source fork distributed as "ahma" | AGPL requires source disclosure; signing verifies the binary origin |
| DNS/CDN hijack serving a tampered binary | Hash check inside the signed manifest detects modification |
| Compromised GitHub release assets | Signature requires the private key held exclusively in GitHub Actions secrets |
| Malicious `install.sh` served from a compromised CDN | Script embeds the public key; a tampered script that removed verification would itself be visible in the install command |

> [!IMPORTANT]
> **Building from source is always an option.** AGPL means the source is always
> public. Users who cannot verify the binary provenance chain can build from the
> published, auditable source instead:
> ```bash
> cargo install --git https://github.com/paulirotta/ahma ahma_bin --bin ahma --root ~/.local --locked
> ```

---

## What is Signed

Each release produces a **combined `SHA256SUMS`** file containing:

1. SHA-256 hashes of every release archive (`ahma-release-*.tar.gz`, `*.zip`)
2. SHA-256 hashes of the raw extracted binaries (for direct verification of the installed binary)

The `SHA256SUMS.sig` file is an RSA-PKCS1v15-SHA256 signature of `SHA256SUMS`,
produced with the private key that lives only inside the `AHMA_RELEASE_SIGNING_KEY`
GitHub Actions secret.

---

## Verification by Installers

### `install.sh` (Linux / macOS)

```bash
# Downloads SHA256SUMS + SHA256SUMS.sig, verifies, then checks archive hash
curl -sSf https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.sh | bash
```

The script:
1. Downloads `SHA256SUMS` and `SHA256SUMS.sig` from the GitHub release
2. Runs `openssl dgst -sha256 -verify` with the embedded public key
3. Only proceeds to download and install if signature is valid

### `install.ps1` (Windows PowerShell)

Uses .NET `System.Security.Cryptography.RSACryptoServiceProvider` with the
embedded XML public key — no external `openssl` dependency required.

### `ahma update` (Rust updater)

Uses the `ring` crate for RSA-PKCS1-SHA256 verification with the embedded
`PUB_KEY_PEM` constant — cryptographic verification in native Rust, no
subprocess or shell dependency.

### Manual verification

```bash
# Download the manifest and signature
curl -sSfL https://github.com/paulirotta/ahma/releases/latest/download/SHA256SUMS -o /tmp/SHA256SUMS
curl -sSfL https://github.com/paulirotta/ahma/releases/latest/download/SHA256SUMS.sig -o /tmp/SHA256SUMS.sig

# Verify (public key is in the repo)
openssl dgst -sha256 -verify scripts/ahma-release.pub.pem \
  -signature /tmp/SHA256SUMS.sig /tmp/SHA256SUMS
# → Verified OK

# Verify your installed binary
INSTALLED_HASH=$(shasum -a 256 ~/.local/bin/ahma | awk '{print $1}')
grep "$INSTALLED_HASH" /tmp/SHA256SUMS
# → should match ahma-darwin-arm64 (or your platform)
```

---

## Key Architecture: GitHub-Only, Zero Local Exposure

> [!IMPORTANT]
> **Private keys never exist on any developer machine.**
>
> All key generation and rotation happens inside ephemeral GitHub Actions runners.
> The private key exists only:
> 1. In RAM on the runner during key generation (seconds)
> 2. As a 1-day admin-only encrypted artifact (for the operator to set as secret)
> 3. As the GitHub Actions secret `AHMA_RELEASE_SIGNING_KEY` (encrypted at rest,
>    never readable after set, not in logs)

### Key inventory

All five locations embed the **public key** and must be updated atomically during rotation.
The `generate-signing-key` workflow handles all five automatically.

| Location | Format | Updated by |
|----------|--------|------------|
| `scripts/ahma-release.pub.pem` | PEM | Rotation workflow (committed) |
| `scripts/ahma-release.pub.xml` | XML (Windows RSA) | Rotation workflow (committed) |
| `scripts/install.sh` — `PUB_KEY_PEM=` | Embedded PEM | Rotation workflow (committed) |
| `scripts/install.ps1` — `$PUB_KEY_XML =` | Embedded XML | Rotation workflow (committed) |
| `ahma_mcp/src/update/install.rs` — `const PUB_KEY_PEM` | Embedded PEM | Rotation workflow (committed) |
| GitHub secret `AHMA_RELEASE_SIGNING_KEY` | PEM (private) | Operator (after artifact download) |

### Key lifecycle

```
[Actions UI] → Run workflow → action: generate-signing-key
       ↓
  ubuntu-latest ephemeral runner
  openssl genpkey → RSA-2048 in /tmp (tmpfs, RAM-backed)
  openssl rsa -pubout → public key
       ↓  public key patches all 5 embedded locations
  git commit → PR opened ("chore(security): rotate release signing key YYYYMMDD")
       ↓  private key
  upload as 1-day admin-only artifact
  dd /dev/urandom → /tmp/release_key.pem  (overwrite)
  rm /tmp/release_key.pem                 (delete)
       ↓
[Operator] download artifact → set AHMA_RELEASE_SIGNING_KEY secret → merge PR → delete local zip
       ↓
[push to main] → job-publish-release
  echo "$AHMA_RELEASE_SIGNING_KEY" > release_key.pem
  openssl dgst -sha256 -sign release_key.pem -out SHA256SUMS.sig SHA256SUMS
  rm -f release_key.pem
  ↓ SHA256SUMS + SHA256SUMS.sig uploaded as release assets
  ↓ post-publish CI step verifies sig with committed pubkey → "PASSED"
       ↓
[user: curl install.sh | bash]
  Authenticity verified: Release signature is valid.
```

---

## Triggering Key Rotation

### Via GitHub Actions UI (recommended)

1. Go to **Actions** → **Ahma** workflow → **Run workflow**
2. Select `action: generate-signing-key` → **Run workflow**
3. Wait ~60 seconds for the job to complete

### What the workflow does

- Generates a fresh RSA-2048 keypair on an ephemeral runner
- Patches all five embedded public-key locations in the repo
- Uploads the private key as a **1-day admin-only artifact**
- Shreds the private key from runner tmpfs (overwrites with `/dev/urandom` before `rm`)
- Opens a PR with all public-key file changes
- Writes next-steps instructions to the [job summary](#)

### Operator steps (after workflow completes)

> [!CAUTION]
> The artifact expires in **24 hours**. Act promptly.

1. **Download** artifact `signing-key-YYYYMMDD-<run_id>` (only repo admins can download artifacts)
2. **Update the secret**: Settings → Secrets and variables → Actions → `AHMA_RELEASE_SIGNING_KEY` → Update  
   Paste the full contents of the downloaded `release_key.pem` (including `-----BEGIN...-----` lines)
3. **Review and merge** the auto-generated PR (no code changes — only public key files)
4. **Permanently delete** the downloaded zip and any extracted `release_key.pem` from your machine
5. **Record the rotation** in the [Key Rotation Log](#key-rotation-log) below
6. Verify the next release CI run shows `Post-publish signature verification: PASSED`

---

## Rotation Schedule

| Trigger | Timeline | Severity |
|---------|----------|----------|
| **Annual rotation** | Schedule in advance; rotate proactively | Routine |
| **Suspected key compromise** | Within hours | Emergency |
| **Repository admin offboarding** | Within 24h of offboarding | Urgent |
| **Algorithm deprecation** (e.g. RSA-2048 broken) | Immediately | Emergency |
| **GitHub Actions secret leaked** | Within hours | Emergency |

### Emergency rotation procedure

If you suspect the private key has been compromised:

1. **Immediately** clear `AHMA_RELEASE_SIGNING_KEY` (set it to a single space or placeholder)  
   → Any in-progress release will hard-fail ("secret is not set") instead of signing with the compromised key
2. Follow the standard rotation SOP above
3. If a compromised key was used to sign any published release:
   - Issue a [GitHub Security Advisory](https://github.com/paulirotta/ahma/security/advisories)
   - Advise users to reinstall via `cargo install` (source build from auditable source) or verify against the new manifest
   - Yank the affected GitHub release assets if possible

---

## Algorithm Notes

**RSA-2048 with PKCS#1 v1.5 and SHA-256** is used because:

- macOS ships with **LibreSSL** (not OpenSSL). `openssl dgst -sha256 -verify` with RSA-PKCS1 works reliably on all supported platforms without extra flags.
- Ed25519 support in shell-script `openssl` is inconsistent across Linux distros and macOS LibreSSL versions.
- RSA-2048 remains the NIST-recommended minimum for signing through at least 2030.

Future migration to **Ed25519** is possible when we can use a compiled verifier (`ring` in Rust already supports it) instead of relying on the system `openssl` binary in shell scripts.

---

## Key Rotation Log

| Date | Reason | Triggered by | PR |
|------|--------|-------------|-----|
| *(first rotation pending)* | Initial setup — key never existed | — | — |

Update this table after each rotation as part of the operator SOP.
