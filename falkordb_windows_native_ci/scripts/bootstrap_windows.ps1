param(
    [string]$WorkDir = "$PSScriptRoot\..\build",
    [switch]$SkipNativeDeps,
    [switch]$SkipBindings
)

$ErrorActionPreference = "Stop"
. "$PSScriptRoot\windows_env.ps1"

$Commit = "55204c94bb6c8bc1684ada3d712a61f73f324067"
$GraphBLASVersion = "v10.5.0"
$LAGraphVersion = "v1.3.x"
$LAGraphCommit = "a96fc4e055b7af6dde5e8a3f4dbb45b87d4bca28"

function Need([string]$Cmd) {
    if (-not (Get-Command $Cmd -ErrorAction SilentlyContinue)) {
        throw "Required command not found: $Cmd"
    }
}

Need git
Need cargo
Need rustc
Need cmake
Need python
$Vs = Import-VsDevEnvironment
$CMakeGeneratorArgs = Get-CMakeGeneratorArgs
Write-Host "Visual Studio: $Vs"
Write-Host "CMake generator: $($CMakeGeneratorArgs -join ' ')"

$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Src = Join-Path $WorkDir "src"
$Prefix = Join-Path $WorkDir "native-prefix"
New-Item -ItemType Directory -Force -Path $Src, $Prefix | Out-Null

$Falkor = Join-Path $Src "FalkorDB"
if (-not (Test-Path $Falkor)) {
    git clone --recurse-submodules https://github.com/FalkorDB/FalkorDB.git $Falkor
}
Push-Location $Falkor
git fetch origin
git checkout $Commit
git submodule update --init --recursive
Pop-Location

python "$PSScriptRoot\apply_windows_foundation.py" $Falkor
python "$PSScriptRoot\check_patch_expectations.py" $Falkor

if (-not $SkipNativeDeps) {
    # Make native dependency builds reproducible. Never let a previous CMake
    # generator or stale .lib file influence the next run.
    if (Test-Path $Prefix) { Remove-Item -Recurse -Force $Prefix }
    New-Item -ItemType Directory -Force -Path $Prefix | Out-Null

    $GB = Join-Path $Src "GraphBLAS"
    if (-not (Test-Path $GB)) {
        git clone --branch $GraphBLASVersion --single-branch --depth 1 `
            https://github.com/DrTimothyAldenDavis/GraphBLAS.git $GB
    }
    Push-Location $GB
    git fetch --depth 1 origin tag $GraphBLASVersion
    git checkout $GraphBLASVersion
    Pop-Location

    # GraphBLAS v10.4.1+ includes a Windows cl/OpenMP fix; use MSVC rather
    # than clang-cl for the native library build. v10.5.0 is pinned above.
    $GBBuild = Join-Path $WorkDir "graphblas-build-msvc"
    if (Test-Path $GBBuild) { Remove-Item -Recurse -Force $GBBuild }
    cmake -S $GB -B $GBBuild @CMakeGeneratorArgs `
        -DCMAKE_BUILD_TYPE=Release `
        -DSUITESPARSE_USE_FORTRAN=OFF `
        -DGRAPHBLAS_COMPACT=ON `
        -DGRAPHBLAS_USE_JIT=OFF `
        -DGRAPHBLAS_USE_OPENMP=ON `
        -DSUITESPARSE_DEMOS=OFF `
        -DBUILD_TESTING=OFF `
        -DBUILD_SHARED_LIBS=OFF `
        -DBUILD_STATIC_LIBS=ON `
        -DCMAKE_INSTALL_LIBDIR=lib `
        "-DCMAKE_INSTALL_PREFIX=$Prefix"
    cmake --build $GBBuild --parallel
    cmake --install $GBBuild --config Release

    $LA = Join-Path $Src "LAGraph"
    if (-not (Test-Path $LA)) {
        git clone --branch $LAGraphVersion --single-branch --depth 32 `
            https://github.com/GraphBLAS/LAGraph.git $LA
    }
    Push-Location $LA
    git fetch --depth 1 origin $LAGraphCommit
    git checkout $LAGraphCommit
    Pop-Location

    $LABuild = Join-Path $WorkDir "lagraph-build-msvc"
    if (Test-Path $LABuild) { Remove-Item -Recurse -Force $LABuild }
    cmake -S $LA -B $LABuild @CMakeGeneratorArgs `
        -DCMAKE_BUILD_TYPE=Release `
        -DSUITESPARSE_USE_FORTRAN=OFF `
        -DLAGRAPH_USE_OPENMP=OFF `
        -DSUITESPARSE_USE_OPENMP=OFF `
        -DBUILD_TESTING=OFF `
        -DBUILD_SHARED_LIBS=OFF `
        -DBUILD_STATIC_LIBS=ON `
        -DCMAKE_INSTALL_LIBDIR=lib `
        "-DCMAKE_PREFIX_PATH=$Prefix" `
        "-DCMAKE_INSTALL_PREFIX=$Prefix"
    cmake --build $LABuild --parallel
    cmake --install $LABuild --config Release

    $InstalledLibDir = Get-NativeLibDir $Prefix
    $RequiredStaticLibs = @(
        "graphblas_static.lib",
        "lagraph_static.lib",
        "lagraphx_static.lib"
    )
    foreach ($lib in $RequiredStaticLibs) {
        $candidate = Join-Path $InstalledLibDir $lib
        if (-not (Test-Path $candidate)) {
            Write-Host "Installed .lib files:"
            Get-ChildItem -Path $InstalledLibDir -Filter *.lib -ErrorAction SilentlyContinue |
                ForEach-Object { Write-Host "  $($_.Name)" }
            throw "Required native static library was not installed: $candidate"
        }
    }
}

if (-not $SkipBindings) {
    & "$PSScriptRoot\generate_graphblas_bindings.ps1" -FalkorRoot $Falkor -Prefix $Prefix
}

$LibDir = Get-NativeLibDir $Prefix
Write-Host ""
Write-Host "Prepared FalkorDB source: $Falkor"
Write-Host "Native prefix:          $Prefix"
Write-Host "Native libraries:       $LibDir"
Write-Host "Next: .\scripts\diagnose.ps1"
