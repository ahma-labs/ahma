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
# -Version '0.9.3'
# Install-OneSkill -Version '0.9.3'
#
# Environment variables:
#   AHMA_INSTALL_DIR     - Override install directory (default: $HOME\.local\bin)

#Requires -Version 5

[CmdletBinding()]
param(
    [switch]$Verify,
    [switch]$InsecureSkipSignature,
    [string]$Mode = ""
)

$ErrorActionPreference = 'Stop'

# Public key for release verification
$PUB_KEY_XML = '<RSAKeyValue><Modulus>5veFxEchlM3iyFx8BQzsf+yn6ZNJygRwfOfLS901Rxm/I3YRwn2Jksyp2bVckjgDeGJVK7IPGaHe1dL7+Ljn5V3zvU9B7CLeeIGdZRRngV/n6r+dsGy0FWQIcN/+dfKPWvhz4m/4QMTLXL05WK8jiI/Qatp2Fs32CUJTJ6NpIDQZi4xd1xhQbF/jk2+pwgwpup7kAVKPa49QegFQEQcSi8duqBKX2ynTA6QhknBX1fY+6vEFLh6uMePjzGyHLax8mMg8sk2WU59bgMGgtPPyle7gp692r3UaP9YgzuNDTyDSoU4gJmOOYYAtMWkNOyD2Bcr8JndwPXG0CD3Hj+j0Gw==</Modulus><Exponent>AQAB</Exponent></RSAKeyValue>'

$shouldVerify = $Verify
if ($args -contains "--verify" -or $args -contains "-v" -or $Mode -eq "verify") {
    $shouldVerify = $true
}

$shouldSkipSignature = $InsecureSkipSignature
if ($args -contains "--insecure-skip-signature" -or $args -contains "-insecure-skip-signature") {
    $shouldSkipSignature = $true
}

