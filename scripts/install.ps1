# One-liner installer for ahma on Windows
# Usage: irm https://raw.githubusercontent.com/ahma-labs/ahma/main/scripts/install.ps1 | iex
#
# Supported platforms:
#   - Windows x86_64 (x64)
#
# Requirements:
#   - PowerShell 5.1+ (built in to Windows 10/11)
#   - Internet access to GitHub releases
#
# Version indicator for build script check:
# -Version '0.22.0'
# Install-OneSkill -Version '0.22.0'
#
# Environment variables:
#   AHMA_INSTALL_DIR     - Override install directory (default: $HOME\.local\bin)

#Requires -Version 5

[CmdletBinding()]
param(
    [switch]$Verify,
    [switch]$InsecureSkipVerify,
    [string]$Mode = ""
)

$ErrorActionPreference = 'Stop'

$shouldVerify = $Verify
if ($args -contains "--verify" -or $args -contains "-v" -or $Mode -eq "verify") {
    $shouldVerify = $true
}

$shouldSkipVerify = $InsecureSkipVerify
if ($args -contains "--insecure-skip-verify" -or $args -contains "--insecure-skip-signature") {
    $shouldSkipVerify = $true
}
if ($env:AHMA_INSECURE_SKIP_VERIFY -eq "1" -or $env:AHMA_INSECURE_SKIP_VERIFY -eq "true" -or
    $env:AHMA_INSECURE_SKIP_SIGNATURE -eq "1" -or $env:AHMA_INSECURE_SKIP_SIGNATURE -eq "true") {
    $shouldSkipVerify = $true
}

function Get-FileSha256 {
    param (
        [string]$path
    )
    $hash = Get-FileHash -Path $path -Algorithm SHA256
    return $hash.Hash.ToLower()
}

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

# ── Verification-Only Mode ─────────────────────────────────────────────────────
if ($shouldVerify) {
    Write-Host "Verifying installed ahma binary against GitHub Build Provenance Attestation..."

    $existingCmd = Get-Command ahma -ErrorAction SilentlyContinue
    $existingInDir = Join-Path $installDir 'ahma.exe'
    $existingBin = if ($existingCmd) { $existingCmd.Source } elseif (Test-Path $existingInDir) { $existingInDir } else { $null }

    if (-not $existingBin) {
        Write-Error "ahma is not currently installed or not in PATH."
        exit 1
    }

    Write-Host "Found binary at: $existingBin"
    & $existingBin verify --self
    exit $LASTEXITCODE
}

# ── Fetch latest release metadata ─────────────────────────────────────────────
$releasesUrl = "https://api.github.com/repos/ahma-labs/ahma/releases/latest"
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
Please check https://github.com/ahma-labs/ahma/releases for available binaries.
"@
    exit 1
}

$sumsAsset = $releaseJson.assets | Where-Object { $_.name -eq "SHA256SUMS" } | Select-Object -First 1

$downloadUrl = $asset.browser_download_url
Write-Host "Downloading $downloadUrl ..."

# ── Download and extract ───────────────────────────────────────────────────────
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    $zipPath = Join-Path $tempDir $assetName

    # Download archive
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -UseBasicParsing

    # Sanity-check archive hash against SHA256SUMS manifest (defense in depth).
    # Sigstore attestation (verified post-install) is the cryptographic anchor.
    if ($sumsAsset) {
        $sumsPath = Join-Path $tempDir "SHA256SUMS"
        try {
            Invoke-WebRequest -Uri $sumsAsset.browser_download_url -OutFile $sumsPath -UseBasicParsing
            $sumsContent = Get-Content -Path $sumsPath
            $matchedLine = $sumsContent | Where-Object { $_ -match "\s+$([regex]::Escape($assetName))$" }
            if ($matchedLine) {
                $expectedHash = ($matchedLine -split '\s+')[0].Trim().ToLower()
                $actualHash = Get-FileSha256 -path $zipPath
                if ($expectedHash -ne $actualHash) {
                    Write-Error @"
########################################################################
CRITICAL SECURITY ERROR: Archive integrity check failed!
Checksum mismatch for $assetName.
Expected: $expectedHash
Actual:   $actualHash
########################################################################
"@
                    exit 1
                }
                Write-Host "Integrity verified: Archive hash matches release manifest."
            }
        } catch {
            Write-Host "Note: Could not verify SHA256SUMS manifest (will rely on Sigstore attestation)."
        }
    }

    # Expand archive
    Expand-Archive -Path $zipPath -DestinationPath $tempDir -Force

    # ── Install binaries ───────────────────────────────────────────────────────
    # Nothing running is stopped. Windows will not replace a running
    # executable, but it will rename one: the new binary is staged and
    # verified beside the old one, the old one is moved aside to ahma.old, and
    # the new one takes the path. The running hub notices the new file and
    # hands over to it once the work in flight is done; open editor sessions
    # reconnect on their own, and the next start removes ahma.old
    # (docs/hub.md, "Upgrades").
    Write-Host "Installing binaries to $installDir ..."

    $src = Join-Path $tempDir "ahma.exe"
    if (-not (Test-Path $src)) {
        Write-Error "ahma.exe not found in archive"
        exit 1
    }
    $mcpBin = Join-Path $installDir "ahma.exe"
    $stagedBin = Join-Path $installDir "ahma.new.exe"
    Copy-Item -Path $src -Destination $stagedBin -Force

    # Cryptographic verification: confirm the new binary has a valid GitHub Build Provenance
    # Attestation (Sigstore SLSA Level 3) from the official ahma-labs/ahma CI pipeline. It runs
    # before the binary takes the install path, so a failure leaves the previous install untouched.
    if (-not $shouldSkipVerify) {
        Write-Host "Verifying Sigstore Build Provenance Attestation..."
        & $stagedBin verify --self
        if ($LASTEXITCODE -ne 0) {
            Write-Error @"
########################################################################
CRITICAL SECURITY ERROR: Sigstore attestation verification FAILED!
The downloaded binary failed GitHub Build Provenance Attestation.
Removing $stagedBin; the existing install is unchanged.
########################################################################
"@
            Remove-Item -Force $stagedBin -ErrorAction SilentlyContinue
            exit 1
        }
    } else {
        Write-Warning "Sigstore attestation verification bypassed (--insecure-skip-verify)."
    }

    if (Test-Path $mcpBin) {
        $aside = Join-Path $installDir "ahma.old"
        Remove-Item -Force $aside -ErrorAction SilentlyContinue
        if (Test-Path $aside) {
            # An earlier ahma.old is itself still running.
            $aside = Join-Path $installDir ("ahma.{0}.old" -f [DateTimeOffset]::UtcNow.ToUnixTimeSeconds())
        }
        Move-Item -Path $mcpBin -Destination $aside -Force
    }
    Move-Item -Path $stagedBin -Destination $mcpBin -Force
    Write-Host "  Installed ahma.exe"
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
