param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Stop"
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Falkor = Join-Path $WorkDir "src\FalkorDB"
$Prefix = Join-Path $WorkDir "native-prefix"
$ShimDir = Join-Path $WorkDir "redisearch-shim"
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
$env:FALKORDB_NATIVE_REDISEARCH_SHIM_DIR = $ShimDir
Remove-Item Env:FALKORDB_SKIP_REDISEARCH -ErrorAction SilentlyContinue

rustc -Vv | Tee-Object -FilePath (Join-Path $Logs "00_rustc.txt")
cargo -V | Tee-Object -FilePath (Join-Path $Logs "00_cargo.txt")
cmake --version | Tee-Object -FilePath (Join-Path $Logs "00_cmake.txt")
"Native library directory: $NativeLibDir" |
    Tee-Object -FilePath (Join-Path $Logs "00_native_lib_dir.txt")

Write-Host "=== Stage 1: graph crate type-check ==="
Push-Location $Falkor
cargo check -p graph 2>&1 | Tee-Object -FilePath (Join-Path $Logs "01_graph_check.txt")
Pop-Location

Write-Host "=== Stage 2: native host type-check ==="
$Host = Join-Path $PSScriptRoot "..\native_host\Cargo.toml"
cargo check --manifest-path $Host 2>&1 | Tee-Object -FilePath (Join-Path $Logs "02_host_check.txt")

Write-Host "=== Stage 3: native smoke executable link ==="
cargo build --manifest-path $Host --bin smoke --release 2>&1 |
    Tee-Object -FilePath (Join-Path $Logs "03_smoke_build.txt")

Write-Host "=== Stage 4: native smoke execution ==="
$SmokeExe = Join-Path $PSScriptRoot "..\native_host\target\release\smoke.exe"
if (-not (Test-Path $SmokeExe)) {
    throw "Smoke executable was not produced: $SmokeExe"
}
$SmokeOutput = & $SmokeExe 2>&1 | Tee-Object -FilePath (Join-Path $Logs "04_smoke_run.txt")
if ($LASTEXITCODE -ne 0) {
    throw "Native smoke executable failed with exit code $LASTEXITCODE"
}
if (($SmokeOutput -join "`n") -notmatch "NATIVE_SMOKE_OK") {
    throw "Native smoke executable did not emit NATIVE_SMOKE_OK"
}

Write-Host ""
Write-Host "NATIVE_WINDOWS_CORE_SMOKE_PASS"
Write-Host "Logs: $Logs"
