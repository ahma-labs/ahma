//! Install receipts: what an installer verified, kept beside a binary it then changed.
//!
//! On macOS an installer re-signs the release binary ad hoc with the hardened runtime
//! when it does not already carry a Developer ID signature (root SPEC R-SIGN.1). That
//! rewrites the binary's code signature, so the installed file's SHA-256 is no longer
//! the one the release attestation names, and `ahma verify --self` cannot find an
//! attestation for it. Without a record, a binary re-signed by design is
//! indistinguishable from a modified one.
//!
//! The installer therefore writes `<binary>.install-receipt` next to the binary
//! whenever the installed bytes differ from the verified release bytes. It records what
//! was verified and both digests. `ahma verify` reads it back and reports the
//! difference as what it is, instead of failing as if the binary had been tampered
//! with.
//!
//! A receipt is a local record, not a signature: anything that can rewrite the binary
//! can rewrite its receipt too. It explains a digest mismatch; it proves nothing to
//! someone who does not trust the machine it is on. It is only ever honoured for the
//! exact bytes it names (`installed_sha256`), so a receipt left over from an earlier
//! install never vouches for a later binary.
//!
//! The format is `key=value` lines with `#` comments, so `scripts/install.sh` writes
//! the same file with `printf`. Unknown keys are ignored.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Suffix appended to the binary's file name: `ahma` → `ahma.install-receipt`.
pub const RECEIPT_SUFFIX: &str = "install-receipt";

/// The installer's record of a verified install whose bytes it then changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReceipt {
    /// Who installed it: `ahma update` or `scripts/install.sh`.
    pub installer: String,
    /// The release version installed, or empty if unknown.
    pub version: String,
    /// The artifact whose GitHub Build Provenance attestation was verified: the release
    /// archive (`ahma update`) or the release binary itself (`scripts/install.sh`).
    pub verified_artifact: String,
    /// SHA-256 of `verified_artifact`.
    pub verified_sha256: String,
    /// SHA-256 of the binary exactly as released, before the installer changed it.
    pub released_sha256: String,
    /// SHA-256 of the binary as installed, after the installer changed it.
    pub installed_sha256: String,
}

impl InstallReceipt {
    /// The receipt file's contents.
    pub fn render(&self) -> String {
        format!(
            "# ahma install receipt (SPEC R-SIGN.1; see docs/release-signing.md).\n\
             # The installer verified the release, then re-signed the binary on this machine,\n\
             # which changes its SHA-256. This is a local record, not a signature.\n\
             installer={}\n\
             version={}\n\
             verified_artifact={}\n\
             verified_sha256={}\n\
             released_sha256={}\n\
             installed_sha256={}\n",
            self.installer,
            self.version,
            self.verified_artifact,
            self.verified_sha256,
            self.released_sha256,
            self.installed_sha256,
        )
    }

    /// Parse a receipt file's contents.
    ///
    /// The three digests and `verified_artifact` are required, and each digest must be
    /// 64 lowercase hex characters: a receipt that does not say exactly which bytes it
    /// covers is no receipt at all.
    pub fn parse(text: &str) -> Result<Self> {
        let mut installer = None;
        let mut version = None;
        let mut verified_artifact = None;
        let mut verified_sha256 = None;
        let mut released_sha256 = None;
        let mut installed_sha256 = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("install receipt line is not key=value: {line:?}");
            };
            let value = value.trim().to_string();
            match key.trim() {
                "installer" => installer = Some(value),
                "version" => version = Some(value),
                "verified_artifact" => verified_artifact = Some(value),
                "verified_sha256" => verified_sha256 = Some(value),
                "released_sha256" => released_sha256 = Some(value),
                "installed_sha256" => installed_sha256 = Some(value),
                _ => {}
            }
        }

        let verified_artifact = verified_artifact
            .filter(|v| !v.is_empty())
            .context("install receipt has no verified_artifact")?;
        Ok(Self {
            installer: installer.unwrap_or_default(),
            version: version.unwrap_or_default(),
            verified_artifact,
            verified_sha256: require_digest("verified_sha256", verified_sha256)?,
            released_sha256: require_digest("released_sha256", released_sha256)?,
            installed_sha256: require_digest("installed_sha256", installed_sha256)?,
        })
    }

    /// What `ahma verify` prints for a binary this receipt covers.
    pub fn explain(&self, binary: &Path) -> String {
        let installer = non_empty_or(&self.installer, "the installer");
        let version = non_empty_or(&self.version, "unknown version");
        format!(
            "Verified at install, then re-signed on this machine.\n\
             {binary} (sha256:{installed}) is not byte-identical to a released artifact,\n\
             so no attestation can name it. That is by design: {installer} verified the\n\
             release, then re-signed the binary ad hoc with the hardened runtime (SPEC\n\
             R-SIGN.1), which rewrites its code signature and so its SHA-256.\n\
             Install receipt {receipt}:\n\
             \x20 verified   {artifact} (sha256:{verified}): build-provenance attestation passed\n\
             \x20 released   ahma {version} binary sha256:{released}\n\
             \x20 installed  sha256:{installed}: matches this file\n\
             The receipt is a local record, not a signature: whatever can rewrite this binary\n\
             can rewrite the receipt too. To check the release independently, run\n\
             `gh attestation verify <release archive> --repo ahma-labs/ahma`, or reinstall\n\
             with `ahma update --force`.",
            binary = binary.display(),
            receipt = receipt_path(binary).display(),
            artifact = self.verified_artifact,
            verified = self.verified_sha256,
            released = self.released_sha256,
            installed = self.installed_sha256,
        )
    }
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

