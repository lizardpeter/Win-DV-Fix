param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Stop"
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Prefix = Join-Path $WorkDir "native-prefix"
$Logs = Join-Path $WorkDir "logs"
New-Item -ItemType Directory -Force -Path $Logs | Out-Null

$LibDirFile = Join-Path $WorkDir "native-lib-dir.txt"
if (-not (Test-Path $LibDirFile)) {
    throw "native-lib-dir.txt missing; bootstrap_windows.ps1 must run first"
}
$NativeLibDir = (Get-Content $LibDirFile -Raw).Trim()

$env:GRAPHBLAS_LIB_DIR = $NativeLibDir
$env:LAGRAPH_LIB_DIR = $NativeLibDir
$env:FALKORDB_NATIVE_NO_OPENMP = "1"
$env:FALKORDB_SKIP_REDISEARCH = "1"
$env:CARGO_TARGET_DIR = Join-Path $WorkDir "cargo-target"
Remove-Item Env:FALKORDB_NATIVE_REDISEARCH_SHIM_DIR -ErrorAction SilentlyContinue

$HostManifest = Join-Path $PSScriptRoot "..\native_host\Cargo.toml"
$ClientSmoke = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\network_client_smoke.py"))

Write-Host "=== Fast network stage: build native RESP server ==="
cargo build --manifest-path $HostManifest --bin server 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "network_server_build.txt")
if ($LASTEXITCODE -ne 0) { throw "Network server build failed with exit code $LASTEXITCODE" }

$ServerExe = Join-Path $env:CARGO_TARGET_DIR "debug\server.exe"
if (-not (Test-Path $ServerExe)) {
    throw "Network server executable was not produced: $ServerExe"
}

$NetworkData = Join-Path $WorkDir "network-data-fast"
Remove-Item -Recurse -Force $NetworkData -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $NetworkData | Out-Null

function Wait-NativeServer([int]$Port) {
    for ($i = 0; $i -lt 100; $i++) {
        try {
            $tcp = [System.Net.Sockets.TcpClient]::new()
            $async = $tcp.ConnectAsync("127.0.0.1", $Port)
            if ($async.Wait(100) -and $tcp.Connected) {
                $tcp.Dispose()
                return
            }
            $tcp.Dispose()
        } catch {}
        Start-Sleep -Milliseconds 100
    }
    throw "Native RESP server did not become ready on port $Port"
}

function Start-NativeServer([string]$Suffix) {
    $out = Join-Path $Logs ("network_server_" + $Suffix + "_stdout.txt")
    $err = Join-Path $Logs ("network_server_" + $Suffix + "_stderr.txt")
    $proc = Start-Process -FilePath $ServerExe -ArgumentList @(
        "--bind", "127.0.0.1:6391",
        "--data-dir", $NetworkData,
        "--password", "native-ci-secret"
    ) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    Wait-NativeServer 6391
    return $proc
}

$Server = $null
try {
    $Server = Start-NativeServer "first"
    python $ClientSmoke write 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "network_official_client_write.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official FalkorDB client write/network phase failed with exit code $LASTEXITCODE"
    }

    Stop-Process -Id $Server.Id -Force
    Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    $Server = $null
    Start-Sleep -Milliseconds 300

    $Server = Start-NativeServer "restart"
    python $ClientSmoke read 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "network_official_client_restart.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official FalkorDB client restart/network phase failed with exit code $LASTEXITCODE"
    }
} finally {
    if ($null -ne $Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    }
}

Write-Host "NATIVE_WINDOWS_NETWORK_FALKORDB_CLIENT_PASS"
