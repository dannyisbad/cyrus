# Builds the cyrus release bundle: cyrus.exe + the pinned patched codex.exe -> dist/
#
# cyrus ships two binaries side by side. cyrus.exe is built from this workspace;
# codex.exe is built from the fork pinned in scripts/codex.lock (cloned at the
# exact SHA into a cached build dir — no vendored copy in this repo).
#
# Usage:  pwsh scripts/release.ps1            # release build into ./dist
#         pwsh scripts/release.ps1 -Debug     # faster debug build

[CmdletBinding()]
param(
    [switch]$Debug,
    [string]$OutDir   = (Join-Path $PSScriptRoot '..' 'dist'),
    [string]$BuildDir = (Join-Path $env:TEMP 'cyrus-codex-build')
)

$ErrorActionPreference = 'Stop'
$root      = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
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

# --- 2. fetch the codex fork at the pinned SHA -----------------------------
if (-not (Test-Path (Join-Path $BuildDir '.git'))) {
    Write-Host "cloning codex fork into $BuildDir ..." -ForegroundColor DarkGray
    git clone --no-checkout $url $BuildDir
}
git -C $BuildDir fetch --all --quiet
git -C $BuildDir -c advice.detachedHead=false checkout --quiet $sha
$haveSha = (git -C $BuildDir rev-parse HEAD).Trim()
if ($haveSha -ne $sha) { throw "codex checkout is $haveSha, expected pinned $sha" }

# --- 3. build both workspaces ----------------------------------------------
Write-Host "building cyrus ($profileDir) ..." -ForegroundColor Cyan
& cargo build $profileFlag --manifest-path (Join-Path $root 'Cargo.toml') -p cyrus-setup --bin cyrus
if ($LASTEXITCODE) { throw "cyrus build failed ($LASTEXITCODE)" }

Write-Host "building codex ($profileDir) ..." -ForegroundColor Cyan
& cargo build $profileFlag --manifest-path (Join-Path $BuildDir 'codex-rs' 'Cargo.toml') --bin codex
if ($LASTEXITCODE) { throw "codex build failed ($LASTEXITCODE)" }

# --- 4. assemble dist/ ------------------------------------------------------
New-Item -ItemType Directory -Force $OutDir | Out-Null
Copy-Item (Join-Path $root      "target\$profileDir\cyrus.exe")          $OutDir -Force
Copy-Item (Join-Path $BuildDir  "codex-rs\target\$profileDir\codex.exe") $OutDir -Force

# cloudflared is the tunnel. Bundle it if on PATH; otherwise the target needs it.
$cf = Get-Command cloudflared -ErrorAction SilentlyContinue
if ($cf) { Copy-Item $cf.Source $OutDir -Force }
else { Write-Warning 'cloudflared not on PATH - bundle it into dist/ manually, or require it on the target machine' }

Write-Host "`ndist ready: $OutDir" -ForegroundColor Green
Get-ChildItem $OutDir | Select-Object Name, @{n='MB';e={[math]::Round($_.Length/1MB,1)}}