fn require_digest(key: &str, value: Option<String>) -> Result<String> {
    let value = value.with_context(|| format!("install receipt has no {key}"))?;
    let is_sha256 = value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !is_sha256 {
        bail!("install receipt {key} is not a lowercase hex SHA-256: {value:?}");
    }
    Ok(value)
}

/// Where the receipt for `binary` lives: beside it, `<file name>.install-receipt`.
pub fn receipt_path(binary: &Path) -> PathBuf {
    let mut name = binary
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".");
    name.push(RECEIPT_SUFFIX);
    binary.with_file_name(name)
}

/// The receipt beside `binary`, if there is one **and** it names `binary`'s current bytes.
///
/// A missing, unreadable, malformed or stale receipt is `None`: the caller then
/// verifies the binary strictly, exactly as if no receipt existed.
pub async fn read_matching(binary: &Path) -> Option<InstallReceipt> {
    let path = receipt_path(binary);
    let text = tokio::fs::read_to_string(&path).await.ok()?;
    let receipt = match InstallReceipt::parse(&text) {
        Ok(receipt) => receipt,
        Err(e) => {
            tracing::warn!("Ignoring install receipt {}: {e:#}", path.display());
            return None;
        }
    };
    let bytes = tokio::fs::read(binary).await.ok()?;
    let actual = crate::sha256_hex(&bytes);
    if actual != receipt.installed_sha256 {
        tracing::debug!(
            "Ignoring install receipt {}: it covers sha256:{}, the binary is sha256:{actual}",
            path.display(),
            receipt.installed_sha256
        );
        return None;
    }
    Some(receipt)
}

/// What an installer verified before installing a binary.
#[derive(Debug, Clone)]
pub struct VerifiedArtifact {
    /// The artifact whose attestation passed (archive or binary file name).
    pub name: String,
    /// Its SHA-256.
    pub sha256: String,
}

