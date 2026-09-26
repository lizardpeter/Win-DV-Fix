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
Copy-Item -Force (Join-Path $Root "requirements-migration.txt") (Join-Path $Stage "requirements-migration.txt")
Copy-Item -Force (Join-Path $Root "DEPLOYMENT.md") (Join-Path $Stage "DEPLOYMENT.md")
Copy-Item -Force (Join-Path $Root "README.md") (Join-Path $Stage "README.md")
Copy-Item -Force (Join-Path $Root "PORT_STATUS.md") (Join-Path $Stage "PORT_STATUS.md")

@'
param(
    [string]$DataDir = "$PSScriptRoot\data",
    [string]$Bind = "127.0.0.1:6379"
)

if (-not $env:FALKORDB_PASSWORD) {
    throw "Set FALKORDB_PASSWORD before starting the server."
}

& "$PSScriptRoot\server.exe" --bind $Bind --data-dir $DataDir
exit $LASTEXITCODE
'@ | Set-Content -Encoding UTF8 (Join-Path $Stage "start-local.ps1")

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

Write-Host "NATIVE_WINDOWS_PACKAGE_PASS"
Write-Host "Package directory: $Stage"
Write-Host "Package archive:   $Zip"
Write-Host "server.exe SHA256: $Hash"
