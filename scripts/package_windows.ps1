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
$RuntimeStage = Join-Path $OutputDir $PackageName
$Zip = Join-Path $OutputDir ($PackageName + ".zip")

$ToolsPackageName = "falkordb-native-windows-x64-tools"
$Stage = Join-Path $OutputDir $ToolsPackageName
$ToolsZip = Join-Path $OutputDir ($ToolsPackageName + ".zip")

Remove-Item -Recurse -Force $RuntimeStage -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
Remove-Item -Force $Zip -ErrorAction SilentlyContinue
Remove-Item -Force $ToolsZip -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

Copy-Item -Force $ServerExe (Join-Path $Stage "server.exe")
Copy-Item -Force (Join-Path $Root "scripts\migrate_current_falkordb.py") (Join-Path $Stage "migrate_current_falkordb.py")
Copy-Item -Force (Join-Path $Root "scripts\falkordb_bundle.py") (Join-Path $Stage "falkordb_bundle.py")
Copy-Item -Force (Join-Path $Root "scripts\import_falkordb_rdb.py") (Join-Path $Stage "import_falkordb_rdb.py")
Copy-Item -Force (Join-Path $Root "requirements-migration.txt") (Join-Path $Stage "requirements-migration.txt")
Copy-Item -Force (Join-Path $Root "docs\DEPLOYMENT.md") (Join-Path $Stage "DEPLOYMENT.md")
Copy-Item -Force (Join-Path $Root "README.md") (Join-Path $Stage "README.md")
Copy-Item -Force (Join-Path $Root "CONSOLIDATION_STATUS.md") (Join-Path $Stage "CONSOLIDATION_STATUS.md")

# Verify the embedded official OpenAI Secure MCP Tunnel runtime made it into the
# single-file Windows server and expose its bundled notice/license in the package.
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
$PythonEmbedZipSha256 = "4acbed6dd1c744b0376e3b1cf57ce906f9dc9e95e68824584c8099a63025a3c3"
$SitePackages = Join-Path $PythonDir "Lib\site-packages"

New-Item -ItemType Directory -Force -Path (Split-Path $PythonEmbedZip -Parent) | Out-Null
New-Item -ItemType Directory -Force -Path $PythonDir | Out-Null
Invoke-WebRequest -UseBasicParsing -Uri $PythonUrl -OutFile $PythonEmbedZip
$PythonEmbedActualSha256 = (Get-FileHash -Algorithm SHA256 $PythonEmbedZip).Hash.ToLowerInvariant()
if ($PythonEmbedActualSha256 -ne $PythonEmbedZipSha256) {
    throw "Embedded Python archive SHA256 mismatch: $PythonEmbedActualSha256"
}
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
$StateRoot = Join-Path $Root "FalkorDB"
Set-Location $Root

foreach ($Name in @("data", "logs", "tls", "tmp", "pycache", "imports", "exports", "migration", "cache", "config", "profile", "profile\AppData\Roaming", "profile\AppData\Local")) {
    New-Item -ItemType Directory -Force -Path (Join-Path $StateRoot $Name) | Out-Null
}

$env:FALKORDB_PORTABLE = "1"
$env:FALKORDB_PORTABLE_ROOT = $StateRoot
$env:FALKORDB_DATA_DIR = "data"
$env:FALKORDB_IMPORT_DIR = "imports"
$env:TEMP = Join-Path $StateRoot "tmp"
$env:TMP = Join-Path $StateRoot "tmp"
$env:PYTHONPYCACHEPREFIX = Join-Path $StateRoot "pycache"
$env:PYTHONDONTWRITEBYTECODE = "1"
$env:PYTHONNOUSERSITE = "1"
$env:PYTHONPATH = ""
$env:HOME = Join-Path $StateRoot "profile"
$env:USERPROFILE = Join-Path $StateRoot "profile"
$env:APPDATA = Join-Path $StateRoot "profile\AppData\Roaming"
$env:LOCALAPPDATA = Join-Path $StateRoot "profile\AppData\Local"
$env:XDG_CACHE_HOME = Join-Path $StateRoot "cache"
$env:XDG_CONFIG_HOME = Join-Path $StateRoot "config"
$env:PIP_CACHE_DIR = Join-Path $StateRoot "cache\pip"
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
$StateRoot = Join-Path $Root "FalkorDB"
Set-Location $Root

