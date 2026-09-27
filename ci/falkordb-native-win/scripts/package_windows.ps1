param(
    [string]$WorkDir = "$PSScriptRoot\..\build",
    [string]$OutputDir = "$PSScriptRoot\..\build\package"
)

$ErrorActionPreference = "Stop"
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
$Root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$ReleaseDir = Join-Path $WorkDir "cargo-target\release"
$ServerExe = Join-Path $ReleaseDir "server.exe"

if (-not (Test-Path $ServerExe)) {
    throw "Native server executable not found: $ServerExe"
}

$PackageName = "falkordb-native-windows-x64"
$Stage = Join-Path $OutputDir $PackageName
$Zip = Join-Path $OutputDir ($PackageName + ".zip")

Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
Remove-Item -Force $Zip -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

Copy-Item -Force $ServerExe (Join-Path $Stage "server.exe")
Copy-Item -Force (Join-Path $Root "scripts\migrate_current_falkordb.py") (Join-Path $Stage "migrate_current_falkordb.py")
Copy-Item -Force (Join-Path $Root "scripts\falkordb_bundle.py") (Join-Path $Stage "falkordb_bundle.py")
Copy-Item -Force (Join-Path $Root "scripts\import_falkordb_rdb.py") (Join-Path $Stage "import_falkordb_rdb.py")
Copy-Item -Force (Join-Path $Root "requirements-migration.txt") (Join-Path $Stage "requirements-migration.txt")
Copy-Item -Force (Join-Path $Root "DEPLOYMENT.md") (Join-Path $Stage "DEPLOYMENT.md")
Copy-Item -Force (Join-Path $Root "README.md") (Join-Path $Stage "README.md")
Copy-Item -Force (Join-Path $Root "PORT_STATUS.md") (Join-Path $Stage "PORT_STATUS.md")

# The official OpenAI tunnel runtime is embedded directly inside server.exe.
$EmbeddedTunnelVersion = & (Join-Path $Stage "server.exe") --embedded-tunnel-runtime-version 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Embedded OpenAI tunnel runtime version check failed with exit code $LASTEXITCODE"
}
if (($EmbeddedTunnelVersion -join "`n") -notmatch "0\.0\.15") {
    throw "server.exe did not report the pinned embedded OpenAI tunnel runtime v0.0.15"
}
$EmbeddedTunnelVersion | Set-Content -Encoding UTF8 (Join-Path $Stage "EMBEDDED_TUNNEL_RUNTIME.txt")

$EmbeddedLicenses = & (Join-Path $Stage "server.exe") --third-party-licenses 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Embedded OpenAI tunnel license display failed with exit code $LASTEXITCODE"
}
$EmbeddedLicenseText = $EmbeddedLicenses -join "`n"
if ($EmbeddedLicenseText -notmatch "Copyright 2026 OpenAI" -or $EmbeddedLicenseText -notmatch "Apache License") {
    throw "server.exe did not expose the embedded OpenAI NOTICE/license"
}

# Bundle an official embeddable Python runtime plus all migration dependencies
# so helper tools do not use a machine-wide Python installation.
$PythonVersion = "3.12.10"
$PythonDir = Join-Path $Stage "python"
$PythonEmbedZip = Join-Path $Stage "tmp\python-$PythonVersion-embed-amd64.zip"
$PythonUrl = "https://www.python.org/ftp/python/$PythonVersion/python-$PythonVersion-embed-amd64.zip"
$SitePackages = Join-Path $PythonDir "Lib\site-packages"

New-Item -ItemType Directory -Force -Path (Split-Path $PythonEmbedZip -Parent) | Out-Null
New-Item -ItemType Directory -Force -Path $PythonDir | Out-Null
Invoke-WebRequest -UseBasicParsing -Uri $PythonUrl -OutFile $PythonEmbedZip
Expand-Archive -Path $PythonEmbedZip -DestinationPath $PythonDir -Force
Remove-Item -Force $PythonEmbedZip

