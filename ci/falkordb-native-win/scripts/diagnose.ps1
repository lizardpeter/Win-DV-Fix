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
if ($IndexedText -notmatch "NATIVE_CHECKPOINT_WAL_ROTATION_PASS") {
    throw "Indexed smoke did not emit NATIVE_CHECKPOINT_WAL_ROTATION_PASS"
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
$ParitySmoke = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\falkordb_parity_smoke.py"))
$UpstreamFixtureTest = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tests\upstream_dump_fixture.py"))
$UpstreamFixture = Join-Path $WorkDir "upstream-fixture\upstream-real.dump"
$MigrationUtility = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "migrate_current_falkordb.py"))
$PortableBundleUtility = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "falkordb_portable_bundle.py"))
$PortableBundle = Join-Path $WorkDir "portable-migration.falkor.zip"
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

$MigrationData = Join-Path $WorkDir "migration-destination-data"
Remove-Item -Recurse -Force $MigrationData -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $MigrationData | Out-Null

function Start-MigrationDestination([string]$Suffix) {
    $out = Join-Path $Logs ("10_migration_dest_" + $Suffix + "_stdout.txt")
    $err = Join-Path $Logs ("10_migration_dest_" + $Suffix + "_stderr.txt")
    $proc = Start-Process -FilePath $ServerExe -ArgumentList @(
        "--bind", "127.0.0.1:6392",
        "--data-dir", $MigrationData,
        "--password", "native-ci-secret",
        "--tls-cert", (Join-Path $TlsDir "server-cert.pem"),
        "--tls-key", (Join-Path $TlsDir "server-key.pem"),
        "--tls-client-ca", (Join-Path $TlsDir "ca.pem")
    ) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    Wait-NativeServer 6392
    return $proc
}

$BundleData = Join-Path $WorkDir "bundle-destination-data"
Remove-Item -Recurse -Force $BundleData -ErrorAction SilentlyContinue
Remove-Item -Force $PortableBundle -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $BundleData | Out-Null

