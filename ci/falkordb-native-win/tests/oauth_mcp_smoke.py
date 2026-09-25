import argparse
import json
import os
import pathlib
import ssl
import urllib.error
import urllib.request

import anyio
import httpx2
from mcp import Client
from mcp.client.streamable_http import streamable_http_client

HOST = "localhost"
PORT = 8443
MCP_URL = f"https://{HOST}:{PORT}/mcp"
RESOURCE = MCP_URL
GRAPH = "chatgpt-oauth-smoke"

TLS_DIR = pathlib.Path(os.environ["FALKORDB_TEST_TLS_DIR"]).resolve()
OAUTH_DIR = pathlib.Path(os.environ["FALKORDB_TEST_OAUTH_DIR"]).resolve()
CA = TLS_DIR / "ca.pem"


def tls_context():
    return ssl.create_default_context(cafile=str(CA))


def token(name: str) -> str:
    return (OAUTH_DIR / name).read_text(encoding="utf-8").strip()


def raw_request(method, path, body=None, bearer=None, extra_headers=None):
    headers = {"Accept": "application/json"}
    if body is not None:
        body = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    if bearer is not None:
        headers["Authorization"] = f"Bearer {bearer}"
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(
        f"https://{HOST}:{PORT}{path}",
        data=body,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(req, context=tls_context(), timeout=10) as response:
            data = response.read()
            return response.status, dict(response.headers), json.loads(data) if data else None
    except urllib.error.HTTPError as exc:
        data = exc.read()
        return exc.code, dict(exc.headers), json.loads(data) if data else None


def mcp_transport(access_token: str):
    http_client = httpx2.AsyncClient(
        headers={"Authorization": f"Bearer {access_token}"},
        verify=tls_context(),
        timeout=httpx2.Timeout(30.0, read=300.0),
    )
    return http_client, streamable_http_client(
        MCP_URL,
        http_client=http_client,
        terminate_on_close=False,
    )


def prove_linking_signals():
    status, _, metadata = raw_request(
        "GET", "/.well-known/oauth-protected-resource/mcp"
    )
    assert status == 200, (status, metadata)
    assert metadata["resource"] == RESOURCE, metadata
    assert metadata["authorization_servers"] == ["http://127.0.0.1:8765"], metadata
    assert set(metadata["scopes_supported"]) == {"graph:read", "graph:write"}, metadata

    status, _, discovery = raw_request(
        "POST",
        "/mcp",
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {},
        },
    )
    assert status == 200, (status, discovery)
    assert "2026-07-28" in discovery["result"]["supportedVersions"], discovery

    status, _, listed = raw_request(
        "POST",
        "/mcp",
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        },
        extra_headers={"MCP-Protocol-Version": "2026-07-28"},
    )
    assert status == 200, (status, listed)
    tools = {tool["name"]: tool for tool in listed["result"]["tools"]}
    assert tools["read_graph"]["securitySchemes"] == [
        {"type": "oauth2", "scopes": ["graph:read"]}
    ], tools["read_graph"]
    assert tools["write_graph"]["securitySchemes"] == [
        {"type": "oauth2", "scopes": ["graph:write"]}
    ], tools["write_graph"]
    assert tools["delete_graph"]["securitySchemes"] == [
        {"type": "oauth2", "scopes": ["graph:write"]}
    ], tools["delete_graph"]

    status, _, challenged = raw_request(
        "POST",
        "/mcp",
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "read_graph",
                "arguments": {
                    "graph": GRAPH,
                    "cypher": "RETURN 1",
                },
            },
        },
        extra_headers={"MCP-Protocol-Version": "2026-07-28"},
    )
    assert status == 200, (status, challenged)
    result = challenged["result"]
    assert result["isError"] is True, result
    challenges = result["_meta"]["mcp/www_authenticate"]
    assert len(challenges) == 1, challenges
    challenge = challenges[0]
    assert "resource_metadata=" in challenge, challenge
    assert 'scope="graph:read"' in challenge, challenge
    assert 'error="insufficient_scope"' in challenge, challenge
    assert 'error_description=' in challenge, challenge

    for bad in ("wrong-audience.token", "expired.token"):
        status, headers, body = raw_request(
            "GET", "/v1/graphs", bearer=token(bad)
        )
        assert status == 401, (bad, status, body)
        assert "WWW-Authenticate" in headers, (bad, headers)
        assert "resource_metadata=" in headers["WWW-Authenticate"], headers


async def write_phase():
    prove_linking_signals()

    write_client, write_transport = mcp_transport(token("write.token"))
    async with write_client:
        async with Client(write_transport) as client:
            created = await client.call_tool(
                "write_graph",
                {
                    "graph": GRAPH,
                    "cypher": (
                        "CREATE (p:Project {name:'T6',auth:'oauth'}), "
                        "(t:Task {name:'secure MCP',state:'IMPLEMENTED'}), "
                        "(p)-[:HAS_TASK]->(t) "
                        "RETURN p.name,p.auth,t.name,t.state"
                    ),
                },
            )
            assert not created.is_error, created
            assert created.structured_content["rows"] == [
                ["T6", "oauth", "secure MCP", "IMPLEMENTED"]
            ], created.structured_content

            batch = await client.call_tool(
                "batch_graph",
                {
                    "queries": [
                        {
                            "graph": GRAPH,
                            "cypher": "MATCH (p:Project) RETURN p.name,p.auth",
                            "read_only": True,
                        },
                        {
                            "graph": GRAPH,
                            "cypher": (
                                "MATCH (t:Task {name:'secure MCP'}) "
                                "SET t.state='VERIFIED' RETURN t.name,t.state"
                            ),
                            "read_only": False,
                        },
                    ]
                },
            )
            assert not batch.is_error, batch
            assert batch.structured_content["results"][1]["result"]["rows"] == [
                ["secure MCP", "VERIFIED"]
            ], batch.structured_content

    read_client, read_transport = mcp_transport(token("read.token"))
    async with read_client:
        async with Client(read_transport) as client:
            read = await client.call_tool(
                "read_graph",
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project)-[:HAS_TASK]->(t:Task) "
                        "RETURN p.name,p.auth,t.name,t.state"
                    ),
                },
            )
            assert not read.is_error, read
            assert read.structured_content["rows"] == [
                ["T6", "oauth", "secure MCP", "VERIFIED"]
            ], read.structured_content

            denied = await client.call_tool(
                "write_graph",
                {
                    "graph": GRAPH,
                    "cypher": "CREATE (:ShouldNotExist)",
                },
            )
            assert denied.is_error, denied

    print("CHATGPT_OAUTH_MCP_WRITE_PASS")


async def read_phase():
    read_client, read_transport = mcp_transport(token("read.token"))
    async with read_client:
        async with Client(read_transport) as client:
            result = await client.call_tool(
                "read_graph",
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project)-[:HAS_TASK]->(t:Task) "
                        "RETURN p.name,p.auth,t.name,t.state"
                    ),
                },
            )
            assert not result.is_error, result
            assert result.structured_content["rows"] == [
                ["T6", "oauth", "secure MCP", "VERIFIED"]
            ], result.structured_content

    print("CHATGPT_OAUTH_MCP_RESTART_PASS")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    args = parser.parse_args()
    if args.phase == "write":
        anyio.run(write_phase)
    else:
        anyio.run(read_phase)