$Pth = Join-Path $PythonDir "python312._pth"
@'
python312.zip
.
..
Lib\site-packages
import site
'@ | Set-Content -Encoding ASCII $Pth

New-Item -ItemType Directory -Force -Path $SitePackages | Out-Null
$EmbeddedPython = Join-Path $PythonDir "python.exe"
if (-not (Test-Path $EmbeddedPython)) {
    throw "Embedded Python runtime was not produced: $EmbeddedPython"
}

# Resolve migration dependencies explicitly for the embedded CPython 3.12
# Windows x64 ABI at build time. The final package does not need pip.
$OldPipCache = $env:PIP_CACHE_DIR
$env:PIP_CACHE_DIR = Join-Path $Stage "tmp\pip-cache"
try {
    python -m pip install --disable-pip-version-check --no-compile `
        --target $SitePackages `
        --platform win_amd64 `
        --python-version 3.12 `
        --implementation cp `
        --abi cp312 `
        --abi abi3 `
        --abi none `
        --only-binary=:all: `
        -r (Join-Path $Root "requirements-migration.txt")
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to populate embedded migration Python runtime"
    }
} finally {
    if ($null -eq $OldPipCache) {
        Remove-Item Env:PIP_CACHE_DIR -ErrorAction SilentlyContinue
    } else {
        $env:PIP_CACHE_DIR = $OldPipCache
    }
    Remove-Item -Recurse -Force (Join-Path $Stage "tmp\pip-cache") -ErrorAction SilentlyContinue
}

& $EmbeddedPython -c "import redis, falkordb; print('NATIVE_EMBEDDED_MIGRATION_PYTHON_PASS')"
if ($LASTEXITCODE -ne 0) {
    throw "Embedded migration Python failed to import packaged dependencies"
}
& $EmbeddedPython (Join-Path $Stage "import_falkordb_rdb.py") --help | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Embedded migration Python failed to execute packaged importer"
}

@'
param()

$ErrorActionPreference = "Stop"
$Root = [IO.Path]::GetFullPath($PSScriptRoot)
Set-Location $Root

foreach ($Name in @("data", "logs", "tls", "tmp", "pycache", "imports", "exports", "migration", "cache", "config", "profile", "profile\AppData\Roaming", "profile\AppData\Local")) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root $Name) | Out-Null
}

$env:FALKORDB_PORTABLE = "1"
$env:FALKORDB_PORTABLE_ROOT = $Root
$env:FALKORDB_DATA_DIR = "data"
$env:TEMP = Join-Path $Root "tmp"
$env:TMP = Join-Path $Root "tmp"
$env:PYTHONPYCACHEPREFIX = Join-Path $Root "pycache"
$env:PYTHONDONTWRITEBYTECODE = "1"
$env:PYTHONNOUSERSITE = "1"
$env:PYTHONPATH = ""
$env:HOME = Join-Path $Root "profile"
$env:USERPROFILE = Join-Path $Root "profile"
$env:APPDATA = Join-Path $Root "profile\AppData\Roaming"
$env:LOCALAPPDATA = Join-Path $Root "profile\AppData\Local"
$env:XDG_CACHE_HOME = Join-Path $Root "cache"
$env:XDG_CONFIG_HOME = Join-Path $Root "config"
$env:PIP_CACHE_DIR = Join-Path $Root "cache\pip"
'@ | Set-Content -Encoding UTF8 (Join-Path $Stage "portable-env.ps1")

@'
param(
    [string]$Bind = "127.0.0.1:6379"
)

$ErrorActionPreference = "Stop"
. "$PSScriptRoot\portable-env.ps1"

if (-not $env:FALKORDB_PASSWORD) {
    throw "Set FALKORDB_PASSWORD before starting the server."
}

& "$PSScriptRoot\server.exe" --portable --bind $Bind
exit $LASTEXITCODE
'@ | Set-Content -Encoding UTF8 (Join-Path $Stage "start-local.ps1")

@'
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("migrate-live", "bundle", "import-rdb")]
    [string]$Tool,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Arguments
)

$ErrorActionPreference = "Stop"
. "$PSScriptRoot\portable-env.ps1"

$Python = Join-Path $PSScriptRoot "python\python.exe"
if (-not (Test-Path $Python)) {
    throw "Bundled migration Python runtime is missing: $Python"
}

$Script = switch ($Tool) {
    "migrate-live" { "migrate_current_falkordb.py" }
    "bundle"       { "falkordb_bundle.py" }
    "import-rdb"   { "import_falkordb_rdb.py" }
}

& $Python (Join-Path $PSScriptRoot $Script) @Arguments
exit $LASTEXITCODE
'@ | Set-Content -Encoding UTF8 (Join-Path $Stage "run-tool.ps1")

@'
param()

$ErrorActionPreference = "Stop"
$Root = [IO.Path]::GetFullPath($PSScriptRoot)
Set-Location $Root

$RequiredFiles = @(
    "server.exe",
    "EMBEDDED_TUNNEL_RUNTIME.txt",
    "portable-env.ps1",
    "start-local.ps1",
    "run-tool.ps1",
    "import_falkordb_rdb.py",
    "migrate_current_falkordb.py",
    "falkordb_bundle.py",
    "python\python.exe",
    "DEPENDENCIES.txt"
)
foreach ($Relative in $RequiredFiles) {
    if (-not (Test-Path (Join-Path $Root $Relative))) {
        throw "Portable package is missing required file: $Relative"
    }
}

. "$Root\portable-env.ps1"
$RequiredDirs = @(
    "data", "logs", "tls", "tmp", "pycache", "imports", "exports",
    "migration", "cache", "config", "profile",
    "profile\AppData\Roaming", "profile\AppData\Local"
)
foreach ($Relative in $RequiredDirs) {
    if (-not (Test-Path (Join-Path $Root $Relative))) {
        throw "Portable package is missing required local directory: $Relative"
    }
}

$Deps = Get-Content (Join-Path $Root "DEPENDENCIES.txt") -Raw
if ($Deps -match "(?i)(VCRUNTIME|MSVCP)[^\s]*\.dll") {
    throw "server.exe depends on an external MSVC runtime"
}

$Manifest = Join-Path $Root "SHA256SUMS.txt"
if (Test-Path $Manifest) {
    foreach ($Line in Get-Content $Manifest) {
        if ([string]::IsNullOrWhiteSpace($Line)) { continue }
        $Parts = $Line -split "\s{2}", 2
        if ($Parts.Count -ne 2) { throw "Malformed SHA256SUMS entry: $Line" }
        $File = Join-Path $Root $Parts[1]
        if (-not (Test-Path $File)) { throw "Manifest file missing: $($Parts[1])" }
        $Actual = (Get-FileHash -Algorithm SHA256 $File).Hash.ToLowerInvariant()
        if ($Actual -ne $Parts[0].ToLowerInvariant()) {
            throw "SHA256 mismatch for $($Parts[1])"
        }
    }
}

& "$Root\python\python.exe" -c "import redis, falkordb"
if ($LASTEXITCODE -ne 0) {
    throw "Bundled migration Python runtime is not self-contained"
}

& "$Root\run-tool.ps1" import-rdb --help | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Portable run-tool wrapper failed to forward helper arguments"
}

$OutsideFile = Join-Path $env:WINDIR "win.ini"
if (Test-Path $OutsideFile) {
    $ToolEscape = & "$Root\python\python.exe" "$Root\import_falkordb_rdb.py" inspect --rdb $OutsideFile 2>&1
    if ($LASTEXITCODE -eq 0 -or ($ToolEscape -join "`n") -notmatch "outside portable package root") {
        throw "Portable migration helper accepted a file outside the package"
    }
}