function Start-BundleDestination([string]$Suffix) {
    $out = Join-Path $Logs ("10_bundle_dest_" + $Suffix + "_stdout.txt")
    $err = Join-Path $Logs ("10_bundle_dest_" + $Suffix + "_stderr.txt")
    $proc = Start-Process -FilePath $ServerExe -ArgumentList @(
        "--bind", "127.0.0.1:6393",
        "--data-dir", $BundleData,
        "--password", "native-ci-secret",
        "--tls-cert", (Join-Path $TlsDir "server-cert.pem"),
        "--tls-key", (Join-Path $TlsDir "server-key.pem"),
        "--tls-client-ca", (Join-Path $TlsDir "ca.pem")
    ) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    Wait-NativeServer 6393
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

    if (-not (Test-Path $UpstreamFixture)) {
        throw "Official upstream FalkorDB DUMP artifact is missing: $UpstreamFixture"
    }
    python $UpstreamFixtureTest verify `
        --input $UpstreamFixture `
        --host localhost `
        --port 6391 `
        --password native-ci-secret `
        --ssl `
        --ca (Join-Path $TlsDir "ca.pem") `
        --cert (Join-Path $TlsDir "client-cert.pem") `
        --key (Join-Path $TlsDir "client-key.pem") 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "10_upstream_dump_restore.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official upstream FalkorDB DUMP restore verification failed with exit code $LASTEXITCODE"
    }
    $UpstreamRestoreText = (Get-Content (Join-Path $Logs "10_upstream_dump_restore.txt") -Raw)
    if ($UpstreamRestoreText -notmatch "UPSTREAM_FALKORDB_DUMP_RESTORE_PASS") {
        throw "Official upstream FalkorDB DUMP did not emit restore success marker"
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

    python $ParitySmoke 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "10_official_client_parity.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official FalkorDB parity gate failed with exit code $LASTEXITCODE"
    }
    $ParityText = (Get-Content (Join-Path $Logs "10_official_client_parity.txt") -Raw)
    if ($ParityText -notmatch "OFFICIAL_FALKORDB_PARITY_GATE_PASS") {
        throw "Official FalkorDB parity gate did not emit success marker"
    }

    # Prove the actual whole-database migration utility, not only the
    # server-side DUMP/RESTORE primitives. Use a second independent native
    # process and hard-restart it before the normal restart verifier.
    $MigrationServer = $null
    try {
        $MigrationServer = Start-MigrationDestination "first"
        $MigrationArgs = @(
            "--source-host", "localhost",
            "--source-port", "6391",
            "--source-password", "native-ci-secret",
            "--source-ssl",
            "--source-ca", (Join-Path $TlsDir "ca.pem"),
            "--source-cert", (Join-Path $TlsDir "client-cert.pem"),
            "--source-key", (Join-Path $TlsDir "client-key.pem"),
            "--destination-host", "localhost",
            "--destination-port", "6392",
            "--destination-password", "native-ci-secret",
            "--destination-ssl",
            "--destination-ca", (Join-Path $TlsDir "ca.pem"),
            "--destination-cert", (Join-Path $TlsDir "client-cert.pem"),
            "--destination-key", (Join-Path $TlsDir "client-key.pem")
        )

        python $MigrationUtility @MigrationArgs 2>&1 |
            Tee-Object -FilePath (Join-Path $Logs "10_migration_utility_first.txt")
        if ($LASTEXITCODE -ne 0) {
            throw "Whole-database migration utility failed with exit code $LASTEXITCODE"
        }
        $MigrationText = (Get-Content (Join-Path $Logs "10_migration_utility_first.txt") -Raw)
        if ($MigrationText -notmatch "MIGRATION_COMPLETE") {
            throw "Whole-database migration utility did not emit completion marker"
        }

        # Exercise replacement backup and verification against populated data.
        python $MigrationUtility @MigrationArgs --replace 2>&1 |
            Tee-Object -FilePath (Join-Path $Logs "10_migration_utility_replace.txt")
        if ($LASTEXITCODE -ne 0) {
            throw "Whole-database migration --replace failed with exit code $LASTEXITCODE"
        }

        Stop-Process -Id $MigrationServer.Id -Force
        Wait-Process -Id $MigrationServer.Id -ErrorAction SilentlyContinue
        $MigrationServer = $null
        Start-Sleep -Milliseconds 300

        $MigrationServer = Start-MigrationDestination "restart"
        $env:FALKORDB_TEST_PORT = "6392"
        python $ClientSmoke read 2>&1 |
            Tee-Object -FilePath (Join-Path $Logs "10_migration_utility_restart_verify.txt")
        if ($LASTEXITCODE -ne 0) {
            throw "Migrated destination restart verification failed with exit code $LASTEXITCODE"
        }
        $MigrationRestartText = (Get-Content (Join-Path $Logs "10_migration_utility_restart_verify.txt") -Raw)
        if ($MigrationRestartText -notmatch "NATIVE_REDIS_DUMP_RESTORE_RESTART_PASS") {
            throw "Migrated destination did not pass restart verification"
        }
        Write-Host "NATIVE_WHOLE_DATABASE_MIGRATION_PASS"
    } finally {
        Remove-Item Env:FALKORDB_TEST_PORT -ErrorAction SilentlyContinue
        if ($null -ne $MigrationServer -and -not $MigrationServer.HasExited) {
            Stop-Process -Id $MigrationServer.Id -Force -ErrorAction SilentlyContinue
            Wait-Process -Id $MigrationServer.Id -ErrorAction SilentlyContinue
        }
    }

    # Prove offline export/import: source and destination need not be online
    # together. Export the populated source to one archive, import it into a
    # third independent native process, then hard-restart that destination.
    $BundleServer = $null
    try {
        python $PortableBundleUtility export `
            --source-host localhost `
            --source-port 6391 `
            --source-password native-ci-secret `
            --source-ssl `
            --source-ca (Join-Path $TlsDir "ca.pem") `
            --source-cert (Join-Path $TlsDir "client-cert.pem") `
            --source-key (Join-Path $TlsDir "client-key.pem") `
            --bundle $PortableBundle 2>&1 |
            Tee-Object -FilePath (Join-Path $Logs "10_portable_bundle_export.txt")
        if ($LASTEXITCODE -ne 0) {
            throw "Portable FalkorDB bundle export failed with exit code $LASTEXITCODE"
        }
        $BundleExportText = (Get-Content (Join-Path $Logs "10_portable_bundle_export.txt") -Raw)
        if ($BundleExportText -notmatch "BUNDLE_EXPORT_COMPLETE") {
            throw "Portable FalkorDB bundle export did not emit completion marker"
        }

        $BundleServer = Start-BundleDestination "first"
        python $PortableBundleUtility import `
            --destination-host localhost `
            --destination-port 6393 `
            --destination-password native-ci-secret `
            --destination-ssl `
            --destination-ca (Join-Path $TlsDir "ca.pem") `
            --destination-cert (Join-Path $TlsDir "client-cert.pem") `
            --destination-key (Join-Path $TlsDir "client-key.pem") `
            --bundle $PortableBundle 2>&1 |
            Tee-Object -FilePath (Join-Path $Logs "10_portable_bundle_import.txt")
        if ($LASTEXITCODE -ne 0) {
            throw "Portable FalkorDB bundle import failed with exit code $LASTEXITCODE"
        }
        $BundleImportText = (Get-Content (Join-Path $Logs "10_portable_bundle_import.txt") -Raw)
        if ($BundleImportText -notmatch "BUNDLE_IMPORT_COMPLETE") {
            throw "Portable FalkorDB bundle import did not emit completion marker"
        }

        Stop-Process -Id $BundleServer.Id -Force
        Wait-Process -Id $BundleServer.Id -ErrorAction SilentlyContinue
        $BundleServer = $null
        Start-Sleep -Milliseconds 300

        $BundleServer = Start-BundleDestination "restart"
        $env:FALKORDB_TEST_PORT = "6393"
        python $ClientSmoke read 2>&1 |
            Tee-Object -FilePath (Join-Path $Logs "10_portable_bundle_restart_verify.txt")
        if ($LASTEXITCODE -ne 0) {
            throw "Portable bundle restart verification failed with exit code $LASTEXITCODE"
        }
        Write-Host "NATIVE_PORTABLE_DATABASE_BUNDLE_PASS"
    } finally {
        Remove-Item Env:FALKORDB_TEST_PORT -ErrorAction SilentlyContinue
        if ($null -ne $BundleServer -and -not $BundleServer.HasExited) {
            Stop-Process -Id $BundleServer.Id -Force -ErrorAction SilentlyContinue
            Wait-Process -Id $BundleServer.Id -ErrorAction SilentlyContinue
        }
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

    python $UpstreamFixtureTest verify `
        --input $UpstreamFixture `
        --skip-restore `
        --host localhost `
        --port 6391 `
        --password native-ci-secret `
        --ssl `
        --ca (Join-Path $TlsDir "ca.pem") `
        --cert (Join-Path $TlsDir "client-cert.pem") `
        --key (Join-Path $TlsDir "client-key.pem") 2>&1 |
        Tee-Object -FilePath (Join-Path $Logs "11_upstream_dump_restart.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Official upstream FalkorDB DUMP restart verification failed with exit code $LASTEXITCODE"
    }
    $UpstreamRestartText = (Get-Content (Join-Path $Logs "11_upstream_dump_restart.txt") -Raw)
    if ($UpstreamRestartText -notmatch "UPSTREAM_FALKORDB_DUMP_RESTART_PASS") {
        throw "Official upstream FalkorDB DUMP did not survive native restart"
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
Write-Host "NATIVE_WINDOWS_FALKORDB_PARITY_GATE_PASS"
Write-Host "NATIVE_WINDOWS_WHOLE_DATABASE_MIGRATION_PASS"
Write-Host "NATIVE_WINDOWS_UPSTREAM_FALKORDB_DUMP_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_DATABASE_BUNDLE_PASS"

