# Build and validate the native Windows FalkorDB single-EXE package locally.
# This never writes into or overwrites an existing installed server/data folder.
# Usage: .\scripts\build_local_release.ps1 [-FullValidation]
#
# Requires only the MSVC Rust toolchain, CMake and the Visual Studio C++ build
# tools: every native dependency is vendored and compiled by cargo.
[CmdletBinding()]
param(
    [switch]$FullValidation
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$Root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$Build = Join-Path $Root "build"
$Manifest = Join-Path $Root "native_host\Cargo.toml"
$PackageDir = Join-Path $Build "package"
$PackageExe = Join-Path $PackageDir "falkordb-native-windows-x64\server.exe"
$PackageZip = Join-Path $PackageDir "falkordb-native-windows-x64.zip"
$Package = Join-Path $PSScriptRoot "package_windows.ps1"
$Diagnose = Join-Path $PSScriptRoot "diagnose.ps1"

if ($PSVersionTable.PSEdition -eq "Core" -and -not $IsWindows) {
    throw "This builds a native Windows executable and must run on Windows."
}
foreach ($command in @("rustc", "cargo", "cmake")) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Missing $command. Install the Windows MSVC Rust toolchain, CMake and the Visual Studio C++ build tools."
    }
}
if (-not (Test-Path $Manifest)) {
    throw "Run this script from an intact custom-falkor-db source checkout."
}

Push-Location $Root
try {
    $env:CARGO_TARGET_DIR = Join-Path $Build "cargo-target"
    if ([string]::IsNullOrWhiteSpace($env:RUSTFLAGS)) {
        $env:RUSTFLAGS = "-C target-feature=+crt-static"
    } elseif (-not $env:RUSTFLAGS.Contains("target-feature=+crt-static")) {
        $env:RUSTFLAGS = "$($env:RUSTFLAGS) -C target-feature=+crt-static"
    }

    Write-Host "=== Compile and run native host unit tests ==="
    & cargo test --manifest-path $Manifest --lib --locked
    if ($LASTEXITCODE -ne 0) { throw "Rust unit tests failed ($LASTEXITCODE)." }

    if ($FullValidation) {
        Write-Host "=== Run comprehensive native smoke and persistence tests ==="
        & $Diagnose -WorkDir $Build
    }

    Write-Host "=== Build release server.exe ==="
    & cargo build --manifest-path $Manifest --bin server --release --locked
    if ($LASTEXITCODE -ne 0) { throw "Windows release compilation failed ($LASTEXITCODE)." }

    Write-Host "=== Perform portable runtime smoke tests and package ==="
    & $Package -WorkDir $Build -OutputDir $PackageDir

    if (-not (Test-Path $PackageExe) -or -not (Test-Path $PackageZip)) {
        throw "Windows package is incomplete. Missing executable or ZIP."
    }
    $hash = (Get-FileHash -Algorithm SHA256 $PackageExe).Hash.ToLowerInvariant()
    Write-Host ""
    Write-Host "WINDOWS_DIAGNOSTICS_BUILD_PASS"
    Write-Host "EXE: $PackageExe"
    Write-Host "ZIP: $PackageZip"
    Write-Host "server.exe SHA256: $hash"
    Write-Host "An installed server or database was not modified."
} finally {
    Pop-Location
}
