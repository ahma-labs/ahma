# Bootstrap installer for ahma on Windows
#
# Usage:
#   irm https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.ps1 | iex
#   irm https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.ps1 | iex -Ref main
#   irm ... | iex -Ref feature/update
#   irm ... | iex -Ref 0.6.7
#
# If ahma is already installed, delegates to: ahma update [ref]
#
# Environment variables:
#   AHMA_INSTALL_DIR - Override install directory (default: $HOME\.local\bin)

#Requires -Version 5

param(
    [Parameter(Position = 0)]
    [string]$Ref = $null
)

$ErrorActionPreference = 'Stop'

$installDir = if ($env:AHMA_INSTALL_DIR) {
    $env:AHMA_INSTALL_DIR
} else {
    Join-Path $HOME ".local\bin"
}
$cargoRoot = Split-Path $installDir -Parent
$gitRepo   = "https://github.com/paulirotta/ahma"

function Test-SemVerRef([string]$Reference) {
    return $Reference -match '^(v)?\d+\.\d+\.\d+$'
}

function Normalize-ReleaseTag([string]$Reference) {
    if ($Reference -match '^v') { return $Reference }
    return "v$Reference"
}

function Ensure-InstallDir {
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
}

function Invoke-AhmaUpdate {
    param([string]$Reference)
    $updateArgs = @("update")
    if ($Reference) { $updateArgs += $Reference }
    Write-Host "ahma is already installed — running: ahma $($updateArgs -join ' ')"
    & ahma @updateArgs
    exit $LASTEXITCODE
}

function Install-FromGit {
    param([string]$Branch)
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Error "cargo is not installed. Install Rust from https://rustup.rs/ then retry."
    }
    Ensure-InstallDir
    Write-Host "Building ahma from Git branch '$Branch' (this may take several minutes)..."
    cargo install --git $gitRepo --branch $Branch ahma_mcp --bin ahma --root $cargoRoot --locked --force
    Write-Host "Success! Installed ahma to $installDir"
    Write-Host "Ensure $installDir is on your PATH."
}

function Install-FromRelease {
    param([string]$Tag)

    if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
        Write-Error "Unsupported architecture: $($env:PROCESSOR_ARCHITECTURE). Only x86_64 Windows builds are available."
    }

    Ensure-InstallDir

    if ($Tag) {
        $releasesUrl = "https://api.github.com/repos/paulirotta/ahma/releases/tags/$(Normalize-ReleaseTag $Tag)"
    } else {
        $releasesUrl = "https://api.github.com/repos/paulirotta/ahma/releases/latest"
    }

    Write-Host "Fetching release info..."
    $releaseJson = Invoke-RestMethod -Uri $releasesUrl -UseBasicParsing
    $assetName = "ahma-release-windows-x86_64.zip"
    $asset = $releaseJson.assets | Where-Object { $_.name -eq $assetName } | Select-Object -First 1
    if (-not $asset) {
        Write-Error "Could not find release asset '$assetName'. See https://github.com/paulirotta/ahma/releases"
    }

    $tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
    try {
        $zipPath = Join-Path $tempDir $assetName
        Write-Host "Downloading $($asset.browser_download_url) ..."
        Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zipPath -UseBasicParsing
        Expand-Archive -Path $zipPath -DestinationPath $tempDir -Force
        $src = Join-Path $tempDir "ahma.exe"
        if (-not (Test-Path $src)) {
            Write-Error "ahma.exe not found in archive"
        }
        Copy-Item -Path $src -Destination (Join-Path $installDir "ahma.exe") -Force
    } finally {
        Remove-Item -Recurse -Force -Path $tempDir -ErrorAction SilentlyContinue
    }

    & (Join-Path $installDir "ahma.exe") --version
    Write-Host "Success! Installed ahma to $installDir"
    Write-Host "Ensure $installDir is on your PATH."
    Write-Host "To add permanently:"
    Write-Host "  [Environment]::SetEnvironmentVariable('PATH', `"`$env:PATH;$installDir`", 'User')"
}

# ── Main ──────────────────────────────────────────────────────────────────────

$existing = Get-Command ahma -ErrorAction SilentlyContinue
if ($existing) {
    Invoke-AhmaUpdate -Reference $Ref
}

if ($Ref -and -not (Test-SemVerRef $Ref)) {
    Install-FromGit -Branch $Ref
} elseif ($Ref) {
    Install-FromRelease -Tag $Ref
} else {
    Install-FromRelease -Tag $null
}