$Escape = & "$Root\server.exe" --portable --data-dir "..\escape" 2>&1
if ($LASTEXITCODE -eq 0 -or ($Escape -join "`n") -notmatch "may not escape") {
    throw "Portable parent-directory confinement check failed"
}

# Negative confinement checks intentionally execute children that fail.
# Clear their native exit status so callers see the verifier's actual success.
$global:LASTEXITCODE = 0
Write-Host "PORTABLE_FOLDER_VERIFY_PASS"
'@ | Set-Content -Encoding UTF8 (Join-Path $Stage "verify-portable.ps1")

# Create the complete portable folder layout at package time as well as on launch.
& (Join-Path $Stage "portable-env.ps1")
foreach ($Name in @("data", "logs", "tls", "tmp", "pycache", "imports", "exports", "migration", "cache", "config", "profile", "profile\AppData\Roaming", "profile\AppData\Local")) {
    if (-not (Test-Path (Join-Path $Stage $Name))) {
        throw "Portable package directory missing: $Name"
    }
}

# Portable mode must reject any runtime path that could escape the package.
$EscapeOutput = & (Join-Path $Stage "server.exe") --portable --data-dir "..\escape" 2>&1
if ($LASTEXITCODE -eq 0) {
    throw "Portable server unexpectedly accepted parent-directory data path"
}
if (($EscapeOutput -join "`n") -notmatch "may not escape") {
    throw "Portable escape-path rejection did not emit the expected diagnostic"
}

