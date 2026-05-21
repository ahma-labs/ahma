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

    if let Some(expected) = fetch_archive_checksum(client, asset, platform).await? {
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
}
