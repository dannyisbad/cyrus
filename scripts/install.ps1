# cyrus installer (Windows).
#
#   irm https://mundy.sh/install/cyrus.ps1 | iex
#
# Downloads the matching self-contained cyrus.exe from the latest GitHub release,
# drops it in %USERPROFILE%\.cyrus\bin, and adds that to your user PATH. One file,
# no build. Re-run any time to update.
#
# Env knobs:  $env:CYRUS_INSTALL_DIR, $env:CYRUS_VERSION (e.g. v0.1.0), $env:CYRUS_REPO
$ErrorActionPreference = 'Stop'

$repo    = if ($env:CYRUS_REPO) { $env:CYRUS_REPO } else { 'dannyisbad/cyrus' }
$version = if ($env:CYRUS_VERSION) { $env:CYRUS_VERSION } else { 'latest' }
$dir     = if ($env:CYRUS_INSTALL_DIR) { $env:CYRUS_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.cyrus\bin' }

function Info($m) { Write-Host "  $m" -ForegroundColor DarkGray }
function Die($m)  { Write-Host "cyrus install: $m" -ForegroundColor Red; exit 1 }

# --- detect arch ------------------------------------------------------------
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    'AMD64' { $cpu = 'x86_64' }
    'ARM64' { $cpu = 'aarch64' }
    default { Die "unsupported CPU '$arch'. Build from source: https://github.com/$repo" }
}
$target = "$cpu-pc-windows-msvc"
$asset  = "cyrus-$target.zip"

$url = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download/$asset"
} else {
    "https://github.com/$repo/releases/download/$version/$asset"
}

Write-Host "Installing cyrus" -ForegroundColor White
Info "$target | $version"
Write-Host ""

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("cyrus-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    $zip = Join-Path $tmp $asset
    Info "downloading $asset"
    # TLS 1.2 for older Windows PowerShell defaults.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    } catch {
        Die "download failed: $url`n       (no release asset for $target yet? see https://github.com/$repo/releases)"
    }

    Info "unpacking"
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Join-Path $tmp 'cyrus.exe'
    if (-not (Test-Path $exe)) { Die "archive did not contain cyrus.exe." }

    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item $exe (Join-Path $dir 'cyrus.exe') -Force
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host "ok  cyrus.exe -> $dir\cyrus.exe" -ForegroundColor Green
Write-Host ""

# --- PATH (user scope) ------------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
$onPath = ($userPath -split ';') -contains $dir
if (-not $onPath) {
    $newPath = if ($userPath.TrimEnd(';')) { "$($userPath.TrimEnd(';'));$dir" } else { $dir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path = "$env:Path;$dir"  # current session, so the lines below work now
    Info "added $dir to your user PATH"
}

Write-Host "Done." -NoNewline -ForegroundColor White
Write-Host " Open a new terminal, then:"
Write-Host "  cyrus setup" -NoNewline -ForegroundColor White
Write-Host "   # one-time: connect your ChatGPT session" -ForegroundColor DarkGray
Write-Host "  cyrus" -NoNewline -ForegroundColor White
Write-Host "         # codex on the plan you already pay for" -ForegroundColor DarkGray
