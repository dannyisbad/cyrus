# Builds the cyrus release: a SINGLE self-contained cyrus.exe -> dist/
#
# codex (the pinned patched fork) and cloudflared are zstd-compressed and
# embedded into cyrus.exe; they're extracted to ~/.cyrus/bin on first run. The
# user ships and runs ONE file — no sibling binaries, no PATH install, no way
# for a stray npm codex to shadow the patched fork.
#
# Order matters: codex is built first because cyrus's build.rs reads its bytes.
#
# Usage:  pwsh scripts/release.ps1            # release single-binary into ./dist
#         pwsh scripts/release.ps1 -Debug     # faster debug single-binary
#         pwsh scripts/release.ps1 -Separate  # old side-by-side layout (no embed)

[CmdletBinding()]
param(
    [switch]$Debug,
    [switch]$Separate,
    [string]$OutDir   = (Join-Path $PSScriptRoot '..' 'dist'),
    [string]$BuildDir = (Join-Path $env:TEMP 'cyrus-codex-build')
)

$ErrorActionPreference = 'Stop'
$root        = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$profileFlag = if ($Debug) { '' } else { '--release' }
$profileDir  = if ($Debug) { 'debug' } else { 'release' }

# --- 1. parse scripts/codex.lock -------------------------------------------
$cfg = @{}
Get-Content (Join-Path $PSScriptRoot 'codex.lock') |
    Where-Object { $_ -notmatch '^\s*#' } |
    ForEach-Object { if ($_ -match '^\s*(\w+)\s*=\s*(.+?)\s*$') { $cfg[$Matches[1]] = $Matches[2] } }
$url = $cfg['url']; $sha = $cfg['sha']
if (-not $url -or -not $sha) { throw 'codex.lock is missing url or sha' }
Write-Host "codex fork: $url @ $sha" -ForegroundColor DarkGray

# --- 2. fetch + build the pinned-fork codex (must precede the cyrus build) ---
if (-not (Test-Path (Join-Path $BuildDir '.git'))) {
    Write-Host "cloning codex fork into $BuildDir ..." -ForegroundColor DarkGray
    git clone --no-checkout $url $BuildDir
}
git -C $BuildDir fetch --all --quiet
git -C $BuildDir -c advice.detachedHead=false checkout --quiet $sha
$haveSha = (git -C $BuildDir rev-parse HEAD).Trim()
if ($haveSha -ne $sha) { throw "codex checkout is $haveSha, expected pinned $sha" }

Write-Host "building codex ($profileDir) ..." -ForegroundColor Cyan
& cargo build $profileFlag --manifest-path (Join-Path $BuildDir 'codex-rs' 'Cargo.toml') --bin codex
if ($LASTEXITCODE) { throw "codex build failed ($LASTEXITCODE)" }
$codexExe = Join-Path $BuildDir "codex-rs\target\$profileDir\codex.exe"
if (-not (Test-Path $codexExe)) { throw "codex.exe not found at $codexExe" }

# --- 3. locate cloudflared (embedded too) ----------------------------------
$cfExe = $null
$cf = Get-Command cloudflared -ErrorAction SilentlyContinue
if ($cf) { $cfExe = $cf.Source }
elseif (Test-Path 'C:\Program Files (x86)\cloudflared\cloudflared.exe') { $cfExe = 'C:\Program Files (x86)\cloudflared\cloudflared.exe' }
if (-not $cfExe) { Write-Warning 'cloudflared not found - the single binary will rely on a system cloudflared at runtime' }

New-Item -ItemType Directory -Force $OutDir | Out-Null

if ($Separate) {
    # Old layout: cyrus.exe + codex.exe (+ cloudflared) side by side, no embed.
    Write-Host "building cyrus ($profileDir, separate) ..." -ForegroundColor Cyan
    & cargo build $profileFlag --manifest-path (Join-Path $root 'Cargo.toml') -p cyrus-setup --bin cyrus
    if ($LASTEXITCODE) { throw "cyrus build failed ($LASTEXITCODE)" }
    Copy-Item (Join-Path $root "target\$profileDir\cyrus.exe") $OutDir -Force
    Copy-Item $codexExe $OutDir -Force
    if ($cfExe) { Copy-Item $cfExe $OutDir -Force }
}
else {
    # Single binary: embed codex (+ cloudflared) into cyrus.exe via build.rs.
    Write-Host "building cyrus ($profileDir, single-binary, embedding codex$(if($cfExe){' + cloudflared'})) ..." -ForegroundColor Cyan
    $env:CYRUS_EMBED_CODEX = $codexExe
    if ($cfExe) { $env:CYRUS_EMBED_CLOUDFLARED = $cfExe } else { Remove-Item Env:\CYRUS_EMBED_CLOUDFLARED -ErrorAction SilentlyContinue }
    try {
        & cargo build $profileFlag --manifest-path (Join-Path $root 'Cargo.toml') -p cyrus-setup --bin cyrus
        if ($LASTEXITCODE) { throw "cyrus build failed ($LASTEXITCODE)" }
    }
    finally {
        Remove-Item Env:\CYRUS_EMBED_CODEX -ErrorAction SilentlyContinue
        Remove-Item Env:\CYRUS_EMBED_CLOUDFLARED -ErrorAction SilentlyContinue
    }
    Copy-Item (Join-Path $root "target\$profileDir\cyrus.exe") $OutDir -Force
}

Write-Host "`ndist ready: $OutDir" -ForegroundColor Green
Get-ChildItem $OutDir | Select-Object Name, @{n='MB';e={[math]::Round($_.Length/1MB,1)}}
