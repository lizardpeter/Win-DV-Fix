param(
    [string]$WorkDir = "$PSScriptRoot\..\build"
)

$ErrorActionPreference = "Stop"

Write-Host "=== FalkorDB native Windows: bootstrap ==="
& "$PSScriptRoot\bootstrap_windows.ps1" -WorkDir $WorkDir
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "=== FalkorDB native Windows: compile/link diagnostics ==="
& "$PSScriptRoot\diagnose.ps1" -WorkDir $WorkDir
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "=== FalkorDB native Windows: runtime smoke ==="
& "$PSScriptRoot\smoke_run.ps1" -WorkDir $WorkDir
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "FALKORDB_NATIVE_WINDOWS_BRINGUP_PASS"
