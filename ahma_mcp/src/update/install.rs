//! Download, verify, and install release archives.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::platform::{ArchiveFormat, Platform};
use super::release::{ReleaseAsset, parse_checksum_line};

/// Install a release archive into `install_dir`.
pub async fn install_release_asset(
    client: &reqwest::Client,
    asset: &ReleaseAsset,
    platform: &Platform,
    install_dir: &Path,
    dry_run: bool,
    insecure_skip_signature: bool,
) -> Result<PathBuf> {
    let temp_dir = tempfile::tempdir().context("Failed to create temporary directory")?;
    let archive_path = temp_dir.path().join(&asset.asset_name);

    if dry_run {
        println!("[dry-run] Would download {}", asset.download_url);
        println!("[dry-run] Would install to {}", install_dir.display());
        return Ok(install_dir.join(platform.binary_name()));
    }

    println!("Downloading {}...", asset.download_url);
    download_file(client, &asset.download_url, &archive_path).await?;

    if let Some(expected) =
        fetch_archive_checksum(client, asset, platform, insecure_skip_signature).await?
    {
        verify_file_checksum(&archive_path, &expected)?;
        println!("Checksum verified.");
    } else {
        eprintln!("Warning: no SHA256 checksum found; skipping verification.");
    }

    let extracted = extract_archive(&archive_path, platform, temp_dir.path()).await?;
    let target = install_dir.join(platform.binary_name());

    fs::create_dir_all(install_dir)
        .await
        .with_context(|| format!("Failed to create {}", install_dir.display()))?;

    install_binary(&extracted, &target).await?;
    cleanup_legacy_binaries(install_dir).await?;

    Ok(target)
}

async fn download_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("Download failed for {url}"))?;

    let bytes = response
        .bytes()
        .await
        .context("Failed to read download body")?;
    let mut file = fs::File::create(dest)
        .await
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    Ok(())
}

async fn fetch_archive_checksum(
    client: &reqwest::Client,
    asset: &ReleaseAsset,
    platform: &Platform,
    insecure_skip_signature: bool,
) -> Result<Option<String>> {
    // Per-archive SHA256SUMS is bundled inside the release archive; for pre-check we
    // attempt the combined root SHA256SUMS published alongside release assets.
    let sums_url = asset
        .download_url
        .rsplit_once('/')
        .map(|(base, _)| format!("{base}/SHA256SUMS"))
        .unwrap_or_else(|| {
            format!(
                "https://github.com/paulirotta/ahma/releases/download/{}/SHA256SUMS",
                asset.tag
            )
        });

    let response = client.get(&sums_url).send().await?;
    if !response.status().is_success() {
        return Ok(None);
    }

    let body = response.text().await.unwrap_or_default();

    if insecure_skip_signature {
        eprintln!("WARNING: Skipping cryptographic release signature verification!");
    } else {
        let sig_url = format!("{sums_url}.sig");
        let sig_response = client.get(&sig_url).send().await?;
        if !sig_response.status().is_success() {
            bail!(
                "Failed to download release signature file from {sig_url} (HTTP {}).\n\
                 This might be because the release is unsigned or the signing key is not configured.\n\
                 If you trust this build and wish to bypass, use --insecure-skip-signature.",
                sig_response.status()
            );
        }
        let sig_bytes = sig_response.bytes().await?;

        // Cryptographically verify signature
        verify_release_signature(body.as_bytes(), &sig_bytes)?;
        println!(
            "Release signature verified: SHA256SUMS.sig is signed by the official private key."
        );
    }

    let binary_name = platform.binary_name();
    for line in body.lines() {
        if let Some(hash) = parse_checksum_line(line, &asset.asset_name) {
            return Ok(Some(hash));
        }
        if let Some(hash) = parse_checksum_line(line, binary_name) {
            return Ok(Some(hash));
        }
    }
    Ok(None)
}

fn verify_file_checksum(path: &Path, expected_hex: &str) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    let actual = format!("{digest:x}");
    if actual != expected_hex {
        bail!(
            "Checksum mismatch for {}: expected {expected_hex}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

async fn extract_archive(archive_path: &Path, platform: &Platform, dest: &Path) -> Result<PathBuf> {
    match platform.archive_ext {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, dest, platform),
        ArchiveFormat::Zip => extract_zip(archive_path, dest, platform),
    }
}

fn extract_tar_gz(archive_path: &Path, dest: &Path, platform: &Platform) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .context("Failed to extract tar.gz archive")?;

    let binary = dest.join(platform.binary_name());
    if binary.exists() {
        return Ok(binary);
    }
    bail!("Binary {} not found in archive", platform.binary_name());
}

