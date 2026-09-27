#!/usr/bin/env python3
import argparse
import base64
import hashlib
import http.client
import json
import ssl
import urllib.parse
from pathlib import Path

HOST = "localhost"
PORT = 8443
API_TOKEN = "native-api-write-secret"
CLIENT_ID = "https://chatgpt.com/oauth/client.json"
REDIRECT_URI = "https://chatgpt.com/connector_platform_oauth_redirect"
VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"


def challenge(verifier: str) -> str:
    raw = hashlib.sha256(verifier.encode()).digest()
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def request(ctx, method, target, body=None, headers=None):
    conn = http.client.HTTPSConnection(HOST, PORT, context=ctx, timeout=10)
    headers = dict(headers or {})
    payload = body
    if isinstance(body, dict):
        payload = json.dumps(body).encode()
        headers.setdefault("Content-Type", "application/json")
    elif isinstance(body, str):
        payload = body.encode()
    conn.request(method, target, body=payload, headers=headers)
    resp = conn.getresponse()
    data = resp.read()
    result = (resp.status, dict(resp.getheaders()), data)
    conn.close()
    return result


def rpc(ctx, method, params=None, token=None, request_id=1):
    body = {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": params or {},
    }
    headers = {
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": method,
    }
    if method == "tools/call" and params and params.get("name"):
        headers["Mcp-Name"] = params["name"]
    if token:
        headers["Authorization"] = f"Bearer {token}"
    status, response_headers, data = request(
        ctx, "POST", "/mcp", body=body, headers=headers
    )
    if status != 200:
        raise AssertionError((status, data.decode(errors="replace")))
    parsed = json.loads(data)
    if parsed.get("error"):
        raise AssertionError(parsed)
    return parsed["result"]