/// Write, or remove, the receipt for an installed `binary`.
///
/// A receipt is written only when the release was verified **and** the installed bytes
/// differ from the released bytes (`released_sha256`) — that is, when the installer
/// re-signed it. Otherwise any receipt left by an earlier install is removed: the
/// binary either verifies on its own, or nothing was verified that a receipt could
/// honestly describe.
pub async fn record_install(
    binary: &Path,
    installer: &str,
    version: &str,
    verified: Option<&VerifiedArtifact>,
    released_sha256: &str,
) -> Result<()> {
    let path = receipt_path(binary);
    let bytes = tokio::fs::read(binary)
        .await
        .with_context(|| format!("Failed to read {}", binary.display()))?;
    let installed_sha256 = crate::sha256_hex(&bytes);

    match verified {
        Some(verified) if installed_sha256 != released_sha256 => {
            let receipt = InstallReceipt {
                installer: installer.to_string(),
                version: version.to_string(),
                verified_artifact: verified.name.clone(),
                verified_sha256: verified.sha256.clone(),
                released_sha256: released_sha256.to_string(),
                installed_sha256,
            };
            tokio::fs::write(&path, receipt.render())
                .await
                .with_context(|| format!("Failed to write {}", path.display()))
        }
        _ => match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to remove {}", path.display())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256_hex;

    const D_ARCHIVE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const D_RELEASED: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn sample(installed_sha256: &str) -> InstallReceipt {
        InstallReceipt {
            installer: "ahma update".to_string(),
            version: "0.22.1".to_string(),
            verified_artifact: "ahma-release-darwin-arm64.tar.gz".to_string(),
            verified_sha256: D_ARCHIVE.to_string(),
            released_sha256: D_RELEASED.to_string(),
            installed_sha256: installed_sha256.to_string(),
        }
    }

    #[test]
    fn receipt_path_is_beside_the_binary() {
        let bin = Path::new("/home/me/.local/bin/ahma");
        assert_eq!(
            receipt_path(bin),
            Path::new("/home/me/.local/bin/ahma.install-receipt")
        );
        // Appended, not substituted for an extension.
        assert_eq!(
            receipt_path(Path::new("C:/bin/ahma.exe")),
            Path::new("C:/bin/ahma.exe.install-receipt")
        );
    }

    #[test]
    fn render_then_parse_round_trips() {
        let receipt = sample(&sha256_hex(b"installed"));
        assert_eq!(InstallReceipt::parse(&receipt.render()).unwrap(), receipt);
    }

    /// The exact shape `scripts/install.sh` writes (an unknown key, no installer
    /// comment header, an empty version) must parse.
    #[test]
    fn parse_accepts_the_install_sh_shape() {
        let text = format!(
            "# written by install.sh\n\
             installer=scripts/install.sh\n\
             version=\n\
             verified_artifact=ahma (from ahma-release-darwin-arm64.tar.gz)\n\
             verified_sha256={D_RELEASED}\n\
             released_sha256={D_RELEASED}\n\
             installed_sha256={D_ARCHIVE}\n\
             future_key=ignored\n"
        );
        let receipt = InstallReceipt::parse(&text).unwrap();
        assert_eq!(receipt.installer, "scripts/install.sh");
        assert_eq!(receipt.version, "");
        assert_eq!(receipt.installed_sha256, D_ARCHIVE);
    }

    #[test]
    fn parse_rejects_a_receipt_without_exact_digests() {
        let good = sample(D_ARCHIVE).render();
        for (from, to) in [
            (format!("installed_sha256={D_ARCHIVE}"), String::new()),
            (
                format!("installed_sha256={D_ARCHIVE}"),
                "installed_sha256=abc".to_string(),
            ),
            (
                format!("released_sha256={D_RELEASED}"),
                format!(
                    "released_sha256={}",
                    D_RELEASED.to_uppercase().replace('2', "A")
                ),
            ),
            (
                "verified_artifact=ahma-release-darwin-arm64.tar.gz".to_string(),
                "verified_artifact=".to_string(),
            ),
        ] {
            let bad = good.replace(&from, &to);
            assert!(InstallReceipt::parse(&bad).is_err(), "must reject:\n{bad}");
        }
        assert!(InstallReceipt::parse("not a receipt").is_err());
    }

    #[tokio::test]
    async fn read_matching_honours_only_the_bytes_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("ahma");
        std::fs::write(&bin, b"re-signed bytes").unwrap();

        assert_eq!(read_matching(&bin).await, None, "no receipt");

        let receipt = sample(&sha256_hex(b"re-signed bytes"));
        std::fs::write(receipt_path(&bin), receipt.render()).unwrap();
        assert_eq!(read_matching(&bin).await, Some(receipt));

        // The binary changed after the receipt was written: the receipt is stale.
        std::fs::write(&bin, b"something else").unwrap();
        assert_eq!(read_matching(&bin).await, None, "stale receipt");

        std::fs::write(receipt_path(&bin), "garbage").unwrap();
        assert_eq!(read_matching(&bin).await, None, "malformed receipt");
    }

    #[tokio::test]
    async fn record_install_writes_a_receipt_only_for_verified_changed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("ahma");
        let released = b"released bytes";
        let released_sha = sha256_hex(released);
        let verified = VerifiedArtifact {
            name: "ahma-release-darwin-arm64.tar.gz".to_string(),
            sha256: D_ARCHIVE.to_string(),
        };

        // Re-signed after a verified download: receipt written, and it matches.
        std::fs::write(&bin, b"re-signed bytes").unwrap();
        record_install(
            &bin,
            "ahma update",
            "0.22.1",
            Some(&verified),
            &released_sha,
        )
        .await
        .unwrap();
        let receipt = read_matching(&bin)
            .await
            .expect("receipt for re-signed bytes");
        assert_eq!(receipt.released_sha256, released_sha);
        assert_eq!(receipt.verified_sha256, D_ARCHIVE);
        assert_eq!(receipt.installed_sha256, sha256_hex(b"re-signed bytes"));

        // Installed unchanged (Developer ID kept, or not macOS): the stale receipt goes.
        std::fs::write(&bin, released).unwrap();
        record_install(
            &bin,
            "ahma update",
            "0.22.1",
            Some(&verified),
            &released_sha,
        )
        .await
        .unwrap();
        assert!(
            !receipt_path(&bin).exists(),
            "unchanged bytes need no receipt"
        );

        // Verification bypassed: never a receipt, even for changed bytes.
        std::fs::write(&bin, b"re-signed bytes").unwrap();
        record_install(&bin, "ahma update", "0.22.1", None, &released_sha)
            .await
            .unwrap();
        assert!(
            !receipt_path(&bin).exists(),
            "an unverified install must not leave a receipt claiming verification"
        );
    }

    #[test]
    fn explain_names_both_digests_and_the_caveat() {
        let receipt = sample(D_ARCHIVE);
        let text = receipt.explain(Path::new("/home/me/.local/bin/ahma"));
        assert!(text.contains(D_ARCHIVE), "{text}");
        assert!(text.contains(D_RELEASED), "{text}");
        assert!(text.contains("re-signed"), "{text}");
        assert!(text.contains("not a signature"), "{text}");
        assert!(text.contains("ahma.install-receipt"), "{text}");
    }
}
