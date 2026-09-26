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

$Python = Get-Command python -ErrorAction SilentlyContinue
if (-not $Python) {
    $Python = Get-Command py -ErrorAction SilentlyContinue
}
if (-not $Python) {
    throw "Python is required only for migration helper tools. The database server itself does not require Python."
}

$Script = switch ($Tool) {
    "migrate-live" { "migrate_current_falkordb.py" }
    "bundle"       { "falkordb_bundle.py" }
    "import-rdb"   { "import_falkordb_rdb.py" }
}

& $Python.Source (Join-Path $PSScriptRoot $Script) @Arguments
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

Write-Host "NATIVE_WINDOWS_PORTABLE_CONFINEMENT_PASS"
Write-Host "NATIVE_WINDOWS_PACKAGE_PASS"
Write-Host "Package directory: $Stage"
Write-Host "Package archive:   $Zip"
Write-Host "server.exe SHA256: $Hash"
