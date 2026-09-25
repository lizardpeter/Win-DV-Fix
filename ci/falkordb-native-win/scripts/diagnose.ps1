param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Stop"
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Falkor = Join-Path $WorkDir "src\FalkorDB"
$Prefix = Join-Path $WorkDir "native-prefix"
$Logs = Join-Path $WorkDir "logs"
New-Item -ItemType Directory -Force -Path $Logs | Out-Null

if (-not (Test-Path $Falkor)) {
    throw "FalkorDB source not found. Run bootstrap_windows.ps1 first."
}

$LibDirFile = Join-Path $WorkDir "native-lib-dir.txt"
if (Test-Path $LibDirFile) {
    $NativeLibDir = (Get-Content $LibDirFile -Raw).Trim()
} else {
    $Candidate = Get-ChildItem -Path $Prefix -Recurse -Filter graphblas.lib |
        Select-Object -First 1
    if (-not $Candidate) {
        throw "Could not locate graphblas.lib under $Prefix"
    }
    $NativeLibDir = $Candidate.Directory.FullName
}

$env:GRAPHBLAS_LIB_DIR = $NativeLibDir
$env:LAGRAPH_LIB_DIR = $NativeLibDir

$env:FALKORDB_NATIVE_NO_OPENMP = "1"
$env:CARGO_TARGET_DIR = Join-Path $WorkDir "cargo-target"
$env:FALKORDB_SKIP_REDISEARCH = "1"
Remove-Item Env:FALKORDB_NATIVE_REDISEARCH_SHIM_DIR -ErrorAction SilentlyContinue

rustc -Vv | Tee-Object -FilePath (Join-Path $Logs "00_rustc.txt")
cargo -V | Tee-Object -FilePath (Join-Path $Logs "00_cargo.txt")
cmake --version | Tee-Object -FilePath (Join-Path $Logs "00_cmake.txt")
"Native library directory: $NativeLibDir" |
    Tee-Object -FilePath (Join-Path $Logs "00_native_lib_dir.txt")

Write-Host "=== Stage 1: graph crate type-check ==="
Push-Location $Falkor
cargo check -p graph 2>&1 | Tee-Object -FilePath (Join-Path $Logs "01_graph_check.txt")
$GraphCheckExit = $LASTEXITCODE
Pop-Location
if ($GraphCheckExit -ne 0) { throw "FalkorDB graph cargo check failed with exit code $GraphCheckExit" }

Write-Host "=== Stage 2: native host type-check ==="
$HostManifest = Join-Path $PSScriptRoot "..\native_host\Cargo.toml"
cargo check --manifest-path $HostManifest 2>&1 | Tee-Object -FilePath (Join-Path $Logs "02_host_check.txt")
if ($LASTEXITCODE -ne 0) { throw "Native host cargo check failed with exit code $LASTEXITCODE" }

Write-Host "=== Stage 3: native host unit tests ==="
cargo test --manifest-path $HostManifest --lib 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "03_host_tests.txt")
if ($LASTEXITCODE -ne 0) { throw "Native host unit tests failed with exit code $LASTEXITCODE" }

Write-Host "=== Stage 4: native smoke executable link ==="
cargo build --manifest-path $HostManifest --bin smoke --release 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "04_smoke_build.txt")
if ($LASTEXITCODE -ne 0) { throw "Native smoke build failed with exit code $LASTEXITCODE" }

Write-Host "=== Stage 5: native smoke execution ==="
$SmokeExe = Join-Path $env:CARGO_TARGET_DIR "release\smoke.exe"
if (-not (Test-Path $SmokeExe)) {
    throw "Smoke executable was not produced: $SmokeExe"
}
$SmokeOutput = & $SmokeExe 2>&1 | Tee-Object -FilePath (Join-Path $Logs "05_smoke_run.txt")
if ($LASTEXITCODE -ne 0) {
    throw "Native smoke executable failed with exit code $LASTEXITCODE"
}
if (($SmokeOutput -join "`n") -notmatch "NATIVE_SMOKE_OK") {
    throw "Native smoke executable did not emit NATIVE_SMOKE_OK"
}

Write-Host "=== Stage 6: Cypher-visible native index + WAL restart smoke ==="
cargo build --manifest-path $HostManifest --bin indexed_smoke --release 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "06_indexed_smoke_build.txt")
if ($LASTEXITCODE -ne 0) { throw "Indexed smoke build failed with exit code $LASTEXITCODE" }

$IndexedSmokeExe = Join-Path $env:CARGO_TARGET_DIR "release\indexed_smoke.exe"
if (-not (Test-Path $IndexedSmokeExe)) {
    throw "Indexed smoke executable was not produced: $IndexedSmokeExe"
}
$IndexedOutput = & $IndexedSmokeExe 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "07_indexed_smoke_run.txt")
if ($LASTEXITCODE -ne 0) {
    throw "Indexed smoke executable failed with exit code $LASTEXITCODE"
}
$IndexedText = $IndexedOutput -join "`n"
if ($IndexedText -notmatch "NATIVE_CYPHER_INDEX_INTEGRATION_PASS") {
    throw "Indexed smoke did not emit NATIVE_CYPHER_INDEX_INTEGRATION_PASS"
}
if ($IndexedText -notmatch "NATIVE_WAL_INDEX_RESTART_PASS") {
    throw "Indexed smoke did not emit NATIVE_WAL_INDEX_RESTART_PASS"
}

