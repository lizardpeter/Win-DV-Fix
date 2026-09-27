param(
    [string]$TunnelClient = "",
    [string]$McpUrl = "http://127.0.0.1:18444/mcp",
    [string]$HealthListen = "127.0.0.1:18445"
)

$ErrorActionPreference = "Stop"
$Root = [IO.Path]::GetFullPath($PSScriptRoot)

if (-not $env:CONTROL_PLANE_TUNNEL_ID) {
    throw "Set CONTROL_PLANE_TUNNEL_ID to the tunnel_id from OpenAI Platform tunnel settings."
}
if (-not $env:CONTROL_PLANE_API_KEY) {
    throw "Set CONTROL_PLANE_API_KEY to the runtime API key for the OpenAI Secure MCP Tunnel."
}

if ([string]::IsNullOrWhiteSpace($TunnelClient)) {
    if ($env:TUNNEL_CLIENT_EXE) {
        $TunnelClient = $env:TUNNEL_CLIENT_EXE
    } elseif (Test-Path (Join-Path $Root "tunnel-client.exe")) {
        $TunnelClient = Join-Path $Root "tunnel-client.exe"
    } else {
        $Command = Get-Command tunnel-client.exe -ErrorAction SilentlyContinue
        if ($null -eq $Command) {
            $Command = Get-Command tunnel-client -ErrorAction SilentlyContinue
        }
        if ($null -ne $Command) {
            $TunnelClient = $Command.Source
        }
    }
}
if ([string]::IsNullOrWhiteSpace($TunnelClient) -or -not (Test-Path $TunnelClient)) {
    throw "tunnel-client was not found. Download the latest OpenAI tunnel-client release, place tunnel-client.exe beside this script, or set TUNNEL_CLIENT_EXE."
}

function Unquote([string]$Value) {
    $Value = $Value.Trim()
    if ($Value.Length -ge 2) {
        if (($Value[0] -eq '"' -and $Value[$Value.Length - 1] -eq '"') -or
            ($Value[0] -eq "'" -and $Value[$Value.Length - 1] -eq "'")) {
            return $Value.Substring(1, $Value.Length - 2)
        }
    }
    return $Value
}

$LocalToken = $env:FALKORDB_API_TOKEN
if ([string]::IsNullOrWhiteSpace($LocalToken)) {
    $Secrets = Join-Path $Root "falkordb-secrets.txt"
    if (Test-Path $Secrets) {
        foreach ($Line in Get-Content $Secrets) {
            $Trimmed = $Line.Trim()
            if ($Trimmed -match '^(?:set\s+|\$env:)?FALKORDB_API_TOKEN\s*=\s*(.+)$') {
                $LocalToken = Unquote $Matches[1]
                break
            }
        }
    }
}
if ([string]::IsNullOrWhiteSpace($LocalToken)) {
    throw "FALKORDB_API_TOKEN was not found in the environment or falkordb-secrets.txt."
}

try {
    $Health = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:18444/healthz" -TimeoutSec 3
    if ($Health.StatusCode -ne 200) {
        throw "local MCP backend health returned HTTP $($Health.StatusCode)"
    }
} catch {
    throw "The local Secure MCP Tunnel backend is not reachable on 127.0.0.1:18444. Start server.exe with --tunnel-mcp-bind 127.0.0.1:18444 (or set FALKORDB_TUNNEL_MCP_BIND). $($_.Exception.Message)"
}

$env:FALKORDB_TUNNEL_LOCAL_AUTH = "Bearer $LocalToken"
try {
    Write-Host "Starting OpenAI Secure MCP Tunnel..."
    Write-Host "Tunnel ID: $($env:CONTROL_PLANE_TUNNEL_ID)"
    Write-Host "Local MCP: $McpUrl"
    $Args = @(
        "run",
        "--control-plane.tunnel-id", $env:CONTROL_PLANE_TUNNEL_ID,
        "--control-plane.api-key", "env:CONTROL_PLANE_API_KEY",
        "--mcp.server-url", $McpUrl,
        "--mcp.extra-headers", "Authorization: env:FALKORDB_TUNNEL_LOCAL_AUTH",
        "--mcp.discovery-extra-headers", "Authorization: env:FALKORDB_TUNNEL_LOCAL_AUTH",
        "--health.listen-addr", $HealthListen,
        "--log.level", "info",
        "--log.format", "struct-text"
    )
    & $TunnelClient @Args
    exit $LASTEXITCODE
} finally {
    Remove-Item Env:FALKORDB_TUNNEL_LOCAL_AUTH -ErrorAction SilentlyContinue
}
