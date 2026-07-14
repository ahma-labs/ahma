//! Download, verify, and install release archives.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::platform::{ArchiveFormat, Platform};
use super::release::{ReleaseAsset, parse_checksum_line};
use super::verify;

/// Install a release archive into `install_dir`.
pub async fn install_release_asset(
    client: &reqwest::Client,
    asset: &ReleaseAsset,
    platform: &Platform,
    install_dir: &Path,
    dry_run: bool,
    insecure_skip_verify: bool,
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

    // Sanity-check the archive hash against the SHA256SUMS manifest (defense in depth).
    if let Some(expected) = fetch_archive_checksum(client, asset, platform).await? {
        verify_file_checksum(&archive_path, &expected)?;
        println!("Checksum verified.");
    } else {
        eprintln!("Warning: no SHA256 checksum found in release manifest; skipping hash check.");
    }

    // Cryptographic verification: confirm the archive has a valid GitHub Build Provenance
    // Attestation (Sigstore SLSA Level 3) from the official paulirotta/ahma pipeline.
    if !insecure_skip_verify {
        verify::verify_artifact(&archive_path)
            .await
            .context("Release attestation verification failed; aborting install")?;
        println!("Attestation verified: archive was built by the official CI pipeline.");
    } else {
        eprintln!("WARNING: Sigstore attestation verification skipped (--insecure-skip-verify).");
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

/// Fetch the expected SHA256 hash for the release archive from the combined SHA256SUMS manifest.
///
/// Returns `None` if the manifest is unavailable (older release or network issue).
/// Cryptographic verification is handled separately by [`verify::verify_artifact`].
async fn fetch_archive_checksum(
    client: &reqwest::Client,
    asset: &ReleaseAsset,
    platform: &Platform,
) -> Result<Option<String>> {
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
    let actual = super::sha256_hex(&bytes);
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
        // Atomic out-of-place install (SPEC R-SIGN.2): stage the new binary
        // next to the target, then rename over it. The path never holds a
        // partially-written file, and the running binary's inode is never
        // written through — overwriting it in place invalidates the mapped
        // code signature on macOS and the kernel SIGKILLs the live server.
        let staged = target.with_extension("new");
        let _ = fs::remove_file(&staged).await;
        fs::copy(source, &staged)
            .await
            .with_context(|| format!("Failed to stage binary at {}", staged.display()))?;
        let mut perms = fs::metadata(&staged).await?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&staged, perms).await?;

        // Re-sign the staged binary with the hardened runtime (SPEC R-SIGN.1,
        // local part): a linker-ad-hoc signature fails code-page re-validation
        // under memory pressure and gets the process SIGKILLed. Best-effort —
        // an install must not fail because codesign is unavailable.
        #[cfg(target_os = "macos")]
        {
            let result = tokio::process::Command::new("codesign")
                .args(["--force", "--sign", "-", "--options", "runtime"])
                .arg(&staged)
                .output()
                .await;
            match result {
                Ok(out) if out.status.success() => {}
                Ok(out) => eprintln!(
                    "warning: codesign of {} failed ({}); the binary may be killed \
                     under memory pressure (SPEC R-SIGN.1): {}",
                    staged.display(),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => eprintln!(
                    "warning: could not run codesign for {}: {e} (SPEC R-SIGN.1)",
                    staged.display()
                ),
            }
        }

        // Keep a rollback copy. Best-effort: even if this rename fails, the
        // atomic rename below still replaces the path without touching the
        // old inode's pages.
        if target.exists() {
            let backup = target.with_extension("old");
            let _ = fs::remove_file(&backup).await;
            let _ = fs::rename(target, &backup).await;
        }

        fs::rename(&staged, target).await.with_context(|| {
            format!(
                "Failed to atomically install {} over {}",
                staged.display(),
                target.display()
            )
        })?;
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
    // Bring in sibling-module types used by helper functions below.
    use super::super::platform::{ArchiveFormat, Platform};
    use super::super::release::ReleaseAsset;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn tar_gz_platform() -> Platform {
        Platform {
            id: "test-platform".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        }
    }

    fn zip_platform() -> Platform {
        Platform {
            id: "test-platform".to_string(),
            archive_ext: ArchiveFormat::Zip,
        }
    }

    fn make_asset(download_url: &str, platform: &Platform) -> ReleaseAsset {
        ReleaseAsset {
            tag: "v0.7.0".to_string(),
            version: "0.7.0".to_string(),
            download_url: download_url.to_string(),
            asset_name: platform.asset_name(),
        }
    }

    /// Build a tar.gz archive in memory containing one entry named `binary_name`.
    fn make_tar_gz_bytes(binary_name: &str, content: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tar::Builder;

        let buf = Vec::new();
        let gz = GzEncoder::new(buf, Compression::default());
        let mut builder = Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_path(binary_name).unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, content).unwrap();
        let gz = builder.into_inner().unwrap();
        gz.finish().unwrap()
    }

    /// Write a tar.gz to `dir/filename` containing one file `binary_name`.
    fn write_tar_gz(dir: &Path, filename: &str, binary_name: &str, content: &[u8]) -> PathBuf {
        let archive_path = dir.join(filename);
        std::fs::write(&archive_path, make_tar_gz_bytes(binary_name, content)).unwrap();
        archive_path
    }

    /// Build a zip archive in memory containing one entry named `binary_name`.
    fn make_zip_bytes(binary_name: &str, content: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        use zip::ZipWriter;
        use zip::write::SimpleFileOptions;

        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();
        writer.start_file(binary_name, options).unwrap();
        writer.write_all(content).unwrap();
        writer.finish().unwrap().into_inner()
    }

    /// Write a zip to `dir/filename` containing one file `binary_name`.
    fn write_zip(dir: &Path, filename: &str, binary_name: &str, content: &[u8]) -> PathBuf {
        let archive_path = dir.join(filename);
        std::fs::write(&archive_path, make_zip_bytes(binary_name, content)).unwrap();
        archive_path
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent("ahma-test")
            .build()
            .unwrap()
    }

    // ── Original tests (preserved) ────────────────────────────────────────────

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
        let digest = crate::update::sha256_hex(b"hello");
        verify_file_checksum(&file, &digest).unwrap();
    }

    #[test]
    fn test_verify_file_checksum_fails_on_wrong_hash() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.bin");
        std::fs::write(&file, b"hello").unwrap();
        let result = verify_file_checksum(&file, "deadbeefdeadbeefdeadbeefdeadbeef");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Checksum mismatch")
        );
    }

    // ── default_install_dir ───────────────────────────────────────────────────

    #[test]
    fn test_default_install_dir_ends_with_local_bin() {
        let dir = default_install_dir().expect("home dir should be resolvable");
        let s = dir.to_string_lossy();
        assert!(s.contains(".local"), "expected .local in path, got: {s}");
        assert!(
            s.ends_with("bin"),
            "expected path to end with 'bin', got: {s}"
        );
    }

    // ── cargo_install_root: no-parent edge case ───────────────────────────────

    #[test]
    fn test_cargo_install_root_single_component_falls_back_to_self() {
        // Path::new("bin").parent() returns Some("") — the empty-string current-dir,
        // not None — so the function returns "". The fallback to self only fires for
        // paths that have no parent at all (e.g. the empty path).
        let result = cargo_install_root(Path::new("bin"));
        assert_eq!(result, PathBuf::from(""));
    }

    #[test]
    fn test_cargo_install_root_empty_path_falls_back_to_self() {
        // An empty path has no parent (parent() returns None), so the fallback fires.
        let result = cargo_install_root(Path::new(""));
        assert_eq!(result, PathBuf::from(""));
    }

    // ── verify_file_checksum: missing file ────────────────────────────────────

    #[test]
    fn test_verify_file_checksum_missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent.bin");
        let result = verify_file_checksum(&missing, "deadbeef");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Failed to read"), "unexpected error: {msg}");
    }

    // ── extract_tar_gz ────────────────────────────────────────────────────────

    #[test]
    fn test_extract_tar_gz_success() {
        let dir = tempfile::tempdir().unwrap();
        let platform = tar_gz_platform();
        let binary_name = platform.binary_name();
        let archive = write_tar_gz(dir.path(), "archive.tar.gz", binary_name, b"#!/bin/sh\n");
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_tar_gz(&archive, &dest, &platform);
        assert!(result.is_ok(), "expected success: {:?}", result);
        assert!(result.unwrap().exists(), "extracted binary should exist");
    }

    #[test]
    fn test_extract_tar_gz_binary_not_in_archive() {
        let dir = tempfile::tempdir().unwrap();
        let platform = tar_gz_platform();
        // Archive contains "other_file", not the expected binary
        let archive = write_tar_gz(dir.path(), "archive.tar.gz", "other_file", b"content");
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_tar_gz(&archive, &dest, &platform);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found in archive"), "unexpected: {msg}");
    }

    #[test]
    fn test_extract_tar_gz_corrupt_archive_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.tar.gz");
        std::fs::write(&archive, b"not a tar.gz file").unwrap();
        let platform = tar_gz_platform();
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_tar_gz(&archive, &dest, &platform);
        assert!(result.is_err(), "corrupt archive should return error");
    }

    // ── extract_zip ───────────────────────────────────────────────────────────

    #[test]
    fn test_extract_zip_success() {
        let dir = tempfile::tempdir().unwrap();
        let platform = zip_platform();
        let binary_name = platform.binary_name();
        let archive = write_zip(dir.path(), "archive.zip", binary_name, b"binary content");
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_zip(&archive, &dest, &platform);
        assert!(result.is_ok(), "expected success: {:?}", result);
        assert!(result.unwrap().exists());
    }

    #[test]
    fn test_extract_zip_binary_not_in_archive() {
        let dir = tempfile::tempdir().unwrap();
        let platform = zip_platform();
        let archive = write_zip(dir.path(), "archive.zip", "other_file", b"content");
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_zip(&archive, &dest, &platform);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found in archive"), "unexpected: {msg}");
    }

    #[test]
    fn test_extract_zip_corrupt_archive_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.zip");
        std::fs::write(&archive, b"not a zip").unwrap();
        let platform = zip_platform();
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_zip(&archive, &dest, &platform);
        assert!(result.is_err());
    }

    // ── extract_archive (dispatch) ────────────────────────────────────────────

    #[tokio::test]
    async fn test_extract_archive_dispatches_tar_gz() {
        let dir = tempfile::tempdir().unwrap();
        let platform = tar_gz_platform();
        let binary_name = platform.binary_name();
        let archive = write_tar_gz(dir.path(), "archive.tar.gz", binary_name, b"content");
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_archive(&archive, &platform, &dest).await;
        assert!(result.is_ok(), "TarGz dispatch: {:?}", result);
    }

    #[tokio::test]
    async fn test_extract_archive_dispatches_zip() {
        let dir = tempfile::tempdir().unwrap();
        let platform = zip_platform();
        let binary_name = platform.binary_name();
        let archive = write_zip(dir.path(), "archive.zip", binary_name, b"content");
        let dest = dir.path().join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_archive(&archive, &platform, &dest).await;
        assert!(result.is_ok(), "Zip dispatch: {:?}", result);
    }

    // ── install_binary (unix) ─────────────────────────────────────────────────

    /// New target (no backup needed): copy + chmod.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_install_binary_creates_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source_bin");
        let target = dir.path().join("installed_bin");
        std::fs::write(&source, b"#!/bin/sh\necho ok\n").unwrap();

        install_binary(&source, &target).await.unwrap();

        assert!(target.exists(), "installed binary should exist");
        let perms = std::fs::metadata(&target).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "installed binary should be executable"
        );
    }

    /// Existing target: rename to .old, then copy new binary.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_install_binary_backs_up_existing_target() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source_bin");
        let target = dir.path().join("installed_bin");

        // Pre-create target so the backup branch is exercised
        std::fs::write(&target, b"old binary").unwrap();
        std::fs::write(&source, b"new binary").unwrap();

        install_binary(&source, &target).await.unwrap();

        let installed = std::fs::read(&target).unwrap();
        assert_eq!(installed, b"new binary", "target should hold new binary");

        let backup = target.with_extension("old");
        assert!(backup.exists(), "backup (.old) should exist");

        let perms = std::fs::metadata(&target).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "installed binary should be executable"
        );
    }

    /// R-SIGN.2: installing over an existing binary must never write through
    /// the old inode (that is what invalidates a running process's mapped
    /// code signature on macOS). The path must get a fresh inode, the old
    /// inode's bytes must survive untouched, and no staged temp may remain.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_install_binary_never_writes_through_old_inode() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source_bin");
        let target = dir.path().join("installed_bin");

        std::fs::write(&target, b"old binary").unwrap();
        std::fs::write(&source, b"new binary").unwrap();
        let old_inode = std::fs::metadata(&target).unwrap().ino();

        install_binary(&source, &target).await.unwrap();

        let new_inode = std::fs::metadata(&target).unwrap().ino();
        assert_ne!(
            old_inode, new_inode,
            "target must be a fresh inode, never the old one written in place"
        );
        let backup = target.with_extension("old");
        assert_eq!(
            std::fs::metadata(&backup).unwrap().ino(),
            old_inode,
            "the old inode must survive untouched as the backup"
        );
        assert_eq!(std::fs::read(&backup).unwrap(), b"old binary");
        assert!(
            !target.with_extension("new").exists(),
            "no staged temp file may remain after install"
        );
    }

    // ── cleanup_legacy_binaries ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_cleanup_legacy_binaries_removes_legacy_binary() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_name = if cfg!(target_os = "windows") {
            "ahma-simplify.exe"
        } else {
            "ahma-simplify"
        };
        let legacy_path = dir.path().join(legacy_name);
        std::fs::write(&legacy_path, b"old simplify binary").unwrap();
        assert!(
            legacy_path.exists(),
            "legacy binary should exist before cleanup"
        );

        cleanup_legacy_binaries(dir.path()).await.unwrap();

        assert!(
            !legacy_path.exists(),
            "legacy binary should be removed by cleanup"
        );
    }

    #[tokio::test]
    async fn test_cleanup_legacy_binaries_noop_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        // No legacy binary present — should succeed without error
        let result = cleanup_legacy_binaries(dir.path()).await;
        assert!(result.is_ok());
    }

    // ── read_installed_version ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_read_installed_version_nonexistent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("ahma");
        let version = read_installed_version(&missing).await;
        assert!(version.is_none(), "should return None for missing binary");
    }

    /// Create a tiny shell script and verify version parsing.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_installed_version_parses_second_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ahma");
        // Output "ahma 0.7.0" — the function grabs split_whitespace().nth(1) = "0.7.0"
        std::fs::write(&script, b"#!/bin/sh\necho 'ahma 0.7.0'\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let version = read_installed_version(&script).await;
        assert_eq!(version, Some("0.7.0".to_string()));
    }

    /// Binary that exits non-zero → None.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_installed_version_nonzero_exit_returns_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ahma");
        std::fs::write(&script, b"#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let version = read_installed_version(&script).await;
        assert!(version.is_none(), "non-zero exit should return None");
    }

    // ── install_release_asset: dry_run ────────────────────────────────────────

    #[tokio::test]
    async fn test_install_release_asset_dry_run_returns_target_path() {
        let dir = tempfile::tempdir().unwrap();
        let platform = tar_gz_platform();
        let asset = make_asset("https://example.com/v0.7.0/archive.tar.gz", &platform);
        let client = test_client();

        let result = install_release_asset(
            &client,
            &asset,
            &platform,
            dir.path(),
            true,  // dry_run — exits early, no network call
            false, // insecure_skip_verify
        )
        .await;

        assert!(
            result.is_ok(),
            "dry_run should succeed without network: {:?}",
            result
        );
        assert_eq!(result.unwrap(), dir.path().join(platform.binary_name()));
    }

    // ── fetch_archive_checksum (wiremock) ─────────────────────────────────────

    #[tokio::test]
    async fn test_fetch_archive_checksum_404_returns_none() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let platform = tar_gz_platform();
        let download_url = format!("{}/v0.7.0/{}", server.uri(), platform.asset_name());
        let asset = make_asset(&download_url, &platform);
        let client = test_client();

        let result = fetch_archive_checksum(&client, &asset, &platform).await;
        assert!(result.is_ok(), "404 should give Ok(None): {:?}", result);
        assert!(result.unwrap().is_none(), "should be None on 404");
    }

    #[tokio::test]
    async fn test_fetch_archive_checksum_found_by_asset_name() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let platform = tar_gz_platform();
        let asset_name = platform.asset_name();
        let body = format!("abc123  {asset_name}\n");

        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let download_url = format!("{}/v0.7.0/{}", server.uri(), asset_name);
        let asset = make_asset(&download_url, &platform);
        let client = test_client();

        let result = fetch_archive_checksum(&client, &asset, &platform).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some("abc123".to_string()));
    }

    #[tokio::test]
    async fn test_fetch_archive_checksum_found_by_binary_name() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let platform = tar_gz_platform();
        // Body uses the binary name ("ahma" / "ahma.exe"), not the archive name
        let binary_name = platform.binary_name();
        let body = format!("def456  {binary_name}\n");

        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let download_url = format!("{}/v0.7.0/{}", server.uri(), platform.asset_name());
        let asset = make_asset(&download_url, &platform);
        let client = test_client();

        let result = fetch_archive_checksum(&client, &asset, &platform).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some("def456".to_string()));
    }

    #[tokio::test]
    async fn test_fetch_archive_checksum_no_matching_line_returns_none() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("abc123  unrelated_archive.tar.gz\n"),
            )
            .mount(&server)
            .await;

        let platform = tar_gz_platform();
        let download_url = format!("{}/v0.7.0/{}", server.uri(), platform.asset_name());
        let asset = make_asset(&download_url, &platform);
        let client = test_client();

        let result = fetch_archive_checksum(&client, &asset, &platform).await;
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "should be None when no line matches"
        );
    }

    // ── download_file (wiremock) ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_download_file_success() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let content = b"fake binary bytes";
        Mock::given(method("GET"))
            .and(wm_path("/file.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/file.bin", server.uri());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("downloaded.bin");
        let client = test_client();

        download_file(&client, &url, &dest).await.unwrap();

        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn test_download_file_http_error_returns_err() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/file.bin"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let url = format!("{}/file.bin", server.uri());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("downloaded.bin");
        let client = test_client();

        let result = download_file(&client, &url, &dest).await;
        assert!(result.is_err(), "HTTP 403 should be an error");
    }

    #[tokio::test]
    async fn test_download_file_invalid_url_returns_err() {
        let client = test_client();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("downloaded.bin");
        // Malformed URL → reqwest errors immediately without a network call
        let result = download_file(&client, "http://[invalid-host", &dest).await;
        assert!(result.is_err(), "invalid URL should return error");
    }

    // ── install_release_asset: full flow via wiremock + insecure_skip_verify ──

    /// SHA256SUMS returns 404 → checksum skipped (covers the `else` warning branch).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_install_release_asset_full_no_checksum() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let platform = tar_gz_platform();
        let binary_name = platform.binary_name();
        let asset_name = platform.asset_name();
        let archive_bytes = make_tar_gz_bytes(binary_name, b"#!/bin/sh\necho ahma 0.7.0\n");

        // Serve the archive
        Mock::given(method("GET"))
            .and(wm_path(format!("/v0.7.0/{asset_name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive_bytes))
            .mount(&server)
            .await;

        // 404 → fetch_archive_checksum returns Ok(None) → else-branch warning printed
        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let download_url = format!("{}/v0.7.0/{asset_name}", server.uri());
        let asset = make_asset(&download_url, &platform);
        let install_dir = tempfile::tempdir().unwrap();
        let client = test_client();

        let result = install_release_asset(
            &client,
            &asset,
            &platform,
            install_dir.path(),
            false, // not dry_run
            true,  // insecure_skip_verify → skip Sigstore check
        )
        .await;

        assert!(
            result.is_ok(),
            "full install (no checksum) should succeed: {:?}",
            result
        );
        let installed = result.unwrap();
        assert!(
            installed.exists(),
            "installed binary should exist at {}",
            installed.display()
        );
    }

    /// Correct SHA256 in manifest → checksum verification passes (covers the `if let Some` branch).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_install_release_asset_full_with_correct_checksum() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let platform = tar_gz_platform();
        let binary_name = platform.binary_name();
        let asset_name = platform.asset_name();
        let archive_bytes = make_tar_gz_bytes(binary_name, b"#!/bin/sh\necho ahma 0.7.0\n");

        // Pre-compute the hash of the bytes we're about to serve
        let correct_hash = crate::update::sha256_hex(&archive_bytes);
        let sums_body = format!("{correct_hash}  {asset_name}\n");

        Mock::given(method("GET"))
            .and(wm_path(format!("/v0.7.0/{asset_name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive_bytes))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums_body))
            .mount(&server)
            .await;

        let download_url = format!("{}/v0.7.0/{asset_name}", server.uri());
        let asset = make_asset(&download_url, &platform);
        let install_dir = tempfile::tempdir().unwrap();
        let client = test_client();

        let result = install_release_asset(
            &client,
            &asset,
            &platform,
            install_dir.path(),
            false,
            true, // insecure_skip_verify
        )
        .await;

        assert!(
            result.is_ok(),
            "full install with correct checksum should succeed: {:?}",
            result
        );
        assert!(result.unwrap().exists());
    }

    /// Wrong checksum in manifest → install errors on checksum mismatch.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_install_release_asset_checksum_mismatch_returns_error() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let platform = tar_gz_platform();
        let binary_name = platform.binary_name();
        let asset_name = platform.asset_name();
        let archive_bytes = make_tar_gz_bytes(binary_name, b"#!/bin/sh\necho ahma 0.7.0\n");

        Mock::given(method("GET"))
            .and(wm_path(format!("/v0.7.0/{asset_name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive_bytes))
            .mount(&server)
            .await;

        // Deliberately wrong hash (64 hex zeros)
        let sums_body = format!("{:064x}  {asset_name}\n", 0u8);
        Mock::given(method("GET"))
            .and(wm_path("/v0.7.0/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums_body))
            .mount(&server)
            .await;

        let download_url = format!("{}/v0.7.0/{asset_name}", server.uri());
        let asset = make_asset(&download_url, &platform);
        let install_dir = tempfile::tempdir().unwrap();
        let client = test_client();

        let result = install_release_asset(
            &client,
            &asset,
            &platform,
            install_dir.path(),
            false,
            true, // insecure_skip_verify
        )
        .await;

        assert!(result.is_err(), "checksum mismatch should return error");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Checksum mismatch"), "unexpected error: {msg}");
    }
}
