#!/usr/bin/env python3
import http.client
import json

HOST = "127.0.0.1"
PORT = 18444
API_TOKEN = "native-api-write-secret"


def request(method, target, body=None, headers=None):
    conn = http.client.HTTPConnection(HOST, PORT, timeout=10)
    headers = dict(headers or {})
    payload = body
    if isinstance(body, dict):
        payload = json.dumps(body).encode()
        headers.setdefault("Content-Type", "application/json")
    conn.request(method, target, body=payload, headers=headers)
    resp = conn.getresponse()
    data = resp.read()
    out = (resp.status, dict(resp.getheaders()), data)
    conn.close()
    return out


def rpc(method, params=None, token=None, request_id=1):
    headers = {
        "Accept": "application/json, text/event-stream",
        "Content-Type": "application/json",
        "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": method,
    }
    if method == "tools/call" and params and params.get("name"):
        headers["Mcp-Name"] = params["name"]
    if token:
        headers["Authorization"] = f"Bearer {token}"
    status, _, data = request(
        "POST",
        "/mcp",
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params or {},
        },
        headers,
    )
    assert status == 200, (status, data.decode(errors="replace"))
    parsed = json.loads(data)
    assert "error" not in parsed, parsed
    return parsed["result"]


def main():
    status, _, data = request("GET", "/healthz")
    assert status == 200, (status, data)

    status, _, data = request("GET", "/.well-known/oauth-protected-resource")
    assert status == 404, (status, data)

    tools = rpc("tools/list", request_id=1)
    assert len(tools["tools"]) >= 12
    for tool in tools["tools"]:
        assert tool["securitySchemes"] == [{"type": "noauth"}], tool
        assert tool["_meta"]["securitySchemes"] == [{"type": "noauth"}], tool

    denied = rpc(
        "tools/call",
        {"name": "list_graphs", "arguments": {}},
        request_id=2,
    )
    assert denied["isError"] is True, denied
    assert "mcp/www_authenticate" not in denied.get("_meta", {}), denied

    created = rpc(
        "tools/call",
        {"name": "create_graph", "arguments": {"graph": "tunnel-ci"}},
        token=API_TOKEN,
        request_id=3,
    )
    assert created["isError"] is False, created

    written = rpc(
        "tools/call",
        {
            "name": "query_graph_write",
            "arguments": {
                "graph": "tunnel-ci",
                "cypher": "MERGE (:TunnelProbe {name:'secure-tunnel', value:73}) RETURN 73",
            },
        },
        token=API_TOKEN,
        request_id=4,
    )
    assert written["isError"] is False, written

    read = rpc(
        "tools/call",
        {
            "name": "query_graph_read",
            "arguments": {
                "graph": "tunnel-ci",
                "cypher": "MATCH (n:TunnelProbe {name:'secure-tunnel'}) RETURN n.value",
            },
        },
        token=API_TOKEN,
        request_id=5,
    )
    assert read["structuredContent"]["rows"] == [[73]], read
    print("MCP_SECURE_TUNNEL_BACKEND_PASS")


if __name__ == "__main__":
    main()
