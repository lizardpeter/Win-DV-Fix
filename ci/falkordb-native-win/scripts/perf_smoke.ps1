param(
    [string]$WorkDir = ""
)

$ErrorActionPreference = "Stop"
$Root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
if (-not $WorkDir) { $WorkDir = Join-Path $Root "build\perf-smoke" }
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

$ServerExe = Join-Path $Root "build\cargo-target\release\server.exe"
if (-not (Test-Path $ServerExe)) { throw "server.exe not found at $ServerExe" }
$DataDir = Join-Path $WorkDir "data"
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $DataDir
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$Stdout = Join-Path $WorkDir "server.stdout.txt"
$Stderr = Join-Path $WorkDir "server.stderr.txt"
$Result = Join-Path $WorkDir "perf.json"
$Password = "perf-ci-secret"
$Port = 6395

$server = Start-Process -FilePath $ServerExe -ArgumentList @(
    "--bind", "127.0.0.1:$Port",
    "--data-dir", $DataDir,
    "--password", $Password
) -PassThru -RedirectStandardOutput $Stdout -RedirectStandardError $Stderr

try {
    $ready = $false
    for ($i = 0; $i -lt 80; $i++) {
        Start-Sleep -Milliseconds 250
        if ($server.HasExited) { throw "performance smoke server exited early with code $($server.ExitCode)" }
        python -c "from redis import Redis; r=Redis(host='127.0.0.1',port=$Port,password='$Password'); print(r.ping())" *> $null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
    }
    if (-not $ready) { throw "performance smoke server did not become ready" }

    $env:CANDIDATE_PASSWORD = $Password
    $PerfScript = Join-Path $Root "tools\perf_compare.py"
    & python $PerfScript --candidate "127.0.0.1:$Port" --nodes 2000 --reps 50 --workers 4 --out $Result
    if ($LASTEXITCODE -ne 0) { throw "performance smoke failed with exit code $LASTEXITCODE" }
    if (-not (Test-Path $Result)) { throw "performance smoke did not create $Result" }

    $json = Get-Content $Result -Raw | ConvertFrom-Json
    if (-not $json.candidate.metrics -or $json.candidate.metrics.Count -lt 8) {
        throw "performance smoke returned incomplete metrics"
    }

    Write-Host "PERFORMANCE_SMOKE_PASS"
    Write-Host "Results: $Result"
}
finally {
    Remove-Item Env:CANDIDATE_PASSWORD -ErrorAction SilentlyContinue
    if ($server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force
        $server.WaitForExit()
    }
}
