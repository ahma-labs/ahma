//! Platform detection for release asset selection.

use anyhow::{Result, bail};

/// Process-wide musl preference set from the `--prefer-musl` CLI flag.
/// Cross-platform: defined everywhere, consumed by the Linux detection branch.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
static PREFER_MUSL_OVERRIDE: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Prefer musl builds on Linux (set from the `--prefer-musl` CLI flag).
pub fn set_prefer_musl_override() {
    let _ = PREFER_MUSL_OVERRIDE.set(());
}

/// GitHub release asset platform identifier (matches CI packaging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub id: String,
    pub archive_ext: ArchiveFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    #[cfg_attr(target_os = "windows", allow(dead_code))] // Zip on Windows.
    TarGz,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))] // TarGz on Unix-like platforms.
    Zip,
}

impl Platform {
    pub fn asset_name(&self) -> String {
        match self.archive_ext {
            ArchiveFormat::TarGz => format!("ahma-release-{}.tar.gz", self.id),
            ArchiveFormat::Zip => format!("ahma-release-{}.zip", self.id),
        }
    }

    pub fn binary_name(&self) -> &'static str {
        if cfg!(target_os = "windows") {
            "ahma.exe"
        } else {
            "ahma"
        }
    }
}

/// Detect the current platform for release downloads.
pub fn detect_platform() -> Result<Platform> {
    #[cfg(target_os = "windows")]
    {
        // Only x86_64 Windows builds are published today.
        if std::env::consts::ARCH != "x86_64" {
            bail!(
                "Unsupported Windows architecture: {}. Only x86_64 builds are available.",
                std::env::consts::ARCH
            );
        }
        return Ok(Platform {
            id: "windows-x86_64".to_string(),
            archive_ext: ArchiveFormat::Zip,
        });
    }

    #[cfg(target_os = "macos")]
    {
        if std::env::consts::ARCH == "x86_64" {
            bail!(
                "macOS Intel (x86_64) is no longer supported. \
                 Prebuilt binaries are only available for Apple Silicon (arm64). \
                 Build from source: ahma update main"
            );
        }
        if std::env::consts::ARCH != "aarch64" {
            bail!("Unsupported macOS architecture: {}", std::env::consts::ARCH);
        }
        Ok(Platform {
            id: "darwin-arm64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        })
    }

    #[cfg(target_os = "linux")]
    {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "arm64",
            "arm" | "armv7" => "armv7",
            other => bail!("Unsupported Linux architecture: {other}"),
        };

        let env_prefer_musl = std::env::var("AHMA_PREFER_MUSL")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        if env_prefer_musl {
            tracing::warn!(
                "Deprecated: AHMA_PREFER_MUSL environment variable is set. Use the --prefer-musl flag instead."
            );
        }
        let prefer_musl =
            PREFER_MUSL_OVERRIDE.get().is_some() || env_prefer_musl || detect_linux_musl();

        let id = if arch == "armv7" {
            "linux-armv7".to_string()
        } else if prefer_musl {
            format!("linux-{arch}-musl")
        } else {
            format!("linux-{arch}")
        };

        return Ok(Platform {
            id,
            archive_ext: ArchiveFormat::TarGz,
        });
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        bail!("Unsupported operating system for prebuilt releases");
    }
}

#[cfg(target_os = "linux")]
fn detect_linux_musl() -> bool {
    if std::path::Path::new("/etc/alpine-release").exists() {
        return true;
    }
    std::process::Command::new("ldd")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let bytes = if !o.stderr.is_empty() {
                o.stderr
            } else {
                o.stdout
            };
            String::from_utf8(bytes).ok()
        })
        .map(|s| s.to_ascii_lowercase().contains("musl"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_asset_name_tar_gz() {
        let p = Platform {
            id: "linux-x86_64".to_string(),
            archive_ext: ArchiveFormat::TarGz,
        };
        assert_eq!(p.asset_name(), "ahma-release-linux-x86_64.tar.gz");
    }

    #[test]
    fn test_platform_asset_name_zip() {
        let p = Platform {
            id: "windows-x86_64".to_string(),
            archive_ext: ArchiveFormat::Zip,
        };
        assert_eq!(p.asset_name(), "ahma-release-windows-x86_64.zip");
    }

    #[test]
    fn test_detect_platform_does_not_panic() {
        // Smoke test on the current CI/dev platform.
        let _ = detect_platform();
    }
}
