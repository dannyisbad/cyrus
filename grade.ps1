# direct /mcp grader for chimera reliability fixes. loopback + bearer.
param(
  [string]$Url = 'http://127.0.0.1:8787/mcp',
  [int]$Port = 8787
)

$secrets = Get-Content C:\Users\Danny\.cyrus\secrets.json -Raw
if ($secrets -notmatch '"bearer_token"\s*:\s*"([^"]+)"') { throw 'no bearer_token in secrets.json' }
$bearer = $Matches[1]

function Invoke-Tool {
  param([string]$Name, [hashtable]$ToolArgs, [int]$TimeoutSec = 120)
  $body = @{ jsonrpc = '2.0'; id = 1; method = 'tools/call'; params = @{ name = $Name; arguments = $ToolArgs } } | ConvertTo-Json -Depth 10 -Compress
  $headers = @{
    'Authorization' = "Bearer $bearer"
    'Accept'        = 'application/json, text/event-stream'
    'Content-Type'  = 'application/json'
  }
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  try {
    $resp = Invoke-WebRequest -Uri $Url -Method Post -Headers $headers -Body $body -TimeoutSec $TimeoutSec -UseBasicParsing
    $sw.Stop()
    $raw = $resp.Content
    # SSE: pull the data: {...} line(s)
    $dataLines = ($raw -split "`n") | Where-Object { $_ -match '^data:' } | ForEach-Object { ($_ -replace '^data:\s?', '') }
    $json = ($dataLines -join '')
    $obj = $null
    try { $obj = $json | ConvertFrom-Json } catch {}
    [PSCustomObject]@{ ok = $true; ms = $sw.ElapsedMilliseconds; status = $resp.StatusCode; obj = $obj; raw = $raw }
  } catch {
    $sw.Stop()
    [PSCustomObject]@{ ok = $false; ms = $sw.ElapsedMilliseconds; status = $_.Exception.Response.StatusCode.value__; err = $_.Exception.Message; raw = $null }
  }
}

function Show-Result {
  param([string]$Label, $R)
  $head = if ($R.ok) { "PASS-CALL" } else { "FAIL-CALL" }
  Write-Output ("[{0}] {1}  {2}ms  http={3}" -f $head, $Label, $R.ms, $R.status)
  if ($R.obj -and $R.obj.result) {
    $isErr = $R.obj.result.isError
    $text = ''
    if ($R.obj.result.content) { $text = ($R.obj.result.content | ForEach-Object { $_.text }) -join "`n" }
    Write-Output ("    isError={0}" -f $isErr)
    $preview = if ($text.Length -gt 600) { $text.Substring(0,600) + ' …' } else { $text }
    Write-Output ("    text: {0}" -f ($preview -replace "`n", "`n          "))
  } elseif ($R.obj) {
    Write-Output ("    raw-json: {0}" -f (($R.obj | ConvertTo-Json -Depth 6 -Compress)))
  } elseif (-not $R.ok) {
    Write-Output ("    err: {0}" -f $R.err)
  } else {
    Write-Output ("    raw: {0}" -f ($R.raw.Substring(0,[Math]::Min(400,$R.raw.Length))))
  }
  Write-Output ''
}