Write-Host ""
Write-Host "NATIVE_WINDOWS_FULL_STANDALONE_PASS"
Write-Host "Logs: $Logs"


Write-Host "=== Stage 7: official FalkorDB client network compatibility ==="
cargo build --manifest-path $HostManifest --bin server --release 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "08_server_build.txt")
if ($LASTEXITCODE -ne 0) { throw "Network server build failed with exit code $LASTEXITCODE" }

$ServerExe = Join-Path $env:CARGO_TARGET_DIR "release\server.exe"
if (-not (Test-Path $ServerExe)) {
    throw "Network server executable was not produced: $ServerExe"
}

$NetworkData = Join-Path $WorkDir "network-data"
Remove-Item -Recurse -Force $NetworkData -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $NetworkData | Out-Null
$ClientSmoke = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\network_client_smoke.py"))
$ApiSmoke = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\chatgpt_api_smoke.py"))
$TlsGenerator = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\generate_tls_fixtures.py"))
$TlsDir = Join-Path $WorkDir "tls-fixtures"
Remove-Item -Recurse -Force $TlsDir -ErrorAction SilentlyContinue
python $TlsGenerator $TlsDir 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "09_tls_fixture_generation.txt")
if ($LASTEXITCODE -ne 0) {
    throw "TLS fixture generation failed with exit code $LASTEXITCODE"
}
$env:FALKORDB_TEST_TLS_DIR = $TlsDir

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
    $out = Join-Path $Logs ("09_server_" + $Suffix + "_stdout.txt")
    $err = Join-Path $Logs ("09_server_" + $Suffix + "_stderr.txt")
    $proc = Start-Process -FilePath $ServerExe -ArgumentList @(
        "--bind", "127.0.0.1:6391",
        "--data-dir", $NetworkData,
        "--password", "native-ci-secret",
        "--tls-cert", (Join-Path $TlsDir "server-cert.pem"),
        "--tls-key", (Join-Path $TlsDir "server-key.pem"),
        "--tls-client-ca", (Join-Path $TlsDir "ca.pem"),
        "--api-bind", "127.0.0.1:8443",
        "--api-token", "native-api-write-secret",
        "--api-read-token", "native-api-read-secret"
    ) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    Wait-NativeServer 6391
    Wait-NativeServer 8443
    return $proc
}

$Server = $null
try {
    $Server = Start-NativeServer "first"
    python $ClientSmoke write 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "10_official_client_write.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official FalkorDB client write/network phase failed with exit code $LASTEXITCODE"
    }
    $WriteText = (Get-Content (Join-Path $Logs "10_official_client_write.txt") -Raw)
    if ($WriteText -notmatch "OFFICIAL_FALKORDB_CLIENT_MTLS_WRITE_PASS") {
        throw "Official FalkorDB client did not prove mTLS write connectivity"
    }

    python $ApiSmoke write 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "10_chatgpt_api_write.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "ChatGPT HTTPS API write phase failed with exit code $LASTEXITCODE"
    }
    $ApiWriteText = (Get-Content (Join-Path $Logs "10_chatgpt_api_write.txt") -Raw)
    if ($ApiWriteText -notmatch "CHATGPT_HTTPS_API_WRITE_PASS") {
        throw "ChatGPT HTTPS API did not prove authenticated write/read connectivity"
    }

    # Hard-stop the server to prove committed graph state is recoverable solely
    # from the native WAL on a fresh process.
    Stop-Process -Id $Server.Id -Force
    Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    $Server = $null
    Start-Sleep -Milliseconds 300

    $Server = Start-NativeServer "restart"
    python $ClientSmoke read 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "11_official_client_restart.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official FalkorDB client restart/network phase failed with exit code $LASTEXITCODE"
    }
    $RestartText = (Get-Content (Join-Path $Logs "11_official_client_restart.txt") -Raw)
    if ($RestartText -notmatch "OFFICIAL_FALKORDB_CLIENT_MTLS_RESTART_PASS") {
        throw "Official FalkorDB client did not prove mTLS restart connectivity"
    }

    python $ApiSmoke read 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "11_chatgpt_api_restart.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "ChatGPT HTTPS API restart phase failed with exit code $LASTEXITCODE"
    }
    $ApiRestartText = (Get-Content (Join-Path $Logs "11_chatgpt_api_restart.txt") -Raw)
    if ($ApiRestartText -notmatch "CHATGPT_HTTPS_API_RESTART_PASS") {
        throw "ChatGPT HTTPS API did not prove restart/WAL recovery"
    }
} finally {
    if ($null -ne $Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    }
}

Write-Host ""
Write-Host "NATIVE_WINDOWS_NETWORK_FALKORDB_CLIENT_PASS"
Write-Host "NATIVE_WINDOWS_MTLS_FALKORDB_CLIENT_PASS"
Write-Host "NATIVE_WINDOWS_CHATGPT_HTTPS_API_PASS"

