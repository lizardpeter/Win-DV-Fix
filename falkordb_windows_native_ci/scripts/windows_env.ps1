function Get-VsInstallPath {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        throw "vswhere.exe not found. Install Visual Studio Build Tools with Desktop development with C++."
    }

    $vs = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $vs) {
        throw "Visual Studio C++ x64 build tools were not found."
    }
    return $vs.Trim()
}

function Import-VsDevEnvironment {
    $vs = Get-VsInstallPath
    $devShell = Join-Path $vs "Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
    if (-not (Test-Path $devShell)) {
        throw "Visual Studio DevShell module not found: $devShell"
    }
    Import-Module $devShell -Force
    Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation `
        -DevCmdArguments '-arch=x64 -host_arch=x64' | Out-Null
    return $vs
}

function Get-LlvmBin {
    $clang = Get-Command clang.exe -ErrorAction SilentlyContinue
    if ($clang) {
        return Split-Path $clang.Source -Parent
    }

    $candidates = @(
        "C:\Program Files\LLVM\bin",
        (Join-Path (Get-VsInstallPath) "VC\Tools\Llvm\x64\bin")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path (Join-Path $candidate "clang.exe")) {
            return $candidate
        }
    }
    throw "LLVM/Clang was not found. Install LLVM (clang + libclang)."
}

function Get-NativeLibDir([string]$Prefix) {
    foreach ($candidate in @((Join-Path $Prefix "lib"), (Join-Path $Prefix "lib64"))) {
        if (Test-Path $candidate) {
            return $candidate
        }
    }
    throw "No installed native library directory found under $Prefix"
}

function Get-CMakeGeneratorArgs {
    if (Get-Command ninja.exe -ErrorAction SilentlyContinue) {
        return @("-G", "Ninja")
    }
    if (Get-Command nmake.exe -ErrorAction SilentlyContinue) {
        return @("-G", "NMake Makefiles")
    }
    throw "Neither Ninja nor NMake was found after entering the Visual Studio C++ developer shell."
}
