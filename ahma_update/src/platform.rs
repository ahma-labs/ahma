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

/// Name of the ahma executable on the current platform.
///
/// The single source of truth for the platform binary name — do not re-derive
/// `if cfg!(windows) { "ahma.exe" } else { "ahma" }` elsewhere.
pub const AHMA_BINARY_NAME: &str = if cfg!(target_os = "windows") {
    "ahma.exe"
} else {
    "ahma"
};

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
        AHMA_BINARY_NAME
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
            other => bail!("Unsupported Linux architecture: {other}"),
        };

        // R-CFG1.2 / R-CFG1.2.1: `AHMA_PREFER_MUSL` is RETIRED — warn and ignore.
        // It was documented as retired while this line still *honored* it, which is
        // the exact drift R-CFG1.2.1 exists to close: the docs said one thing and
        // one surface did another. Which libc variant of a binary gets downloaded
        // and installed is not a decision ambient process state should make.
        // `--prefer-musl` (via `PREFER_MUSL_OVERRIDE`) is the replacement, and
        // `detect_linux_musl()` still handles the case automatically.
        ahma_common::config::warn_retired_env("AHMA_PREFER_MUSL");
        let prefer_musl = PREFER_MUSL_OVERRIDE.get().is_some() || detect_linux_musl();

        let id = if prefer_musl {
            format!("linux-{arch}-musl")
        } else {
            format!("linux-{arch}")
        };

        Ok(Platform {
            id,
            archive_ext: ArchiveFormat::TarGz,
        })
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