fn extract_zip(archive_path: &Path, dest: &Path, platform: &Platform) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).context("Failed to open zip archive")?;
    archive
        .extract(dest)
        .context("Failed to extract zip archive")?;

    let binary = dest.join(platform.binary_name());
    if binary.exists() {
        return Ok(binary);
    }
    bail!("Binary {} not found in archive", platform.binary_name());
}

async fn install_binary(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if target.exists() {
            let backup = target.with_extension("old");
            let _ = fs::remove_file(&backup).await;
            fs::rename(target, &backup).await.ok();
        }
        fs::copy(source, target)
            .await
            .with_context(|| format!("Failed to copy binary to {}", target.display()))?;
        let mut perms = fs::metadata(target).await?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(target, perms).await?;
    }

    #[cfg(windows)]
    {
        if target.exists() {
            // Avoid overwriting a running executable when possible.
            let staged = target.with_extension("new.exe");
            fs::copy(source, &staged).await?;
            match fs::rename(&staged, target).await {
                Ok(()) => {}
                Err(_) => {
                    bail!(
                        "Could not replace {}. Close running ahma/MCP clients and retry, \
                         or manually replace the binary after exit.",
                        target.display()
                    );
                }
            }
        } else {
            fs::copy(source, target).await?;
        }
    }

    println!("Installed {}", target.display());
    Ok(())
}

async fn cleanup_legacy_binaries(install_dir: &Path) -> Result<()> {
    let legacy_names = if cfg!(target_os = "windows") {
        vec!["ahma-simplify.exe"]
    } else {
        vec!["ahma-simplify"]
    };

    for name in legacy_names {
        let path = install_dir.join(name);
        if fs::try_exists(&path).await.unwrap_or(false) {
            fs::remove_file(&path).await.ok();
            println!("Removed legacy binary: {}", path.display());
        }
    }
    Ok(())
}

/// Resolve default install directory (`~/.local/bin`).
pub fn default_install_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".local").join("bin"))
}

/// Cargo `--root` value corresponding to an install dir ending in `/bin`.
pub fn cargo_install_root(install_dir: &Path) -> PathBuf {
    install_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| install_dir.to_path_buf())
}

/// Read installed version from target binary, if present.
pub async fn read_installed_version(binary: &Path) -> Option<String> {
    if !binary.exists() {
        return None;
    }
    let output = tokio::process::Command::new(binary)
        .arg("--version")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().nth(1).map(str::to_string)
}

const PUB_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA5veFxEchlM3iyFx8BQzs
f+yn6ZNJygRwfOfLS901Rxm/I3YRwn2Jksyp2bVckjgDeGJVK7IPGaHe1dL7+Ljn
5V3zvU9B7CLeeIGdZRRngV/n6r+dsGy0FWQIcN/+dfKPWvhz4m/4QMTLXL05WK8j
iI/Qatp2Fs32CUJTJ6NpIDQZi4xd1xhQbF/jk2+pwgwpup7kAVKPa49QegFQEQcS
i8duqBKX2ynTA6QhknBX1fY+6vEFLh6uMePjzGyHLax8mMg8sk2WU59bgMGgtPPy
le7gp692r3UaP9YgzuNDTyDSoU4gJmOOYYAtMWkNOyD2Bcr8JndwPXG0CD3Hj+j0
GwIDAQAB
-----END PUBLIC KEY-----";

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let mut base64_content = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with("-----") {
            base64_content.push_str(line);
        }
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(&base64_content)
        .context("Failed to decode public key base64")
}

pub fn verify_release_signature(data: &[u8], sig: &[u8]) -> Result<()> {
    let der = pem_to_der(PUB_KEY_PEM)?;
    let public_key =
        ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, &der);
    public_key
        .verify(data, sig)
        .context("Release signature verification failed: SHA256SUMS is NOT signed by the official private key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cargo_install_root() {
        assert_eq!(
            cargo_install_root(Path::new("/home/user/.local/bin")),
            PathBuf::from("/home/user/.local")
        );
    }

    #[test]
    fn test_verify_file_checksum_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.bin");
        std::fs::write(&file, b"hello").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"hello"));
        verify_file_checksum(&file, &digest).unwrap();
    }

    #[test]
    fn test_pem_to_der_success() {
        let der = pem_to_der(PUB_KEY_PEM).unwrap();
        assert!(!der.is_empty());
    }

    #[test]
    fn test_verify_release_signature_fails_on_invalid_sig() {
        let data = b"some release manifest data";
        let invalid_sig = b"invalid signature bytes here";
        let result = verify_release_signature(data, invalid_sig);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("verification failed"));
    }
}
