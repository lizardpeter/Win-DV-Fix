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
CLAUDE_WEB_CLIENT_ID = "https://claude.ai/oauth/mcp-oauth-client-metadata"
CLAUDE_WEB_REDIRECT_URI = "https://claude.ai/api/mcp/auth_callback"
CLAUDE_CODE_CLIENT_ID = "https://claude.ai/oauth/claude-code-client-metadata"
CLAUDE_CODE_REDIRECT_URI = "http://localhost:43123/callback"
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


def request_chunked(ctx, method, target, body, headers=None):
    conn = http.client.HTTPSConnection(HOST, PORT, context=ctx, timeout=10)
    headers = dict(headers or {})
    payload = body.encode() if isinstance(body, str) else bytes(body)
    conn.putrequest(method, target)
    for name, value in headers.items():
        conn.putheader(name, value)
    conn.putheader("Transfer-Encoding", "chunked")
    conn.endheaders()

    # Send multiple chunks plus a harmless trailer to exercise the same HTTP/1.1
    # framing a reverse proxy may use for a browser form POST.
    cut1 = max(1, len(payload) // 3)
    cut2 = max(cut1 + 1, (2 * len(payload)) // 3)
    for chunk in (payload[:cut1], payload[cut1:cut2], payload[cut2:]):
        if not chunk:
            continue
        conn.send(f"{len(chunk):X}\r\n".encode())
        conn.send(chunk)
        conn.send(b"\r\n")
    conn.send(b"0\r\nX-Proxy-Trailer: oauth-ci\r\n\r\n")

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


def oauth_link(
    ctx,
    client_id=CLIENT_ID,
    redirect_uri=REDIRECT_URI,
    state="native-ci-state",
    chunked_authorize=False,
):
    resource = f"https://{HOST}:{PORT}/mcp"
    scope = "graph:read graph:write graph:admin"
    auth_form = {
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": redirect_uri,
        "state": state,
        "code_challenge": challenge(VERIFIER),
        "code_challenge_method": "S256",
        "resource": resource,
        "scope": scope,
        "api_token": API_TOKEN,
    }
    encoded = urllib.parse.urlencode(auth_form)
    authorize_request = request_chunked if chunked_authorize else request
    status, headers, data = authorize_request(
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
    assert params["state"][0] == state
    assert params["iss"][0] == f"https://{HOST}:{PORT}"

    token_form = urllib.parse.urlencode(
        {
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "redirect_uri": redirect_uri,
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
    return refreshed["access_token"], refreshed.get(
        "refresh_token", token_response["refresh_token"]
    )


def write_phase(ctx, token_file: Path, import_root: Path):
    status, _, data = request(ctx, "GET", "/.well-known/oauth-protected-resource")
    assert status == 200, (status, data)
    metadata = json.loads(data)
    assert metadata["resource"] == f"https://{HOST}:{PORT}/mcp"
    assert "graph:admin" in metadata["scopes_supported"]

    status, _, data = request(ctx, "GET", "/.well-known/oauth-authorization-server")
    assert status == 200, (status, data)
    oauth_metadata = json.loads(data)
    assert oauth_metadata["client_id_metadata_document_supported"] is True
    assert oauth_metadata["revocation_endpoint"] == f"https://{HOST}:{PORT}/oauth/revoke"

    discover = rpc(ctx, "server/discover")
    assert "2026-07-28" in discover["supportedVersions"]

    tools = rpc(ctx, "tools/list", request_id=2)
    names = {tool["name"] for tool in tools["tools"]}
    for required in {
        "list_graphs",
        "database_stats",
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
        "import_falkordb_rdb_file",
        "bulk_import_file",
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

    claude_web_token, claude_web_refresh = oauth_link(
        ctx,
        client_id=CLAUDE_WEB_CLIENT_ID,
        redirect_uri=CLAUDE_WEB_REDIRECT_URI,
        state="claude-web-ci-state",
        chunked_authorize=True,
    )
    claude_web_read = rpc(
        ctx,
        "tools/call",
        {"name": "list_graphs", "arguments": {}},
        token=claude_web_token,
        request_id=29,
    )
    assert claude_web_read["isError"] is False, claude_web_read
    print("CLAUDE_WEB_CHUNKED_OAUTH_PASS")

    claude_token, _ = oauth_link(
        ctx,
        client_id=CLAUDE_CODE_CLIENT_ID,
        redirect_uri=CLAUDE_CODE_REDIRECT_URI,
        state="claude-code-ci-state",
    )
    claude_read = rpc(
        ctx,
        "tools/call",
        {"name": "list_graphs", "arguments": {}},
        token=claude_token,
        request_id=30,
    )
    assert claude_read["isError"] is False, claude_read
    print("CLAUDE_OAUTH_PASS")

    token, _ = oauth_link(ctx)

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

    # Server-local generic file import: the corpus itself never traverses MCP.
    import_root.mkdir(parents=True, exist_ok=True)
    import_file = import_root / "mcp-bulk.jsonl"
    import_bytes = (
        b'{"kind":"alpha","value":101}\n'
        b'{"kind":"beta","value":202}\n'
    )
    import_file.write_bytes(import_bytes)
    import_sha = hashlib.sha256(import_bytes).hexdigest()
    import_args = {
        "graph": "mcp-ci",
        "file": import_file.name,
        "format": "jsonl",
        "sha256": import_sha,
        "batch_size": 1,
        "cypher": (
            "UNWIND {{ROWS}} AS row "
            "MERGE (n:McpBulk {kind:row.kind}) "
            "SET n.value=row.value"
        ),
    }

    dry_run = rpc(
        ctx,
        "tools/call",
        {
            "name": "bulk_import_file",
            "arguments": {**import_args, "dry_run": True},
        },
        token=token,
        request_id=7,
    )
    assert dry_run["isError"] is False, dry_run
    assert dry_run["structuredContent"]["records_total"] == 2, dry_run
    assert dry_run["structuredContent"]["records_imported"] == 0, dry_run

    imported = rpc(
        ctx,
        "tools/call",
        {
            "name": "bulk_import_file",
            "arguments": {**import_args, "checkpoint": False},
        },
        token=token,
        request_id=8,
    )
    assert imported["isError"] is False, imported
    assert imported["structuredContent"]["records_imported"] == 2, imported
    assert imported["structuredContent"]["batches"] == 2, imported

    bulk_read = rpc(
        ctx,
        "tools/call",
        {
            "name": "query_graph_read",
            "arguments": {
                "graph": "mcp-ci",
                "cypher": "MATCH (n:McpBulk) RETURN n.kind,n.value ORDER BY n.kind",
            },
        },
        token=token,
        request_id=9,
    )
    assert bulk_read["structuredContent"]["rows"] == [
        ["alpha", 101],
        ["beta", 202],
    ], bulk_read

    checkpoint = rpc(
        ctx,
        "tools/call",
        {"name": "checkpoint_graph", "arguments": {"graph": "mcp-ci"}},
        token=token,
        request_id=10,
    )
    assert checkpoint["isError"] is False

    stats = rpc(
        ctx,
        "tools/call",
        {
            "name": "database_stats",
            "arguments": {
                "graph": "mcp-ci",
                "include_memory": True,
                "memory_samples": 10,
            },
        },
        token=token,
        request_id=11,
    )
    assert stats["isError"] is False, stats
    stats_body = stats["structuredContent"]
    assert stats_body["graph_count"] == 1, stats_body
    assert stats_body["total_storage_bytes"] >= stats_body["graphs_storage_bytes"], stats_body
    assert stats_body["graphs_storage_bytes"] >= stats_body["attributed_graph_bytes"], stats_body
    assert stats_body["unattributed_graph_storage_bytes"] == (
        stats_body["graphs_storage_bytes"] - stats_body["attributed_graph_bytes"]
    ), stats_body
    graph_stats = stats_body["graphs"][0]
    assert graph_stats["graph"] == "mcp-ci", graph_stats
    assert graph_stats["nodes"] >= 3, graph_stats
    assert graph_stats["relationships"] == 0, graph_stats
    assert graph_stats["persistent_bytes"] > 0, graph_stats
    assert graph_stats["persistent_bytes"] == (
        graph_stats["wal_bytes"] + graph_stats["checkpoint_bytes"]
    ), graph_stats
    assert graph_stats["checkpoint_count"] >= 1, graph_stats
    assert graph_stats["estimated_memory_bytes"] is not None, graph_stats

    # Filtering detail rows must not change whole-database storage attribution.
    all_stats = rpc(
        ctx,
        "tools/call",
        {
            "name": "database_stats",
            "arguments": {"include_memory": False},
        },
        token=token,
        request_id=12,
    )
    assert all_stats["isError"] is False, all_stats
    all_stats_body = all_stats["structuredContent"]
    filtered_stats_body = stats_body
    assert filtered_stats_body["attributed_graph_bytes"] == all_stats_body["attributed_graph_bytes"]
    assert filtered_stats_body["unattributed_graph_storage_bytes"] == (
        all_stats_body["unattributed_graph_storage_bytes"]
    )
    assert filtered_stats_body["total_storage_bytes"] == all_stats_body["total_storage_bytes"]
    print("MCP_DATABASE_STATS_PASS")

    token_file.write_text(
        json.dumps({"claude_web_refresh_token": claude_web_refresh}),
        encoding="utf-8",
    )
    print("MCP_DIRECT_OAUTH_WRITE_PASS")


def read_phase(ctx, token_file: Path):
    persisted = json.loads(token_file.read_text(encoding="utf-8"))
    refresh_token = persisted["claude_web_refresh_token"]
    resource = f"https://{HOST}:{PORT}/mcp"
    scope = "graph:read graph:write graph:admin"
    refresh_form = urllib.parse.urlencode(
        {
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_WEB_CLIENT_ID,
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
        raise AssertionError(
            ("persistent Claude Web refresh failed after server restart", status, data.decode(errors="replace"))
        )
    refreshed = json.loads(data)
    token = refreshed["access_token"]
    assert refreshed["refresh_token"] == refresh_token
    print("CLAUDE_WEB_OAUTH_RESTART_REFRESH_PASS")
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

    stats = rpc(
        ctx,
        "tools/call",
        {
            "name": "database_stats",
            "arguments": {"graph": "mcp-ci", "include_memory": False},
        },
        token=token,
        request_id=9,
    )
    assert stats["isError"] is False, stats
    graph_stats = stats["structuredContent"]["graphs"][0]
    assert graph_stats["nodes"] >= 3, graph_stats
    assert graph_stats["persistent_bytes"] > 0, graph_stats
    assert graph_stats["estimated_memory_bytes"] is None, graph_stats
    print("MCP_DATABASE_STATS_RESTART_PASS")
    print("MCP_DIRECT_OAUTH_RESTART_PASS")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    parser.add_argument("--ca", required=True)
    parser.add_argument("--token-file", required=True)
    parser.add_argument("--import-root")
    args = parser.parse_args()

    ctx = ssl.create_default_context(cafile=args.ca)
    token_file = Path(args.token_file)

    if args.phase == "write":
        if not args.import_root:
            raise SystemExit("--import-root is required for write phase")
        write_phase(ctx, token_file, Path(args.import_root))
    else:
        read_phase(ctx, token_file)


if __name__ == "__main__":
    main()
