//! GitHub release metadata and asset resolution.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::platform::Platform;

pub const GITHUB_REPO: &str = "ahma-labs/ahma";

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
    fetch_latest_asset_impl(client, "https://api.github.com", platform).await
}

async fn fetch_latest_asset_impl(
    client: &reqwest::Client,
    api_base: &str,
    platform: &Platform,
) -> Result<ReleaseAsset> {
    let url = format!("{api_base}/repos/{GITHUB_REPO}/releases/latest");
    let release = fetch_release_json(client, &url).await?;
    pick_asset(release, platform)
}

/// Fetch a tagged release asset for this platform.
pub async fn fetch_tagged_asset(
    client: &reqwest::Client,
    tag: &str,
    platform: &Platform,
) -> Result<ReleaseAsset> {
    fetch_tagged_asset_impl(client, "https://api.github.com", tag, platform).await
}

async fn fetch_tagged_asset_impl(
    client: &reqwest::Client,
    api_base: &str,
    tag: &str,
    platform: &Platform,
) -> Result<ReleaseAsset> {
    let url = format!("{api_base}/repos/{GITHUB_REPO}/releases/tags/{tag}");
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

    // ── parse_checksum_line ──────────────────────────────────────────────────

    #[test]
    fn test_parse_checksum_line_exact_match() {
        assert_eq!(
            parse_checksum_line(
                "abc123  ahma-release-linux-x86_64.tar.gz",
                "ahma-release-linux-x86_64.tar.gz"
            ),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_parse_checksum_line_star_prefix_stripped() {
        assert_eq!(
            parse_checksum_line("def456 *ahma.exe", "ahma.exe"),
            Some("def456".to_string())
        );
    }

    #[test]
    fn test_parse_checksum_line_no_match_returns_none() {
        assert_eq!(parse_checksum_line("abc123 other.bin", "ahma"), None);
    }

    /// Early-return path: empty string → None (covers the `return None` branch).
    #[test]
    fn test_parse_checksum_line_empty_string() {
        assert_eq!(parse_checksum_line("", "ahma"), None);
    }

    /// Early-return path: whitespace-only trims to empty → None.
    #[test]
    fn test_parse_checksum_line_whitespace_only() {
        assert_eq!(parse_checksum_line("   \t  ", "ahma"), None);
    }

    /// `to_ascii_lowercase` branch: uppercase hash is normalised.
    #[test]
    fn test_parse_checksum_line_uppercase_hash_lowercased() {
        assert_eq!(
            parse_checksum_line(
                "ABCDEF0123456789  ahma-release-linux-x86_64.tar.gz",
                "ahma-release-linux-x86_64.tar.gz"
            ),
            Some("abcdef0123456789".to_string())
        );
    }

    /// `ends_with` branch: name has a path prefix before the bare filename.
    #[test]
    fn test_parse_checksum_line_name_suffix_match() {
        assert_eq!(
            parse_checksum_line("abc123  ./dist/ahma.exe", "ahma.exe"),
            Some("abc123".to_string())
        );
    }

    /// `parts.next()?` returns None when the line has only one token (no filename).
    #[test]
    fn test_parse_checksum_line_single_token_no_name() {
        assert_eq!(parse_checksum_line("abc123", "ahma"), None);
    }

    // ── pick_asset ───────────────────────────────────────────────────────────

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

    /// Error path: no matching asset → `with_context` closure is exercised.
    #[test]
    fn test_pick_asset_missing_asset_returns_error() {
        let platform = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let release = GitHubRelease {
            tag_name: "v1.2.3".to_string(),
            assets: vec![GitHubAsset {
                name: "ahma-release-darwin-arm64.tar.gz".to_string(),
                browser_download_url: "https://example.com/darwin.tar.gz".to_string(),
            }],
        };
        let err = pick_asset(release, &platform).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ahma-release-linux-x86_64.tar.gz"),
            "expected missing asset name in error: {msg}"
        );
        assert!(msg.contains("v1.2.3"), "expected tag in error: {msg}");
        assert!(
            msg.contains("github.com"),
            "expected repo URL hint in error: {msg}"
        );
    }

    /// Error path: empty asset list.
    #[test]
    fn test_pick_asset_empty_assets_returns_error() {
        let platform = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let release = GitHubRelease {
            tag_name: "v0.1.0".to_string(),
            assets: vec![],
        };
        let err = pick_asset(release, &platform).unwrap_err();
        assert!(err.to_string().contains("ahma-release-linux-x86_64.tar.gz"));
    }

    /// All `ReleaseAsset` fields are populated correctly.
    #[test]
    fn test_pick_asset_all_fields_populated() {
        let platform = Platform {
            id: "darwin-arm64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let release = GitHubRelease {
            tag_name: "v2.0.0".to_string(),
            assets: vec![GitHubAsset {
                name: "ahma-release-darwin-arm64.tar.gz".to_string(),
                browser_download_url: "https://example.com/v2.0.0/darwin.tar.gz".to_string(),
            }],
        };
        let asset = pick_asset(release, &platform).unwrap();
        assert_eq!(asset.tag, "v2.0.0");
        assert_eq!(asset.version, "2.0.0");
        assert_eq!(
            asset.download_url,
            "https://example.com/v2.0.0/darwin.tar.gz"
        );
        assert_eq!(asset.asset_name, "ahma-release-darwin-arm64.tar.gz");
    }

    // ── fetch_release_json ───────────────────────────────────────────────────

    /// Success path: 200 + valid JSON → parsed `GitHubRelease`.
    #[tokio::test]
    async fn test_fetch_release_json_success() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v1.0.0",
                "assets": [
                    {
                        "name": "ahma-release-linux-x86_64.tar.gz",
                        "browser_download_url": "https://example.com/linux.tar.gz"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/releases/latest", server.uri());
        let release = fetch_release_json(&client, &url).await.unwrap();
        assert_eq!(release.tag_name, "v1.0.0");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].name, "ahma-release-linux-x86_64.tar.gz");
        assert_eq!(
            release.assets[0].browser_download_url,
            "https://example.com/linux.tar.gz"
        );
    }

    /// HTTP error path: non-2xx status → `bail!` with status in message.
    #[tokio::test]
    async fn test_fetch_release_json_http_error() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases/tags/v9.9.9"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/releases/tags/v9.9.9", server.uri());
        let err = fetch_release_json(&client, &url).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("GitHub release lookup failed"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("404"), "expected HTTP status in error: {msg}");
    }

    /// Parse-error path: 200 response but body is not valid JSON.
    #[tokio::test]
    async fn test_fetch_release_json_invalid_json() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bad-json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/bad-json", server.uri());
        let err = fetch_release_json(&client, &url).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to parse GitHub release JSON"),
            "unexpected error: {err}"
        );
    }

    // ── fetch_latest_asset_impl / fetch_tagged_asset_impl ────────────────────

    /// `fetch_latest_asset_impl` constructs the correct URL path and returns asset.
    #[tokio::test]
    async fn test_fetch_latest_asset_impl_success() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        let expected_path = format!("/repos/{GITHUB_REPO}/releases/latest");
        Mock::given(method("GET"))
            .and(path(expected_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v1.5.0",
                "assets": [{
                    "name": "ahma-release-linux-x86_64.tar.gz",
                    "browser_download_url": "https://example.com/v1.5.0/linux.tar.gz"
                }]
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let platform = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let asset = fetch_latest_asset_impl(&client, &server.uri(), &platform)
            .await
            .unwrap();
        assert_eq!(asset.version, "1.5.0");
        assert_eq!(asset.tag, "v1.5.0");
        assert_eq!(
            asset.download_url,
            "https://example.com/v1.5.0/linux.tar.gz"
        );
    }

    /// `fetch_tagged_asset_impl` constructs the correct URL path for a tag.
    #[tokio::test]
    async fn test_fetch_tagged_asset_impl_success() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        let expected_path = format!("/repos/{GITHUB_REPO}/releases/tags/v0.9.0");
        Mock::given(method("GET"))
            .and(path(expected_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v0.9.0",
                "assets": [{
                    "name": "ahma-release-darwin-arm64.tar.gz",
                    "browser_download_url": "https://example.com/v0.9.0/darwin.tar.gz"
                }]
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let platform = Platform {
            id: "darwin-arm64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let asset = fetch_tagged_asset_impl(&client, &server.uri(), "v0.9.0", &platform)
            .await
            .unwrap();
        assert_eq!(asset.version, "0.9.0");
        assert_eq!(asset.tag, "v0.9.0");
        assert_eq!(
            asset.download_url,
            "https://example.com/v0.9.0/darwin.tar.gz"
        );
        assert_eq!(asset.asset_name, "ahma-release-darwin-arm64.tar.gz");
    }

    /// `fetch_tagged_asset_impl` propagates HTTP errors from `fetch_release_json`.
    #[tokio::test]
    async fn test_fetch_tagged_asset_impl_http_error() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        let expected_path = format!("/repos/{GITHUB_REPO}/releases/tags/v0.0.1");
        Mock::given(method("GET"))
            .and(path(expected_path))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let platform = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let err = fetch_tagged_asset_impl(&client, &server.uri(), "v0.0.1", &platform)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("GitHub release lookup failed"),
            "unexpected error: {err}"
        );
    }

    /// `fetch_latest_asset_impl` propagates asset-not-found error from `pick_asset`.
    #[tokio::test]
    async fn test_fetch_latest_asset_impl_missing_asset_error() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        let expected_path = format!("/repos/{GITHUB_REPO}/releases/latest");
        Mock::given(method("GET"))
            .and(path(expected_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v3.0.0",
                "assets": [{
                    "name": "ahma-release-darwin-arm64.tar.gz",
                    "browser_download_url": "https://example.com/darwin.tar.gz"
                }]
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        // Request the Linux build — only Darwin is in the release
        let platform = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        let err = fetch_latest_asset_impl(&client, &server.uri(), &platform)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ahma-release-linux-x86_64.tar.gz"),
            "expected missing asset name in error: {msg}"
        );
    }
}
