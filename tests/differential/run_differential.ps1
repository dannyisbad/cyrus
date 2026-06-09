# Standalone differential runner (no cargo test harness).
#
# Builds the Rust port emitter, then for each area runs BOTH the original
# (python idare/shadow, node repo-agent-mcp) and the Rust port, and byte-diffs
# the two canonical reports. Prints a PASS/FAIL/SKIP line per area and a summary.
#
# Usage:  pwsh tests/differential/run_differential.ps1
#
# Exit code 0 iff every comparable area matched.

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here "..\..")     # cyrus/
$fixtures = Resolve-Path (Join-Path $here "..\fixtures")
$pyDriver = Join-Path $here "drivers\emit_python.py"
$nodeDriver = Join-Path $here "drivers\emit_node.mjs"

Write-Host "== building cyrus-diff-emit ==" -ForegroundColor Cyan
Push-Location $root
cargo build -q -p cyrus-differential --bin cyrus-diff-emit
Pop-Location
$emit = Join-Path $root "target\debug\cyrus-diff-emit.exe"

# area -> original driver spec: @{ prog=...; script=... } ($null prog = python)
$areas = @(
  @{ area = "v1delta";          prog = "python"; script = $pyDriver },
  @{ area = "sse";              prog = "python"; script = $pyDriver },
  @{ area = "parse_tool_call";  prog = "python"; script = $pyDriver },
  @{ area = "relay";            prog = "python"; script = $pyDriver },
  @{ area = "oauth";            prog = "node";   script = $nodeDriver }
)

$pass = 0; $fail = 0; $skip = 0
$tmp = [System.IO.Path]::GetTempPath()

foreach ($a in $areas) {
  $area = $a.area
  $rustFile = Join-Path $tmp "diff_rust_$area.txt"
  $origFile = Join-Path $tmp "diff_orig_$area.txt"

  & $emit $area $fixtures | Set-Content -NoNewline -Encoding utf8 $rustFile

  $prog = $a.prog
  $haveProg = $null -ne (Get-Command $prog -ErrorAction SilentlyContinue)
  if (-not $haveProg) {
    Write-Host ("SKIP   {0,-16} ({1} not found)" -f $area, $prog) -ForegroundColor Yellow
    $skip++
    continue
  }

  & $prog $a.script $area $fixtures | Set-Content -NoNewline -Encoding utf8 $origFile
  if ($LASTEXITCODE -eq 86) {
    # Driver sentinel: original source tree not configured (CYRUS_SHADOW_PY_ROOT
    # / CYRUS_OAUTH_TS unset). The originals are not part of this repo.
    Write-Host ("SKIP   {0,-16} (original not configured; see drivers/)" -f $area) -ForegroundColor Yellow
    $skip++
    continue
  }

  $rustBytes = [System.IO.File]::ReadAllBytes($rustFile)
  $origBytes = [System.IO.File]::ReadAllBytes($origFile)
  $same = $rustBytes.Length -eq $origBytes.Length
  if ($same) {
    for ($i = 0; $i -lt $rustBytes.Length; $i++) {
      if ($rustBytes[$i] -ne $origBytes[$i]) { $same = $false; break }
    }
  }

  if ($same) {
    Write-Host ("PASS   {0,-16} ({1} bytes)" -f $area, $rustBytes.Length) -ForegroundColor Green
    $pass++
  } else {
    Write-Host ("FAIL   {0,-16} (Rust port != {1} original)" -f $area, $prog) -ForegroundColor Red
    $rl = Get-Content $rustFile; $ol = Get-Content $origFile
    for ($i = 0; $i -lt [Math]::Max($rl.Count, $ol.Count); $i++) {
      if ($rl[$i] -ne $ol[$i]) {
        Write-Host ("         first diff at line {0}:" -f ($i + 1)) -ForegroundColor Red
        Write-Host ("           rust: {0}" -f $rl[$i])
        Write-Host ("           orig: {0}" -f $ol[$i])
        break
      }
    }
    $fail++
  }
}

Write-Host ""
Write-Host ("RESULT: {0} passed, {1} failed, {2} skipped" -f $pass, $fail, $skip) -ForegroundColor Cyan
if ($fail -gt 0) { exit 1 } else { exit 0 }
