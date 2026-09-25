param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Continue"
. "$PSScriptRoot\windows_env.ps1"
Import-VsDevEnvironment | Out-Null

$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Prefix = Join-Path $WorkDir "native-prefix"
$Logs = Join-Path $WorkDir "logs"
New-Item -ItemType Directory -Force -Path $Logs | Out-Null

$LibDir = Get-NativeLibDir $Prefix
$env:GRAPHBLAS_LIB_DIR = $LibDir
$env:LAGRAPH_LIB_DIR = $LibDir
$env:FALKORDB_SKIP_REDISEARCH = "1"
$env:OPENMP_LIB_NAME = "vcomp"

$Host = Join-Path $PSScriptRoot "..\native_host\Cargo.toml"
$SmokeLog = Join-Path $Logs "04_smoke.txt"

cargo run --manifest-path $Host --release -- --smoke 2>&1 |
    Tee-Object -FilePath $SmokeLog
$Status = $LASTEXITCODE
if ($Status -ne 0) { exit $Status }

if (-not (Select-String -Path $SmokeLog -Pattern '^NATIVE_SMOKE_OK$' -Quiet)) {
    Write-Error "Native host exited successfully but did not emit NATIVE_SMOKE_OK"
    exit 20
}

Write-Host "NATIVE_WINDOWS_SMOKE_PASS"
