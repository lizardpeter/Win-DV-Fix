param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Continue"
. "$PSScriptRoot\windows_env.ps1"
Import-VsDevEnvironment | Out-Null

$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Falkor = Join-Path $WorkDir "src\FalkorDB"
$Prefix = Join-Path $WorkDir "native-prefix"
$Logs = Join-Path $WorkDir "logs"
New-Item -ItemType Directory -Force -Path $Logs | Out-Null

if (-not (Test-Path $Falkor)) {
    throw "FalkorDB source not found. Run bootstrap_windows.ps1 first."
}

$LibDir = Get-NativeLibDir $Prefix
$env:GRAPHBLAS_LIB_DIR = $LibDir
$env:LAGRAPH_LIB_DIR = $LibDir
$env:FALKORDB_SKIP_REDISEARCH = "1"
$env:OPENMP_LIB_NAME = "vcomp"

$RequiredStaticLibs = @(
    "graphblas_static.lib",
    "lagraph_static.lib",
    "lagraphx_static.lib"
)
foreach ($lib in $RequiredStaticLibs) {
    $candidate = Join-Path $LibDir $lib
    if (-not (Test-Path $candidate)) {
        Write-Error "Missing required static native library: $candidate"
        Get-ChildItem -Path $LibDir -Filter *.lib -ErrorAction SilentlyContinue |
            ForEach-Object { Write-Host "  $($_.Name)" }
        exit 9
    }
}

rustc -Vv | Tee-Object -FilePath (Join-Path $Logs "rustc.txt")
cargo -V | Tee-Object -FilePath (Join-Path $Logs "cargo.txt")
cmake --version | Tee-Object -FilePath (Join-Path $Logs "cmake.txt")
cl 2>&1 | Select-Object -First 4 | Tee-Object -FilePath (Join-Path $Logs "cl.txt")

$Bindings = Join-Path $Falkor "graph\src\graph\graphblas\mod.rs"
Write-Host "=== Stage 0: Windows binding sanity ==="
$Darwin = Select-String -Path $Bindings -Pattern '__darwin_|__sFILE'
if ($Darwin) {
    $Darwin | Tee-Object -FilePath (Join-Path $Logs "00_binding_sanity.txt")
    Write-Error "Darwin/macOS CRT tokens remain in GraphBLAS bindings"
    exit 10
}
"PASS: no Darwin CRT tokens" | Tee-Object -FilePath (Join-Path $Logs "00_binding_sanity.txt")

Write-Host "=== Stage 1: graph crate type-check ==="
Push-Location $Falkor
cargo check -p graph 2>&1 | Tee-Object -FilePath (Join-Path $Logs "01_graph_check.txt")
$GraphStatus = $LASTEXITCODE
Pop-Location
if ($GraphStatus -ne 0) { exit $GraphStatus }

Write-Host "=== Stage 2: native host type-check ==="
$Host = Join-Path $PSScriptRoot "..\native_host\Cargo.toml"
cargo check --manifest-path $Host 2>&1 | Tee-Object -FilePath (Join-Path $Logs "02_host_check.txt")
$HostStatus = $LASTEXITCODE
if ($HostStatus -ne 0) { exit $HostStatus }

Write-Host "=== Stage 3: native host release link ==="
cargo build --manifest-path $Host --release 2>&1 | Tee-Object -FilePath (Join-Path $Logs "03_host_link.txt")
$LinkStatus = $LASTEXITCODE
if ($LinkStatus -ne 0) { exit $LinkStatus }

Write-Host "ALL_COMPILE_STAGES_OK"
Write-Host "Logs: $Logs"
