//! Download, verify, and install release archives.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::platform::{ArchiveFormat, Platform};
use super::receipt;
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
    // Attestation (Sigstore SLSA Level 3) from the official ahma-labs/ahma pipeline.
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

    // Digests for the install receipt, taken before anything can change the bytes.
    // `should_skip_verify` covers the retired env vars `verify_artifact` still honours:
    // a receipt must never claim a verification that was skipped.
    let verified = if insecure_skip_verify || verify::should_skip_verify() {
        None
    } else {
        Some(receipt::VerifiedArtifact {
            name: asset.asset_name.clone(),
            sha256: file_sha256_hex(&archive_path).await?,
        })
    };
    let released_sha256 = file_sha256_hex(&extracted).await?;

    install_binary(&extracted, &target).await?;
    // Best-effort: the binary is installed; a missing receipt only makes a later
    // `ahma verify --self` on a re-signed macOS binary fail strictly.
    if let Err(e) = receipt::record_install(
        &target,
        "ahma update",
        &asset.version,
        verified.as_ref(),
        &released_sha256,
    )
    .await
    {
        eprintln!("warning: could not record the install receipt: {e:#}");
    }
    cleanup_legacy_binaries(install_dir).await?;

    Ok(target)
}

async fn file_sha256_hex(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(super::sha256_hex(&bytes))
}

async fn download_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let response = crate::github::get(url, || client.get(url)).await?;
    if !response.status().is_success() {
        return Err(crate::github::status_error(&format!("Download of {url}"), response).await);
    }

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
                "https://github.com/ahma-labs/ahma/releases/download/{}/SHA256SUMS",
                asset.tag
            )
        });

    let response = crate::github::get(&sums_url, || client.get(&sums_url)).await?;
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

        // SPEC R-SIGN.1 on macOS: keep a Developer ID signature, re-sign anything
        // else ad hoc with the hardened runtime. Signing happens on the staged
        // copy, before it takes the path.
        #[cfg(target_os = "macos")]
        sign_for_install(&staged).await;

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
            // Windows will not replace a running executable, but it will
            // rename one: move it aside, then put the new build at the path.
            // Nothing running is stopped — the hub notices the new file and
            // hands over once its work is done (SPEC R-HUB.5), and its next
            // start removes what was moved aside.
            let staged = target.with_extension("new.exe");
            let _ = fs::remove_file(&staged).await;
            fs::copy(source, &staged)
                .await
                .with_context(|| format!("Failed to stage binary at {}", staged.display()))?;
            let aside = aside_path(target).await;
            if let Err(e) = fs::rename(target, &aside).await {
                let _ = fs::remove_file(&staged).await;
                bail!(
                    "Could not move the running {} aside to {}: {e}",
                    target.display(),
                    aside.display()
                );
            }
            if let Err(e) = fs::rename(&staged, target).await {
                // Put the old build back rather than leave no binary at all.
                let _ = fs::rename(&aside, target).await;
                bail!("Could not install {}: {e}", target.display());
            }
        } else {
            fs::copy(source, target).await?;
        }
    }

    println!("Installed {}", target.display());
    Ok(())
}

/// What `codesign -dvv` reports about a binary's code signature.
///
/// Only the two facts an installer needs (SPEC R-SIGN.1). `codesign -dvv` writes its
/// report to **stderr**.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CodeSignature {
    /// The leaf certificate — the first `Authority=` line — is a
    /// `Developer ID Application` certificate.
    pub developer_id: bool,
    /// The `CodeDirectory` flags include `runtime` (the hardened runtime).
    pub hardened_runtime: bool,
}

/// Parse `codesign -dvv` output. Pure, so it is tested on every platform.
///
/// Signatures the parser sees in practice: the linker's ad-hoc signature of a cargo
/// build (`flags=0x20002(adhoc,linker-signed)`, no `Authority=`), an installer's ad-hoc
/// re-sign (`flags=0x10002(adhoc,runtime)`, no `Authority=`), and a Developer ID
/// release (`flags=0x10000(runtime)`, `Authority=Developer ID Application: …`).
pub fn parse_codesign_display(output: &str) -> CodeSignature {
    let mut signature = CodeSignature::default();
    let mut seen_authority = false;
    for line in output.lines().map(str::trim) {
        if let Some(authority) = line.strip_prefix("Authority=") {
            // The chain is printed leaf first; only the leaf identifies the signer.
            // ("Developer ID Certification Authority" is an intermediate.)
            if !seen_authority {
                signature.developer_id = authority.starts_with("Developer ID Application:");
                seen_authority = true;
            }
        } else if line.starts_with("CodeDirectory ") {
            signature.hardened_runtime = code_directory_flags(line).any(|f| f == "runtime");
        }
    }
    signature
}

