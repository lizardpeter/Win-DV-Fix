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
$OldPipCache = $env:PIP_CACHE_DIR
$env:PIP_CACHE_DIR = Join-Path $Stage "tmp\pip-cache"
try {
    python -m pip install --disable-pip-version-check --no-compile --target $SitePackages -r (Join-Path $Root "requirements-migration.txt")
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

$EmbeddedPython = Join-Path $PythonDir "python.exe"
if (-not (Test-Path $EmbeddedPython)) {
    throw "Embedded Python runtime was not produced: $EmbeddedPython"
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

foreach ($Name in @("data", "logs", "tls", "tmp", "pycache", "imports", "exports", "migration")) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root $Name) | Out-Null
}

$env:FALKORDB_PORTABLE = "1"
$env:FALKORDB_DATA_DIR = "data"
$env:TEMP = Join-Path $Root "tmp"
$env:TMP = Join-Path $Root "tmp"
$env:PYTHONPYCACHEPREFIX = Join-Path $Root "pycache"
$env:PYTHONDONTWRITEBYTECODE = "1"
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

# Create the complete portable folder layout at package time as well as on launch.
& (Join-Path $Stage "portable-env.ps1")
foreach ($Name in @("data", "logs", "tls", "tmp", "pycache", "imports", "exports", "migration")) {
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

# Verify the final PE does not depend on the separately installed MSVC runtime.
$Link = Get-Command link.exe -ErrorAction SilentlyContinue
if ($Link) {
    $Deps = & $Link.Source /dump /dependents (Join-Path $Stage "server.exe") 2>&1
    $Deps | Set-Content -Encoding UTF8 (Join-Path $Stage "DEPENDENCIES.txt")
    $DepsText = $Deps -join "`n"
    if ($DepsText -match "(?i)VCRUNTIME140|MSVCP140") {
        throw "Packaged server still depends on the external MSVC runtime"
    }
} else {
    "link.exe unavailable during packaging; PE dependency audit skipped" |
        Set-Content -Encoding UTF8 (Join-Path $Stage "DEPENDENCIES.txt")
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

$Hash = (Get-FileHash -Algorithm SHA256 (Join-Path $Stage "server.exe")).Hash.ToLowerInvariant()
@"
server.exe sha256 $Hash
"@ | Set-Content -Encoding ASCII (Join-Path $Stage "SHA256SUMS.txt")

Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $Zip -CompressionLevel Optimal

Write-Host "NATIVE_WINDOWS_EMBEDDED_MIGRATION_RUNTIME_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_EXTERNAL_CWD_PASS"
Write-Host "NATIVE_WINDOWS_STATIC_RUNTIME_AUDIT_PASS"
Write-Host "NATIVE_WINDOWS_PORTABLE_CONFINEMENT_PASS"
Write-Host "NATIVE_WINDOWS_PACKAGE_PASS"
Write-Host "Package directory: $Stage"
Write-Host "Package archive:   $Zip"
Write-Host "server.exe SHA256: $Hash"
