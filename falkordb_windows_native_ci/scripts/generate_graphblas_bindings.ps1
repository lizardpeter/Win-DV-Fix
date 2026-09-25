param(
    [Parameter(Mandatory=$true)][string]$FalkorRoot,
    [Parameter(Mandatory=$true)][string]$Prefix
)

$ErrorActionPreference = "Stop"
. "$PSScriptRoot\windows_env.ps1"
Import-VsDevEnvironment | Out-Null

if (-not (Get-Command bindgen.exe -ErrorAction SilentlyContinue)) {
    Write-Host "Installing bindgen-cli 0.72.1..."
    cargo install bindgen-cli --version 0.72.1
}

$LlvmBin = Get-LlvmBin
$env:LIBCLANG_PATH = $LlvmBin
if ($env:PATH -notlike "*$LlvmBin*") {
    $env:PATH = "$LlvmBin;$env:PATH"
}

$candidates = @(
    (Join-Path $Prefix "include\suitesparse\GraphBLAS.h"),
    (Join-Path $Prefix "include\GraphBLAS.h"),
    (Join-Path $Prefix "include\SuiteSparse\GraphBLAS.h")
)
$Header = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Header) {
    throw "GraphBLAS.h was not found under $Prefix"
}

$Out = Join-Path $env:TEMP "falkordb_graphblas_windows.rs"
$BindgenArgs = @(
    $Header,
    "--default-enum-style=rust",
    "--opaque-type=GB_Iterator_opaque",
    "--blocklist-type=^FILE$",
    "--raw-line=pub enum FILE {}",
    "--allowlist-type=^(GrB|GxB|GB)_.*",
    "--allowlist-function=^(GrB|GxB|GB)_.*",
    "--allowlist-var=^(GrB|GxB|GB)_.*",
    "-o", $Out,
    "--",
    "-target", "x86_64-pc-windows-msvc",
    "-DGB_STATIC",
    "-I$(Split-Path $Header -Parent)"
)

# Give libclang the same Windows SDK/MSVC include paths that cl.exe sees.
if ($env:INCLUDE) {
    foreach ($inc in ($env:INCLUDE -split ';')) {
        if ($inc -and (Test-Path $inc)) {
            $BindgenArgs += "-I$inc"
        }
    }
}

& bindgen.exe @BindgenArgs
if ($LASTEXITCODE -ne 0) {
    throw "bindgen failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path $Out) -or (Get-Item $Out).Length -lt 10000) {
    throw "Generated GraphBLAS binding file is unexpectedly small"
}

python "$PSScriptRoot\splice_graphblas_bindings.py" $FalkorRoot $Out

$Target = Join-Path $FalkorRoot "graph\src\graph\graphblas\mod.rs"
if (Select-String -Path $Target -Pattern '__darwin_|__sFILE' -Quiet) {
    throw "Windows binding regeneration still contains Darwin FILE/CRT definitions"
}

Write-Host "Windows GraphBLAS bindings regenerated and Darwin-token check passed."
