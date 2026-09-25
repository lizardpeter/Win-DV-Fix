param(
    [string]$WorkDir = "$PSScriptRoot\..\build",
    [switch]$SkipNativeDeps
)

$ErrorActionPreference = "Stop"
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

function Get-CMakeVSGenerator {
    $Help = (cmake --help | Out-String)
    foreach ($Generator in @("Visual Studio 18 2026", "Visual Studio 17 2022")) {
        if ($Help.Contains($Generator)) {
            return $Generator
        }
    }
    throw "No supported Visual Studio CMake generator found. Available generators:`n$Help"
}

$VSGenerator = Get-CMakeVSGenerator
Write-Host "Using CMake generator: $VSGenerator"

$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$Src = Join-Path $WorkDir "src"
$Prefix = Join-Path $WorkDir "native-prefix"
New-Item -ItemType Directory -Force -Path $Src, $Prefix | Out-Null

$Falkor = Join-Path $Src "FalkorDB"
if (-not (Test-Path $Falkor)) {
    git clone https://github.com/FalkorDB/FalkorDB.git $Falkor
}
Push-Location $Falkor
git fetch origin
git checkout $Commit
Pop-Location

python "$PSScriptRoot\apply_windows_foundation.py" $Falkor

if (-not $SkipNativeDeps) {
    $GB = Join-Path $Src "GraphBLAS"
    if (-not (Test-Path $GB)) {
        git clone --branch $GraphBLASVersion --single-branch --depth 1 `
            https://github.com/DrTimothyAldenDavis/GraphBLAS.git $GB
    }

    $GBBuild = Join-Path $WorkDir "graphblas-build"
    cmake -S $GB -B $GBBuild -G "$VSGenerator" -A x64 `
        -DCMAKE_C_FLAGS=/MP `
        -DCMAKE_INSTALL_PREFIX="$Prefix" `
        -DSUITESPARSE_USE_FORTRAN=OFF `
        -DBUILD_STATIC_LIBS=ON `
        -DBUILD_SHARED_LIBS=OFF `
        -DGRAPHBLAS_BUILD_STATIC_LIBS=ON `
        -DGRAPHBLAS_COMPACT=ON `
        -DGRAPHBLAS_USE_OPENMP=OFF `
        -DGRAPHBLAS_USE_JIT=OFF `
        -DBUILD_TESTING=OFF
    cmake --build $GBBuild --config Release --target GraphBLAS_static --parallel 4
    cmake --install $GBBuild --config Release

    $GBStatic = Get-ChildItem -Path $Prefix -Recurse -Filter graphblas_static.lib |
        Select-Object -First 1
    if (-not $GBStatic) {
        throw "Expected GraphBLAS static library graphblas_static.lib not found below $Prefix"
    }
    $LibDir = $GBStatic.Directory.FullName
    Copy-Item -Force $GBStatic.FullName (Join-Path $LibDir "graphblas.lib")

    $LA = Join-Path $Src "LAGraph"
    if (-not (Test-Path $LA)) {
        git clone --branch $LAGraphVersion --single-branch `
            https://github.com/GraphBLAS/LAGraph.git $LA
    }
    Push-Location $LA
    git fetch origin $LAGraphCommit --depth 1
    git checkout $LAGraphCommit
    Pop-Location

    $LABuild = Join-Path $WorkDir "lagraph-build"
    cmake -S $LA -B $LABuild -G "$VSGenerator" -A x64 `
        -DCMAKE_C_FLAGS=/MP `
        -DCMAKE_INSTALL_PREFIX="$Prefix" `
        -DBUILD_STATIC_LIBS=ON `
        -DBUILD_SHARED_LIBS=OFF `
        -DBUILD_TESTING=OFF `
        -DLAGRAPH_USE_OPENMP=OFF `
        -DNO_LIBM=ON `
        -DSUITESPARSE_USE_FORTRAN=OFF `
        -DGraphBLAS_DIR="$Prefix\lib\cmake\GraphBLAS" `
        -DCMAKE_PREFIX_PATH="$Prefix"
    cmake --build $LABuild --config Release --target LAGraph_static LAGraphX_static --parallel 4
    cmake --install $LABuild --config Release

    $LGStatic = Get-ChildItem -Path $Prefix -Recurse -Filter lagraph_static.lib |
        Select-Object -First 1
    $LGXStatic = Get-ChildItem -Path $Prefix -Recurse -Filter lagraphx_static.lib |
        Select-Object -First 1
    if (-not $LGStatic) {
        throw "Expected LAGraph static library lagraph_static.lib not found below $Prefix"
    }
    if (-not $LGXStatic) {
        throw "Expected LAGraphX static library lagraphx_static.lib not found below $Prefix"
    }
    Copy-Item -Force $LGStatic.FullName (Join-Path $LGStatic.Directory.FullName "lagraph.lib")
    Copy-Item -Force $LGXStatic.FullName (Join-Path $LGXStatic.Directory.FullName "lagraphx.lib")

    # Persist the actual install library directory for diagnose.ps1.
    $LibDir | Set-Content -Path (Join-Path $WorkDir "native-lib-dir.txt") -Encoding Ascii
}


Write-Host ""
Write-Host "Prepared source:       $Falkor"
Write-Host "Native prefix:         $Prefix"
Write-Host "Next: .\scripts\diagnose.ps1"