def oauth_link(ctx):
    resource = f"https://{HOST}:{PORT}/mcp"
    scope = "graph:read graph:write graph:admin"
    auth_form = {
        "response_type": "code",
        "client_id": CLIENT_ID,
        "redirect_uri": REDIRECT_URI,
        "state": "native-ci-state",
        "code_challenge": challenge(VERIFIER),
        "code_challenge_method": "S256",
        "resource": resource,
        "scope": scope,
        "api_token": API_TOKEN,
    }
    encoded = urllib.parse.urlencode(auth_form)
    status, headers, data = request(
        ctx,
        "POST",
        "/oauth/authorize",
        body=encoded,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    if status != 302:
        raise AssertionError((status, data.decode(errors="replace")))
    location = headers.get("Location")
    if not location:
        raise AssertionError("missing OAuth redirect")
    redirect = urllib.parse.urlparse(location)
    params = urllib.parse.parse_qs(redirect.query)
    code = params["code"][0]
    assert params["state"][0] == "native-ci-state"
    assert params["iss"][0] == f"https://{HOST}:{PORT}"

    token_form = urllib.parse.urlencode(
        {
            "grant_type": "authorization_code",
            "code": code,
            "client_id": CLIENT_ID,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": VERIFIER,
            "resource": resource,
        }
    )
    status, _, data = request(
        ctx,
        "POST",
        "/oauth/token",
        body=token_form,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    if status != 200:
        raise AssertionError((status, data.decode(errors="replace")))
    token_response = json.loads(data)
    assert token_response["token_type"] == "Bearer"
    assert token_response["scope"] == scope

    refresh_form = urllib.parse.urlencode(
        {
            "grant_type": "refresh_token",
            "refresh_token": token_response["refresh_token"],
            "resource": resource,
            "scope": scope,
        }
    )
    status, _, data = request(
        ctx,
        "POST",
        "/oauth/token",
        body=refresh_form,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    if status != 200:
        raise AssertionError((status, data.decode(errors="replace")))
    refreshed = json.loads(data)
    return {
        "access_token": refreshed["access_token"],
        "refresh_token": refreshed["refresh_token"],
        "scope": refreshed["scope"],
        "resource": resource,
    }


def write_phase(ctx, token_file: Path):
    status, headers, data = request(
        ctx,
        "OPTIONS",
        "/mcp",
        headers={
            "Origin": "https://chatgpt.com",
            "Access-Control-Request-Method": "POST",
            "Access-Control-Request-Headers": "authorization,content-type,mcp-protocol-version,mcp-method,mcp-name",
        },
    )
    assert status == 204, (status, data)
    assert headers.get("Access-Control-Allow-Origin") == "*", headers
    assert "POST" in headers.get("Access-Control-Allow-Methods", ""), headers
    assert "authorization" in headers.get("Access-Control-Allow-Headers", "").lower(), headers

    status, _, data = request(ctx, "GET", "/.well-known/oauth-protected-resource")
    assert status == 200, (status, data)
    metadata = json.loads(data)
    assert metadata["resource"] == f"https://{HOST}:{PORT}/mcp"
    assert "graph:admin" in metadata["scopes_supported"]

    discover = rpc(ctx, "server/discover")
    assert "2026-07-28" in discover["supportedVersions"]

    tools = rpc(ctx, "tools/list", request_id=2)
    names = {tool["name"] for tool in tools["tools"]}
    for required in {
        "list_graphs",
        "query_graph_read",
        "query_graph_write",
        "batch_graph_queries",
        "copy_graph",
        "delete_graph",
        "checkpoint_graph",
        "checkpoint_all_graphs",
        "flush_all_graphs",
        "export_graph_dump",
        "restore_graph_dump",
    }:
        assert required in names, required

    unauth = rpc(
        ctx,
        "tools/call",
        {"name": "list_graphs", "arguments": {}},
        request_id=3,
    )
    assert unauth["isError"] is True
    assert "mcp/www_authenticate" in unauth["_meta"]

    token_state = oauth_link(ctx)
    token = token_state["access_token"]

    created = rpc(
        ctx,
        "tools/call",
        {"name": "create_graph", "arguments": {"graph": "mcp-ci"}},
        token=token,
        request_id=4,
    )
    assert created["isError"] is False

    written = rpc(
        ctx,
        "tools/call",
        {
            "name": "query_graph_write",
            "arguments": {
                "graph": "mcp-ci",
                "cypher": "MERGE (:McpProbe {name:'direct-oauth', value:42}) RETURN 42",
            },
        },
        token=token,
        request_id=5,
    )
    assert written["isError"] is False

    read = rpc(
        ctx,
        "tools/call",
        {
            "name": "query_graph_read",
            "arguments": {
                "graph": "mcp-ci",
                "cypher": "MATCH (n:McpProbe {name:'direct-oauth'}) RETURN n.value",
            },
        },
        token=token,
        request_id=6,
    )
    assert read["structuredContent"]["rows"] == [[42]]

    checkpoint = rpc(
        ctx,
        "tools/call",
        {"name": "checkpoint_graph", "arguments": {"graph": "mcp-ci"}},
        token=token,
        request_id=7,
    )
    assert checkpoint["isError"] is False

    token_file.write_text(json.dumps(token_state), encoding="utf-8")
    print("MCP_DIRECT_OAUTH_WRITE_PASS")


def read_phase(ctx, token_file: Path):
    state = json.loads(token_file.read_text(encoding="utf-8"))
    token = state["access_token"]

    # First prove the already-issued access token survives a hard server restart.
    read = rpc(
        ctx,
        "tools/call",
        {
            "name": "query_graph_read",
            "arguments": {
                "graph": "mcp-ci",
                "cypher": "MATCH (n:McpProbe {name:'direct-oauth'}) RETURN n.value",
            },
        },
        token=token,
        request_id=8,
    )
    assert read["structuredContent"]["rows"] == [[42]]
    print("MCP_DIRECT_OAUTH_ACCESS_TOKEN_RESTART_PASS")

    # Then prove the long-lived refresh token also survives the restart and can
    # mint a fresh access token without another browser authorization.
    refresh_form = urllib.parse.urlencode(
        {
            "grant_type": "refresh_token",
            "refresh_token": state["refresh_token"],
            "resource": state["resource"],
            "scope": state["scope"],
        }
    )
    status, _, data = request(
        ctx,
        "POST",
        "/oauth/token",
        body=refresh_form,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    if status != 200:
        raise AssertionError((status, data.decode(errors="replace")))
    refreshed = json.loads(data)
    assert refreshed["token_type"] == "Bearer"
    assert refreshed["scope"] == state["scope"]
    assert refreshed["refresh_token"]

    read_after_refresh = rpc(
        ctx,
        "tools/call",
        {
            "name": "query_graph_read",
            "arguments": {
                "graph": "mcp-ci",
                "cypher": "MATCH (n:McpProbe {name:'direct-oauth'}) RETURN n.value",
            },
        },
        token=refreshed["access_token"],
        request_id=9,
    )
    assert read_after_refresh["structuredContent"]["rows"] == [[42]]
    print("MCP_DIRECT_OAUTH_REFRESH_TOKEN_RESTART_PASS")
    print("MCP_DIRECT_OAUTH_RESTART_PASS")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    parser.add_argument("--ca", required=True)
    parser.add_argument("--token-file", required=True)
    args = parser.parse_args()

    ctx = ssl.create_default_context(cafile=args.ca)
    token_file = Path(args.token_file)

    if args.phase == "write":
        write_phase(ctx, token_file)
    else:
        read_phase(ctx, token_file)


if __name__ == "__main__":
    main()
