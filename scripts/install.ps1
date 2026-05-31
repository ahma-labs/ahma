# One-liner installer for ahma on Windows
# Usage: irm https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.ps1 | iex
#
# Supported platforms:
#   - Windows x86_64 (x64)
#
# Requirements:
#   - PowerShell 5.1+ (built in to Windows 10/11)
#   - Internet access to GitHub releases
#
# Version indicator for build script check:
# -Version '0.8.0'
# Install-OneSkill -Version '0.7.6'
#
# Environment variables:
#   AHMA_INSTALL_DIR     - Override install directory (default: $HOME\.local\bin)

#Requires -Version 5

$ErrorActionPreference = 'Stop'

# ── Detect architecture ────────────────────────────────────────────────────────
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne "AMD64") {
    Write-Error "Unsupported architecture: $arch. Only x86_64 (AMD64) Windows builds are available."
    exit 1
}

$platform = "windows-x86_64"

# ── Install directory ──────────────────────────────────────────────────────────
$installDir = if ($env:AHMA_INSTALL_DIR) {
    $env:AHMA_INSTALL_DIR
} else {
    Join-Path $HOME ".local\bin"
}

# ── Fetch latest release metadata ─────────────────────────────────────────────
$releasesUrl = "https://api.github.com/repos/paulirotta/ahma/releases/latest"
Write-Host "Fetching latest release info..."

try {
    $releaseJson = Invoke-RestMethod -Uri $releasesUrl -UseBasicParsing
} catch {
    Write-Error "Failed to fetch release info from $releasesUrl : $_"
    exit 1
}

$latestVer = ($releaseJson.tag_name -replace '^v', '')

# ── Check for existing installation and compare versions ──────────────────────
$existingCmd   = Get-Command ahma -ErrorAction SilentlyContinue
$existingInDir = Join-Path $installDir 'ahma.exe'
$existingBin   = if ($existingCmd) { $existingCmd.Source } elseif (Test-Path $existingInDir) { $existingInDir } else { $null }

if ($existingBin) {
    $installedVerRaw = (& $existingBin --version 2>&1) -join ''
    $installedVer = ($installedVerRaw -split '\s+' | Select-Object -Last 1).Trim()

    if ($installedVer -ne $latestVer -and $latestVer) {
        Write-Host "Upgrading ahma from $installedVer to $latestVer ..."
    } else {
        Write-Host "Ahma $installedVer is already installed and up to date."
        Write-Host ''
        Write-Host "  Location : $existingBin"
        Write-Host "  Simplify : available via 'ahma simplify --help'"
        Write-Host ''
        $confirm = Read-Host "Reinstall anyway? [y/N]"
        if ($confirm -notmatch '^[Yy]') {
            Write-Host 'No changes made.'
            exit 0
        }
        Write-Host 'Reinstalling...'
    }
}

Write-Host "Installing Ahma for $platform to $installDir ..."
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

$assetName = "ahma-release-$platform.zip"
$asset = $releaseJson.assets | Where-Object { $_.name -eq $assetName } | Select-Object -First 1

if (-not $asset) {
    Write-Error @"
Could not find release asset '$assetName'.
Please check https://github.com/paulirotta/ahma/releases for available binaries.
"@
    exit 1
}

$downloadUrl = $asset.browser_download_url
Write-Host "Downloading $downloadUrl ..."

# ── Download and extract ───────────────────────────────────────────────────────
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    $zipPath = Join-Path $tempDir $assetName
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -UseBasicParsing
    Expand-Archive -Path $zipPath -DestinationPath $tempDir -Force

    # ── Install binaries ───────────────────────────────────────────────────────
    Write-Host "Installing binaries to $installDir ..."

    foreach ($bin in @("ahma.exe")) {
        $src = Join-Path $tempDir $bin
        if (Test-Path $src) {
            Copy-Item -Path $src -Destination $installDir -Force
            Write-Host "  Installed $bin"
        } else {
            if ($bin -eq "ahma.exe") {
                Write-Error "ahma.exe not found in archive"
                exit 1
            }
        }
    }
} finally {
    Remove-Item -Recurse -Force -Path $tempDir -ErrorAction SilentlyContinue
}

# ── Verify and report ──────────────────────────────────────────────────────────
$mcpBin = Join-Path $installDir "ahma.exe"

& $mcpBin --version

# Remove legacy ahma-simplify.exe if present
$legacyCandidates = @(
    (Join-Path $installDir "ahma-simplify.exe"),
    (Join-Path "$HOME\.local\bin" "ahma-simplify.exe")
)
foreach ($legacy in $legacyCandidates) {
    if (Test-Path $legacy) {
        Remove-Item -Force $legacy -ErrorAction SilentlyContinue
        Write-Host "Removed legacy binary: $legacy"
        Write-Host "  Code complexity analysis is now built into ahma."
        Write-Host "  New command: ahma simplify <directory> --ai-fix 1"
    }
}

Write-Host ""
Write-Host "Success! Installed ahma to $installDir"
Write-Host ""
Write-Host "Ensure $installDir is in your PATH."
Write-Host "To add permanently, run:"
Write-Host "  [Environment]::SetEnvironmentVariable('PATH', `"`$env:PATH;$installDir`", 'User')"
Write-Host ""
Write-Host "PowerShell (built into Windows 10/11) is used at runtime. No additional installation needed."
Write-Host ""

# Run the Rust-based setup wizard
if ([Environment]::UserInteractive) {
    & $mcpBin setup
} else {
    Write-Host "Non-interactive shell detected. Running auto-setup..."
    & $mcpBin setup --auto
}
