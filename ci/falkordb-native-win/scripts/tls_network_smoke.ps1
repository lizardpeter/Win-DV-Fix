param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Stop"
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Logs = Join-Path $WorkDir "logs"
New-Item -ItemType Directory -Force -Path $Logs | Out-Null

$LibDirFile = Join-Path $WorkDir "native-lib-dir.txt"
if (-not (Test-Path $LibDirFile)) { throw "native-lib-dir.txt missing" }
$NativeLibDir = (Get-Content $LibDirFile -Raw).Trim()

$env:GRAPHBLAS_LIB_DIR = $NativeLibDir
$env:LAGRAPH_LIB_DIR = $NativeLibDir
$env:FALKORDB_NATIVE_NO_OPENMP = "1"
$env:FALKORDB_SKIP_REDISEARCH = "1"
$env:CARGO_TARGET_DIR = Join-Path $WorkDir "cargo-target"

$HostManifest = Join-Path $PSScriptRoot "..\native_host\Cargo.toml"
$CertGen = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\generate_tls_cert.py"))
$ClientSmoke = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\network_tls_smoke.py"))
$TlsDir = Join-Path $WorkDir "tls"
$NetworkData = Join-Path $WorkDir "network-data-tls"

Remove-Item -Recurse -Force $TlsDir,$NetworkData -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $TlsDir,$NetworkData | Out-Null
python $CertGen $TlsDir
if ($LASTEXITCODE -ne 0) { throw "TLS certificate generation failed" }

cargo build --manifest-path $HostManifest --bin server --release 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "tls_server_build.txt")
if ($LASTEXITCODE -ne 0) { throw "TLS server build failed" }

$ServerExe = Join-Path $env:CARGO_TARGET_DIR "release\server.exe"
$Cert = Join-Path $TlsDir "cert.pem"
$Key = Join-Path $TlsDir "key.pem"

function Wait-Port([int]$Port) {
    for ($i=0; $i -lt 100; $i++) {
        try {
            $tcp=[System.Net.Sockets.TcpClient]::new()
            $async=$tcp.ConnectAsync("127.0.0.1",$Port)
            if ($async.Wait(100) -and $tcp.Connected) {
                $tcp.Dispose()
                return
            }
            $tcp.Dispose()
        } catch {}
        Start-Sleep -Milliseconds 100
    }
    throw "TLS server did not bind port $Port"
}

function Start-TlsServer([string]$Suffix) {
    $out=Join-Path $Logs ("tls_server_"+$Suffix+"_stdout.txt")
    $err=Join-Path $Logs ("tls_server_"+$Suffix+"_stderr.txt")
    $p=Start-Process -FilePath $ServerExe -ArgumentList @(
      "--bind","127.0.0.1:6392",
      "--data-dir",$NetworkData,
      "--password","native-tls-ci-secret",
      "--tls-cert",$Cert,
      "--tls-key",$Key
    ) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    Wait-Port 6392
    return $p
}

$Server=$null
try {
    $Server=Start-TlsServer "first"
    python $ClientSmoke write $Cert 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "tls_official_client_write.txt")
    if ($LASTEXITCODE -ne 0) { throw "Official FalkorDB TLS write phase failed" }

    Stop-Process -Id $Server.Id -Force
    Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    $Server=$null
    Start-Sleep -Milliseconds 300

    $Server=Start-TlsServer "restart"
    python $ClientSmoke read $Cert 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "tls_official_client_restart.txt")
    if ($LASTEXITCODE -ne 0) { throw "Official FalkorDB TLS restart phase failed" }
} finally {
    if ($null -ne $Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    }
}

Write-Host "NATIVE_WINDOWS_TLS_FALKORDB_CLIENT_PASS"