# ── Cryptographic Helpers ─────────────────────────────────────────────────────
function Verify-Signature {
    param (
        [string]$dataPath,
        [string]$sigPath
    )
    
    if ($shouldSkipSignature -or $env:AHMA_INSECURE_SKIP_SIGNATURE -eq "1" -or $env:AHMA_INSECURE_SKIP_SIGNATURE -eq "true") {
        Write-Warning "Skipping cryptographic release signature verification!"
        return $true
    }
    
    try {
        $rsa = New-Object System.Security.Cryptography.RSACryptoServiceProvider
        $rsa.FromXmlString($PUB_KEY_XML)
        
        $dataBytes = [System.IO.File]::ReadAllBytes($dataPath)
        $sigBytes = [System.IO.File]::ReadAllBytes($sigPath)
        
        $hashAlg = [System.Security.Cryptography.CryptoConfig]::MapNameToOID("SHA256")
        $isValid = $rsa.VerifyData($dataBytes, $hashAlg, $sigBytes)
        
        $rsa.Dispose()
        return $isValid
    } catch {
        Write-Warning "Signature verification error: $_"
        return $false
    }
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
    Write-Host "Checking installed binary signature..."
    
    $existingBin = $null
    $existingCmd = Get-Command ahma -ErrorAction SilentlyContinue
    $existingInDir = Join-Path $installDir 'ahma.exe'
    if ($existingCmd) {
        $existingBin = $existingCmd.Source
    } elseif (Test-Path $existingInDir) {
        $existingBin = $existingInDir
    }
    
    if (-not $existingBin) {
        Write-Error "ahma is not currently installed or not in PATH."
        exit 1
    }
    
    Write-Host "Found binary at: $existingBin"
    $localHash = Get-FileSha256 -path $existingBin
    Write-Host "Local SHA-256: $localHash"
    
    $releasesUrl = "https://api.github.com/repos/paulirotta/ahma/releases/latest"
    Write-Host "Fetching latest release info..."
    try {
        $releaseJson = Invoke-RestMethod -Uri $releasesUrl -UseBasicParsing
    } catch {
        Write-Error "Failed to fetch release info: $_"
        exit 1
    }
    
    $sumsAsset = $releaseJson.assets | Where-Object { $_.name -eq "SHA256SUMS" } | Select-Object -First 1
    $sigAsset  = $releaseJson.assets | Where-Object { $_.name -eq "SHA256SUMS.sig" } | Select-Object -First 1
    
    if (-not $sumsAsset -or -not $sigAsset) {
        Write-Error "Could not find SHA256SUMS or SHA256SUMS.sig in the latest release."
        exit 1
    }
    
    $tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
    
    try {
        $sumsPath = Join-Path $tempDir "SHA256SUMS"
        $sigPath = Join-Path $tempDir "SHA256SUMS.sig"
        
        Write-Host "Downloading release manifest and signature..."
        Invoke-WebRequest -Uri $sumsAsset.browser_download_url -OutFile $sumsPath -UseBasicParsing
        Invoke-WebRequest -Uri $sigAsset.browser_download_url -OutFile $sigPath -UseBasicParsing
        
        $sumsContent = Get-Content -Path $sumsPath
        
        $isValid = Verify-Signature -dataPath $sumsPath -sigPath $sigPath
        if (-not $isValid) {
            Write-Error @"
########################################################################
CRITICAL SECURITY ERROR: Release signature verification FAILED!
The checksums file is NOT signed by the official private key.
This release might be compromised or tampered with.
########################################################################
"@
            exit 1
        }
        Write-Host "Authenticity verified: Release signature is valid."
        
        $matchedLine = $sumsContent | Where-Object { $_ -match "^$localHash\s+" }
        
        if ($matchedLine) {
            Write-Host "Success: Installed binary matches a verified release entry!"
            Write-Host "Verified: $($matchedLine.Trim())"
            exit 0
        } else {
            Write-Error @"
########################################################################
SECURITY WARNING: Local binary verification FAILED!
The local hash '$localHash' does not match any entry in the verified release manifest.
The binary may have been modified or is a different/unreleased version.
########################################################################
"@
            exit 1
        }
    } finally {
        Remove-Item -Recurse -Force -Path $tempDir -ErrorAction SilentlyContinue
    }
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

$sumsAsset = $releaseJson.assets | Where-Object { $_.name -eq "SHA256SUMS" } | Select-Object -First 1
$sigAsset  = $releaseJson.assets | Where-Object { $_.name -eq "SHA256SUMS.sig" } | Select-Object -First 1

if (-not $sumsAsset -or -not $sigAsset) {
    Write-Error "Could not find SHA256SUMS or SHA256SUMS.sig in the latest release."
    exit 1
}

$downloadUrl = $asset.browser_download_url
Write-Host "Downloading $downloadUrl ..."

# ── Download and extract ───────────────────────────────────────────────────────
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    $sumsPath = Join-Path $tempDir "SHA256SUMS"
    $sigPath = Join-Path $tempDir "SHA256SUMS.sig"
    $zipPath = Join-Path $tempDir $assetName
    
    Write-Host "Downloading release manifest and signature..."
    Invoke-WebRequest -Uri $sumsAsset.browser_download_url -OutFile $sumsPath -UseBasicParsing
    Invoke-WebRequest -Uri $sigAsset.browser_download_url -OutFile $sigPath -UseBasicParsing
    
    $sumsContent = Get-Content -Path $sumsPath
    
    # Verify signature
    $isValid = Verify-Signature -dataPath $sumsPath -sigPath $sigPath
    if (-not $isValid) {
        Write-Error @"
########################################################################
CRITICAL SECURITY ERROR: Release signature verification FAILED!
The checksums file is NOT signed by the official private key.
This release might be compromised or tampered with.
########################################################################
"@
        exit 1
    }
    Write-Host "Authenticity verified: Release signature is valid."
    
    # Extract expected hash
    $matchedLine = $sumsContent | Where-Object { $_ -match "\s+$([regex]::Escape($assetName))$" }
    if (-not $matchedLine) {
        Write-Error "Error: Checksum entry for '$assetName' not found in release manifest."
        exit 1
    }
    $expectedHash = ($matchedLine -split '\s+')[0].Trim().ToLower()
    
    # Download zip file
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -UseBasicParsing
    
    # Verify zip file hash
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
    
    # Expand
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
    
    # Verify the installed binary hash and print it
    $mcpBin = Join-Path $installDir "ahma.exe"
    $installedHash = Get-FileSha256 -path $mcpBin
    Write-Host "Installed binary hash: $installedHash"
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