/// The symbolic flags of a `CodeDirectory … flags=0x10002(adhoc,runtime) …` line.
fn code_directory_flags(line: &str) -> impl Iterator<Item = &str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix("flags="))
        .and_then(|flags| {
            let open = flags.find('(')?;
            let close = flags.rfind(')')?;
            flags.get(open + 1..close)
        })
        .into_iter()
        .flat_map(|names| names.split(','))
        .map(str::trim)
}

/// Whether an installer must keep a binary's existing signature instead of
/// re-signing it ad hoc (SPEC R-SIGN.1).
///
/// Only a signature that passes `codesign --verify --strict` **and** is a Developer ID
/// with the hardened runtime is kept. That is the release signature: replacing it
/// would discard the notarized identity and change the binary's bytes, so it would no
/// longer match its attestation. Anything else — the linker's ad-hoc signature above
/// all — is what the local re-sign exists to replace.
///
/// This decides nothing about provenance: by the time it runs, the release has
/// already passed attestation verification, and any Developer ID is accepted.
pub fn keeps_existing_signature(strict_verify_passed: bool, codesign_display: &str) -> bool {
    let signature = parse_codesign_display(codesign_display);
    strict_verify_passed && signature.developer_id && signature.hardened_runtime
}

/// Sign a staged binary for install on macOS (SPEC R-SIGN.1): keep a valid Developer
/// ID signature with the hardened runtime; otherwise re-sign ad hoc with the hardened
/// runtime, because a linker-ad-hoc signature fails code-page re-validation under
/// memory pressure and the process is SIGKILLed. Best-effort: an install must not fail
/// because codesign is unavailable.
#[cfg(target_os = "macos")]
async fn sign_for_install(staged: &Path) {
    if has_kept_signature(staged).await {
        println!("Keeping the release's Developer ID signature (hardened runtime).");
        return;
    }
    match run_codesign(&["--force", "--sign", "-", "--options", "runtime"], staged).await {
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

/// `codesign --verify --strict` passes and `codesign -dvv` shows a Developer ID with
/// the hardened runtime — see [`keeps_existing_signature`].
#[cfg(target_os = "macos")]
async fn has_kept_signature(path: &Path) -> bool {
    let strict_verify_passed = matches!(
        run_codesign(&["--verify", "--strict"], path).await,
        Ok(out) if out.status.success()
    );
    if !strict_verify_passed {
        return false;
    }
    match run_codesign(&["-dvv"], path).await {
        Ok(out) if out.status.success() => {
            // The report is on stderr; read both streams in case that ever changes.
            let mut report = String::from_utf8_lossy(&out.stderr).into_owned();
            report.push_str(&String::from_utf8_lossy(&out.stdout));
            keeps_existing_signature(true, &report)
        }
        _ => false,
    }
}

#[cfg(target_os = "macos")]
async fn run_codesign(args: &[&str], path: &Path) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new("codesign")
        .args(args)
        .arg(path)
        .kill_on_drop(true) // owned child (SPEC R-PROC.1)
        .output()
        .await
}

/// Where a running Windows binary is moved aside to: `<name>.old`, or — when
/// an earlier `.old` is itself still running and cannot be removed — a
/// timestamped `<name>.<secs>.old` beside it.
#[cfg(windows)]
async fn aside_path(target: &Path) -> PathBuf {
    let aside = target.with_extension("old");
    let _ = fs::remove_file(&aside).await;
    if !fs::try_exists(&aside).await.unwrap_or(true) {
        return aside;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    target.with_extension(format!("{secs}.old"))
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
        .kill_on_drop(true) // owned child (SPEC R-PROC.1)
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
        let digest = crate::sha256_hex(b"hello");
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

    /// Existing target: rename to .old, then put the new binary at the path.
    /// On every OS — on Windows this is how a running binary is replaced.
    #[tokio::test]
    async fn test_install_binary_backs_up_existing_target() {
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
        assert_eq!(std::fs::read(&backup).unwrap(), b"old binary");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&target).unwrap().permissions();
            assert!(
                perms.mode() & 0o111 != 0,
                "installed binary should be executable"
            );
        }
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

    // ── codesign -dvv parsing (SPEC R-SIGN.1) ─────────────────────────────────
    //
    // Fixtures are verbatim `codesign -dvv` output (stderr) captured on macOS 27,
    // with only the user's home directory rewritten in the `Executable=` lines.
    // `developer_id_no_runtime.txt` is `developer_id_runtime.txt` with the flags
    // rewritten to the `flags=0x0(none)` form codesign prints for a signature without
    // the hardened runtime: no such Developer ID binary was at hand to capture.

    /// `cargo build` output: the linker's ad-hoc signature.
    const LINKER_ADHOC: &str = include_str!("../tests/fixtures/codesign/linker_adhoc.txt");
    /// What the installer's own `codesign --force --sign - --options runtime` produces.
    const ADHOC_RUNTIME: &str = include_str!("../tests/fixtures/codesign/adhoc_runtime.txt");
    /// A notarized Developer ID command-line binary with the hardened runtime.
    const DEVELOPER_ID_RUNTIME: &str =
        include_str!("../tests/fixtures/codesign/developer_id_runtime.txt");
    /// A Developer ID signature without the hardened runtime.
    const DEVELOPER_ID_NO_RUNTIME: &str =
        include_str!("../tests/fixtures/codesign/developer_id_no_runtime.txt");

    #[test]
    fn codesign_parse_linker_adhoc() {
        assert_eq!(
            parse_codesign_display(LINKER_ADHOC),
            CodeSignature {
                developer_id: false,
                hardened_runtime: false
            }
        );
    }

    #[test]
    fn codesign_parse_adhoc_with_runtime() {
        assert_eq!(
            parse_codesign_display(ADHOC_RUNTIME),
            CodeSignature {
                developer_id: false,
                hardened_runtime: true
            }
        );
    }

    #[test]
    fn codesign_parse_developer_id_with_runtime() {
        assert_eq!(
            parse_codesign_display(DEVELOPER_ID_RUNTIME),
            CodeSignature {
                developer_id: true,
                hardened_runtime: true
            }
        );
    }

    #[test]
    fn codesign_parse_developer_id_without_runtime() {
        assert_eq!(
            parse_codesign_display(DEVELOPER_ID_NO_RUNTIME),
            CodeSignature {
                developer_id: true,
                hardened_runtime: false
            }
        );
    }

    /// Only the leaf authority identifies the signer: an Apple platform binary's chain
    /// (and the "Developer ID Certification Authority" intermediate under a real
    /// Developer ID leaf) must not read as a Developer ID Application signature.
    #[test]
    fn codesign_parse_uses_only_the_leaf_authority() {
        let apple_platform = "CodeDirectory v=20400 size=325 flags=0x0(none) hashes=5+2 location=embedded\n\
                              Authority=macOS Software Signing\n\
                              Authority=Apple Code Signing Certification Authority\n\
                              Authority=Apple Root CA\n";
        assert!(!parse_codesign_display(apple_platform).developer_id);

        let intermediate_first = "CodeDirectory v=20500 flags=0x10000(runtime)\n\
                                  Authority=Developer ID Certification Authority\n\
                                  Authority=Developer ID Application: Example (ABCDE12345)\n";
        assert!(!parse_codesign_display(intermediate_first).developer_id);
    }

    /// `runtime` is matched as a whole flag name, never as a substring of another
    /// line (`Runtime Version=`) or of an unrelated flag.
    #[test]
    fn codesign_parse_runtime_is_a_whole_code_directory_flag() {
        let no_flag = "CodeDirectory v=20500 size=10 flags=0x2(adhoc) hashes=1+0 location=embedded\n\
                       Runtime Version=27.0.0\n";
        assert!(!parse_codesign_display(no_flag).hardened_runtime);
        assert_eq!(parse_codesign_display(""), CodeSignature::default());
        assert_eq!(
            parse_codesign_display("code object is not signed at all"),
            CodeSignature::default()
        );
    }

    /// The installer keeps a signature only when strict verification passed and it is
    /// a Developer ID with the hardened runtime; every other case is re-signed ad hoc.
    #[test]
    fn keeps_existing_signature_only_for_valid_developer_id_with_runtime() {
        assert!(keeps_existing_signature(true, DEVELOPER_ID_RUNTIME));

        assert!(
            !keeps_existing_signature(false, DEVELOPER_ID_RUNTIME),
            "a Developer ID that fails `codesign --verify --strict` is not kept"
        );
        assert!(!keeps_existing_signature(true, DEVELOPER_ID_NO_RUNTIME));
        assert!(!keeps_existing_signature(true, LINKER_ADHOC));
        assert!(!keeps_existing_signature(true, ADHOC_RUNTIME));
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
        let correct_hash = crate::sha256_hex(&archive_bytes);
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