$RequiredFiles = @(
    "server.exe",
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
    if (-not (Test-Path (Join-Path $StateRoot $Relative))) {
        throw "Portable package is missing required local directory: FalkorDB\$Relative"
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

# Create the complete tools-bundle portable layout at package time as well as on launch.
& (Join-Path $Stage "portable-env.ps1")
$ToolsStateRoot = Join-Path $Stage "FalkorDB"
foreach ($Name in @("data", "logs", "tls", "tmp", "pycache", "imports", "exports", "migration", "cache", "config", "profile", "profile\AppData\Roaming", "profile\AppData\Local")) {
    if (-not (Test-Path (Join-Path $ToolsStateRoot $Name))) {
        throw "Portable tools package directory missing: FalkorDB\$Name"
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
$RuntimeSmokeDir = Join-Path (Join-Path $Stage "FalkorDB") $RuntimeSmokeDirName
$ExternalLeakDir = Join-Path $OutputDir $RuntimeSmokeDirName
Remove-Item -Recurse -Force $RuntimeSmokeDir -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $ExternalLeakDir -ErrorAction SilentlyContinue
$RuntimeStdout = Join-Path $ToolsStateRoot "logs\portable-runtime-smoke-stdout.txt"
$RuntimeStderr = Join-Path $ToolsStateRoot "logs\portable-runtime-smoke-stderr.txt"
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

$MutableRoot = Join-Path $Stage "FalkorDB"
$MutableRoots = @(
    (Join-Path $MutableRoot "data"),
    (Join-Path $MutableRoot "logs"),
    (Join-Path $MutableRoot "tmp"),
    (Join-Path $MutableRoot "pycache"),
    (Join-Path $MutableRoot "imports"),
    (Join-Path $MutableRoot "exports"),
    (Join-Path $MutableRoot "migration"),
    (Join-Path $MutableRoot "cache"),
    (Join-Path $MutableRoot "config"),
    (Join-Path $MutableRoot "profile"),
    (Join-Path $MutableRoot "tls")
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

Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $ToolsZip -CompressionLevel Optimal

# The normal end-user package is intentionally a single EXE. Migration,
# verification, Python, and PowerShell helpers live only in the separate tools
# archive above.
New-Item -ItemType Directory -Force -Path $RuntimeStage | Out-Null
Copy-Item -Force (Join-Path $Stage "server.exe") (Join-Path $RuntimeStage "server.exe")

$RuntimeExe = Join-Path $RuntimeStage "server.exe"
$RuntimeExternalCwd = Join-Path $OutputDir "runtime-default-portable-cwd"
$RuntimeStateRoot = Join-Path $RuntimeStage "FalkorDB"
$RuntimeExternalLeak = Join-Path $RuntimeExternalCwd "FalkorDB"
Remove-Item -Recurse -Force $RuntimeExternalCwd -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $RuntimeStateRoot -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $RuntimeExternalCwd | Out-Null

$RuntimeStdout = Join-Path $OutputDir "runtime-default-portable-stdout.txt"
$RuntimeStderr = Join-Path $OutputDir "runtime-default-portable-stderr.txt"
$RuntimeProc = Start-Process -FilePath $RuntimeExe `
    -ArgumentList @("--port", "0", "--checkpoint-wal-mb", "0") `
    -WorkingDirectory $RuntimeExternalCwd `
    -RedirectStandardOutput $RuntimeStdout `
    -RedirectStandardError $RuntimeStderr `
    -PassThru
try {
    Start-Sleep -Milliseconds 750
    if ($RuntimeProc.HasExited) {
        $ErrText = if (Test-Path $RuntimeStderr) { Get-Content $RuntimeStderr -Raw } else { "" }
        throw "Default-portable runtime smoke exited unexpectedly: $ErrText"
    }
    if (-not (Test-Path (Join-Path $RuntimeStateRoot "data"))) {
        throw "server.exe did not default to portable state under .\FalkorDB"
    }
    # The packaged process must create only bounded install-local diagnostics.
    $LocalLogs = Join-Path $RuntimeStateRoot "logs"
    $Breadcrumbs = Join-Path $LocalLogs "query-breadcrumbs.bin"
    $LatestCrash = Join-Path $LocalLogs "crash-latest.txt"
    if (-not (Test-Path $Breadcrumbs)) {
        throw "Packaged server did not initialize its local query breadcrumb journal"
    }
    if ((Get-Item $Breadcrumbs).Length -ne 2097152) {
        throw "Packaged server breadcrumb journal does not have its fixed 2 MiB size"
    }
    if (-not (Test-Path $LatestCrash) -or (Get-Item $LatestCrash).Length -gt 4096) {
        throw "Packaged server did not initialize a bounded install-local crash report"
    }
    if (Test-Path $RuntimeExternalLeak) {
        throw "server.exe leaked portable state into the caller working directory"
    }

    # A second process must fail closed instead of sharing the same WAL/data
    # directory with the live server.
    $SecondOwnerOutput = & $RuntimeExe --port 0 --checkpoint-wal-mb 0 2>&1
    if ($LASTEXITCODE -eq 0) {
        throw "second server.exe unexpectedly opened a data directory already owned by the first process"
    }
    if (($SecondOwnerOutput -join "`n") -notmatch "already in use by another FalkorDB native server") {
        throw "second server.exe did not report the expected exclusive data-directory ownership error"
    }
} finally {
    if (-not $RuntimeProc.HasExited) {
        Stop-Process -Id $RuntimeProc.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $RuntimeProc.Id -ErrorAction SilentlyContinue
    }
}

Remove-Item -Recurse -Force $RuntimeStateRoot -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $RuntimeExternalCwd -ErrorAction SilentlyContinue

# Prove an upgrade from the legacy portable layout preserves graph/state and
# credentials by moving top-level runtime state into the new FalkorDB folder.
$LegacyData = Join-Path $RuntimeStage "data"
$LegacySecrets = Join-Path $RuntimeStage "falkordb-secrets.txt"
New-Item -ItemType Directory -Force -Path $LegacyData | Out-Null
"# legacy portable secrets smoke" | Set-Content -Encoding ASCII $LegacySecrets

$MigrationCwd = Join-Path $OutputDir "runtime-legacy-migration-cwd"
Remove-Item -Recurse -Force $MigrationCwd -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $MigrationCwd | Out-Null

$MigrationStdout = Join-Path $OutputDir "runtime-legacy-migration-stdout.txt"
$MigrationStderr = Join-Path $OutputDir "runtime-legacy-migration-stderr.txt"
$MigrationProc = Start-Process -FilePath $RuntimeExe `
    -ArgumentList @("--port", "0", "--checkpoint-wal-mb", "0") `
    -WorkingDirectory $MigrationCwd `
    -RedirectStandardOutput $MigrationStdout `
    -RedirectStandardError $MigrationStderr `
    -PassThru
try {
    Start-Sleep -Milliseconds 750
    if ($MigrationProc.HasExited) {
        $ErrText = if (Test-Path $MigrationStderr) { Get-Content $MigrationStderr -Raw } else { "" }
        throw "Legacy portable migration smoke exited unexpectedly: $ErrText"
    }
    if (Test-Path $LegacyData) {
        throw "Legacy top-level data directory was not migrated"
    }
    if (Test-Path $LegacySecrets) {
        throw "Legacy top-level secrets file was not migrated"
    }
    if (-not (Test-Path (Join-Path $RuntimeStateRoot "data"))) {
        throw "Migrated portable data directory is missing under FalkorDB"
    }
    if (-not (Test-Path (Join-Path $RuntimeStateRoot "falkordb-secrets.txt"))) {
        throw "Migrated portable secrets file is missing under FalkorDB"
    }
} finally {
    if (-not $MigrationProc.HasExited) {
        Stop-Process -Id $MigrationProc.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $MigrationProc.Id -ErrorAction SilentlyContinue
    }
}

Remove-Item -Recurse -Force $RuntimeStateRoot -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $MigrationCwd -ErrorAction SilentlyContinue

$RuntimeEntries = @(Get-ChildItem -Force $RuntimeStage)
if ($RuntimeEntries.Count -ne 1 -or $RuntimeEntries[0].Name -ne "server.exe") {
    throw "Runtime package must contain exactly one top-level entry: server.exe"
}

Compress-Archive -Path (Join-Path $RuntimeStage "*") -DestinationPath $Zip -CompressionLevel Optimal

Write-Host "NATIVE_WINDOWS_DEFAULT_PORTABLE_PASS"
Write-Host "NATIVE_WINDOWS_LEGACY_LAYOUT_MIGRATION_PASS"
Write-Host "NATIVE_WINDOWS_SINGLE_EXE_RUNTIME_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_SELF_VERIFY_PASS"
Write-Host "NATIVE_WINDOWS_EMBEDDED_MIGRATION_RUNTIME_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_EXTERNAL_CWD_PASS"
Write-Host "NATIVE_WINDOWS_STATIC_RUNTIME_AUDIT_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_CONFINEMENT_PASS"
Write-Host "NATIVE_WINDOWS_PACKAGE_PASS"
Write-Host "Runtime directory: $RuntimeStage"
Write-Host "Runtime archive:   $Zip"
Write-Host "Tools directory:   $Stage"
Write-Host "Tools archive:     $ToolsZip"
Write-Host "server.exe SHA256: $Hash"

# The package verification deliberately runs negative native-process tests.
# Ensure the GitHub/PowerShell host exits according to this script's success,
# not a stale child-process LASTEXITCODE.
$global:LASTEXITCODE = 0
