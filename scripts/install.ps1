# Install prebuilt `ravel` on Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/<owner>/ravel/main/scripts/install.ps1 | iex
#
# Env:
#   $env:RAVEL_GITHUB_REPO   owner/repo (default: guigaoliveira/ravel)
#   $env:RAVEL_VERSION       tag without v, or latest
#   $env:RAVEL_INSTALL_DIR   default: $env:LOCALAPPDATA\ravel\bin
#   $env:RAVEL_FROM_SOURCE=1 force cargo install

$ErrorActionPreference = "Stop"

$Repo = if ($env:RAVEL_GITHUB_REPO) { $env:RAVEL_GITHUB_REPO } else { "guigaoliveira/ravel" }
$Version = if ($env:RAVEL_VERSION) { $env:RAVEL_VERSION } else { "latest" }
$InstallDir = if ($env:RAVEL_INSTALL_DIR) { $env:RAVEL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "ravel\bin" }

function Install-FromCargo {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found. Install Rust from https://rustup.rs or wait for a GitHub release binary."
    }
    Write-Host "building from source via cargo install…"
    cargo install --git "https://github.com/$Repo.git" --locked --force
    Write-Host "installed (cargo). Ensure %USERPROFILE%\.cargo\bin is on PATH."
    Write-Host "next: ravel install --yes"
    exit 0
}

if ($env:RAVEL_FROM_SOURCE -eq "1") {
    Install-FromCargo
}

$arch = if ([Environment]::Is64BitOperatingSystem) { "x86_64" } else { "i686" }
# Prefer MSVC target (matches rustup default on Windows)
$Target = "${arch}-pc-windows-msvc"
$Asset = "ravel-${Target}.zip"

if ($Version -eq "latest") {
    $Base = "https://github.com/$Repo/releases/latest/download"
} else {
    $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
    $Base = "https://github.com/$Repo/releases/download/$tag"
}
$Url = "$Base/$Asset"

Write-Host "repo:   $Repo"
Write-Host "target: $Target"
Write-Host "url:    $Url"

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ravel-install-" + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $Zip = Join-Path $Tmp $Asset
    try {
        Invoke-WebRequest -Uri $Url -OutFile $Zip -UseBasicParsing
    } catch {
        Write-Host "prebuilt asset not found — falling back to cargo install"
        Install-FromCargo
    }

    Expand-Archive -Path $Zip -DestinationPath $Tmp -Force
    $Bin = Get-ChildItem -Path $Tmp -Recurse -Filter "ravel.exe" | Select-Object -First 1
    if (-not $Bin) { throw "archive did not contain ravel.exe" }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Dest = Join-Path $InstallDir "ravel.exe"
    Copy-Item $Bin.FullName $Dest -Force
    Write-Host "installed: $Dest"
    & $Dest --version

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "added $InstallDir to user PATH (open a new terminal)"
    }

    Write-Host ""
    Write-Host "Wire agents:"
    Write-Host "  ravel install --yes"
    Write-Host "Then in each project:"
    Write-Host "  cd your-repo; ravel index; ravel status"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
