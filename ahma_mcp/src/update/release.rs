//! GitHub release metadata and asset resolution.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::platform::Platform;

pub const GITHUB_REPO: &str = "paulirotta/ahma";

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Resolved release download target.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub tag: String,
    pub version: String,
    pub download_url: String,
    pub asset_name: String,
}

/// Fetch the latest published release asset for this platform.
pub async fn fetch_latest_asset(
    client: &reqwest::Client,
    platform: &Platform,
) -> Result<ReleaseAsset> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let release = fetch_release_json(client, &url).await?;
    pick_asset(release, platform)
}

/// Fetch a tagged release asset for this platform.
pub async fn fetch_tagged_asset(
    client: &reqwest::Client,
    tag: &str,
    platform: &Platform,
) -> Result<ReleaseAsset> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{tag}");
    let release = fetch_release_json(client, &url).await?;
    pick_asset(release, platform)
}

async fn fetch_release_json(client: &reqwest::Client, url: &str) -> Result<GitHubRelease> {
    let response = client
        .get(url)
        .header("User-Agent", "ahma-updater")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("Failed to fetch release metadata from {url}"))?;

    if !response.status().is_success() {
        bail!(
            "GitHub release lookup failed (HTTP {}): {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    response
        .json::<GitHubRelease>()
        .await
        .context("Failed to parse GitHub release JSON")
}

fn pick_asset(release: GitHubRelease, platform: &Platform) -> Result<ReleaseAsset> {
    let asset_name = platform.asset_name();
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == asset_name)
        .with_context(|| {
            format!(
                "Release {} has no asset '{asset_name}'. \
                 See https://github.com/{GITHUB_REPO}/releases",
                release.tag_name
            )
        })?;

    let version = super::ref_mode::release_version_from_tag(&release.tag_name).to_string();

    Ok(ReleaseAsset {
        tag: release.tag_name,
        version,
        download_url: asset.browser_download_url,
        asset_name,
    })
}

/// Parse a line from SHA256SUMS (`hash  filename` or `hash *filename`).
pub fn parse_checksum_line(line: &str, expected_filename: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    let name = parts.next()?.trim_start_matches('*');
    if name == expected_filename || name.ends_with(expected_filename) {
        Some(hash.to_ascii_lowercase())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::platform::{ArchiveFormat, Platform};
    use super::*;

    #[test]
    fn test_parse_checksum_line() {
        assert_eq!(
            parse_checksum_line(
                "abc123  ahma-release-linux-x86_64.tar.gz",
                "ahma-release-linux-x86_64.tar.gz"
            ),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_checksum_line("def456 *ahma.exe", "ahma.exe"),
            Some("def456".to_string())
        );
        assert_eq!(parse_checksum_line("abc123 other.bin", "ahma"), None);
    }

    #[test]
    fn test_pick_asset_from_release() {
        let platform = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let release = GitHubRelease {
            tag_name: "v0.6.7".to_string(),
            assets: vec![GitHubAsset {
                name: "ahma-release-linux-x86_64.tar.gz".to_string(),
                browser_download_url: "https://example.com/asset.tar.gz".to_string(),
            }],
        };
        let asset = pick_asset(release, &platform).unwrap();
        assert_eq!(asset.version, "0.6.7");
        assert_eq!(asset.tag, "v0.6.7");
    }
}
