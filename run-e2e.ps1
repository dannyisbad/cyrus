# Phase C: real task through ChatGPT via cyrus -> codex -> lipsync -> ChatGPT -> repo connector tools -> chimera.
# Forces the exact failure path (repo_glob/repo_status/repo_grep on a real repo).
param(
  [string]$Prompt = "Use the repo connector to do three things on this codebase and report concisely: (1) list every Cargo.toml file, (2) show the git status, (3) grep for the string ""SHIM_CONDUCTOR"" and show which files contain it. If the repo connector tools are not loaded yet, load them first."
)

$ErrorActionPreference = 'Stop'
$cyrus = 'C:\Users\Danny\Desktop\cyrus\target\debug\cyrus.exe'
$codex = 'C:\Users\Danny\Desktop\codex\codex-rs\target\debug\codex.exe'
$repo  = 'C:\Users\Danny\Desktop\codex'

$env:CYRUS_CODEX_BIN = $codex
$env:RUST_LOG = 'info'

Write-Host "=== driving codex-on-ChatGPT against $repo ===" -ForegroundColor Cyan
Write-Host "prompt: $Prompt" -ForegroundColor DarkGray
Write-Host ""

Set-Location $repo
# cyrus exec = non-interactive codex run; passthrough injects model_provider=shadow
& $cyrus exec --skip-git-repo-check $Prompt
$code = $LASTEXITCODE
Write-Host ""
Write-Host "=== codex exit code: $code ===" -ForegroundColor Cyan