$AbsoluteEscape = Join-Path ([IO.Path]::GetTempPath()) "falkordb-portable-escape"
$AbsoluteOutput = & (Join-Path $Stage "server.exe") --portable --data-dir $AbsoluteEscape 2>&1
if ($LASTEXITCODE -eq 0) {
    throw "Portable server unexpectedly accepted absolute external data path"
}
if (($AbsoluteOutput -join "`n") -notmatch "must be relative") {
    throw "Portable absolute-path rejection did not emit the expected diagnostic"
}

# Launch from outside the package and prove relative storage is still anchored
# beside server.exe rather than to the caller's working directory.
$RuntimeSmokeDirName = "data-runtime-smoke"
$RuntimeSmokeDir = Join-Path $Stage $RuntimeSmokeDirName
$ExternalLeakDir = Join-Path $OutputDir $RuntimeSmokeDirName
Remove-Item -Recurse -Force $RuntimeSmokeDir -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $ExternalLeakDir -ErrorAction SilentlyContinue
$RuntimeStdout = Join-Path $Stage "logs\portable-runtime-smoke-stdout.txt"
$RuntimeStderr = Join-Path $Stage "logs\portable-runtime-smoke-stderr.txt"
$PortableProc = Start-Process -FilePath (Join-Path $Stage "server.exe") `
    -ArgumentList @("--portable", "--data-dir", $RuntimeSmokeDirName, "--port", "0") `
    -WorkingDirectory $OutputDir `
    -RedirectStandardOutput $RuntimeStdout `
    -RedirectStandardError $RuntimeStderr `
    -PassThru
try {
    Start-Sleep -Milliseconds 750
    if ($PortableProc.HasExited) {
        $ErrText = if (Test-Path $RuntimeStderr) { Get-Content $RuntimeStderr -Raw } else { "" }
        throw "Portable runtime smoke exited unexpectedly: $ErrText"
    }
    if (-not (Test-Path $RuntimeSmokeDir)) {
        throw "Portable server did not create runtime data beside server.exe"
    }
    if (Test-Path $ExternalLeakDir) {
        throw "Portable server leaked runtime data into the caller working directory"
    }
} finally {
    if (-not $PortableProc.HasExited) {
        Stop-Process -Id $PortableProc.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $PortableProc.Id -ErrorAction SilentlyContinue
    }
}
Remove-Item -Recurse -Force $RuntimeSmokeDir -ErrorAction SilentlyContinue

# Verify the final PE does not depend on the separately installed MSVC
# runtime. Use our deterministic standard-library PE parser rather than
# link.exe /dump, whose exit status varies across Visual Studio runner images.
$PeScanner = Join-Path $PSScriptRoot "pe_dependencies.py"
if (-not (Test-Path $PeScanner)) {
    throw "PE dependency scanner not found: $PeScanner"
}
$Deps = & python $PeScanner (Join-Path $Stage "server.exe") 2>&1
$PeScanExit = $LASTEXITCODE
$Deps | Set-Content -Encoding UTF8 (Join-Path $Stage "DEPENDENCIES.txt")
$DepsText = $Deps -join "`n"
if ($PeScanExit -ne 0) {
    throw "PE dependency audit failed with exit code $PeScanExit"
}
if ($DepsText -match "(?i)(VCRUNTIME|MSVCP)[^\s]*\.dll") {
    throw "Packaged server still depends on the external MSVC runtime"
}

# Prove the packaged executable itself starts and parses its CLI before zipping.
$Help = & (Join-Path $Stage "server.exe") --help 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Packaged server --help failed with exit code $LASTEXITCODE"
}
if (($Help -join "`n") -notmatch "falkordb-native-server") {
    throw "Packaged server did not emit expected help banner"
}
$Help | Set-Content -Encoding UTF8 (Join-Path $Stage "SERVER_HELP.txt")

# verify-portable.ps1 throws on a real failure. It deliberately runs
# negative child-process tests that return nonzero, so neither $? nor
# $LASTEXITCODE is a valid summary of the script after those assertions.
& (Join-Path $Stage "verify-portable.ps1")

$Hash = (Get-FileHash -Algorithm SHA256 (Join-Path $Stage "server.exe")).Hash.ToLowerInvariant()

$MutableRoots = @(
    (Join-Path $Stage "data"),
    (Join-Path $Stage "logs"),
    (Join-Path $Stage "tmp"),
    (Join-Path $Stage "pycache"),
    (Join-Path $Stage "imports"),
    (Join-Path $Stage "exports"),
    (Join-Path $Stage "migration"),
    (Join-Path $Stage "cache"),
    (Join-Path $Stage "config"),
    (Join-Path $Stage "profile"),
    (Join-Path $Stage "tls")
)
$ManifestPath = Join-Path $Stage "SHA256SUMS.txt"
$ManifestLines = Get-ChildItem -Path $Stage -File -Recurse |
    Where-Object {
        $CurrentPath = $_.FullName
        $UnderMutableRoot = $MutableRoots | Where-Object {
            $CurrentPath.StartsWith($_ + [IO.Path]::DirectorySeparatorChar)
        }
        $CurrentPath -ne $ManifestPath -and -not $UnderMutableRoot
    } |
    Sort-Object FullName |
    ForEach-Object {
        $Relative = [IO.Path]::GetRelativePath($Stage, $_.FullName)
        $Digest = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLowerInvariant()
        "$Digest  $Relative"
    }
$ManifestLines | Set-Content -Encoding ASCII $ManifestPath

& (Join-Path $Stage "verify-portable.ps1")

Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $Zip -CompressionLevel Optimal

Write-Host "NATIVE_WINDOWS_PORTABLE_SELF_VERIFY_PASS"
Write-Host "NATIVE_WINDOWS_EMBEDDED_MIGRATION_RUNTIME_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_EXTERNAL_CWD_PASS"
Write-Host "NATIVE_WINDOWS_STATIC_RUNTIME_AUDIT_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_CONFINEMENT_PASS"
Write-Host "NATIVE_WINDOWS_PACKAGE_PASS"
Write-Host "Package directory: $Stage"
Write-Host "Package archive:   $Zip"
Write-Host "server.exe SHA256: $Hash"

# The package verification deliberately runs negative native-process tests.
# Ensure the GitHub/PowerShell host exits according to this script's success,
# not a stale child-process LASTEXITCODE.
$global:LASTEXITCODE = 0
