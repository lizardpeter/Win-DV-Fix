import argparse
import os
import pathlib
import ssl

import anyio
import httpx2
from mcp import Client
from mcp.client.streamable_http import streamable_http_client

HOST = "localhost"
PORT = 8443
URL = f"https://{HOST}:{PORT}/mcp"
WRITE_TOKEN = "native-api-write-secret"
READ_TOKEN = "native-api-read-secret"
GRAPH = "chatgpt-mcp-smoke"

TLS_DIR = pathlib.Path(os.environ["FALKORDB_TEST_TLS_DIR"]).resolve()
CA = TLS_DIR / "ca.pem"


def tls_context():
    return ssl.create_default_context(cafile=str(CA))


def transport_for(token: str):
    http_client = httpx2.AsyncClient(
        headers={"Authorization": f"Bearer {token}"},
        verify=tls_context(),
        timeout=httpx2.Timeout(30.0, read=300.0),
    )
    transport = streamable_http_client(
        URL,
        http_client=http_client,
        terminate_on_close=False,
    )
    return http_client, transport


async def modern_write_phase():
    http_client, transport = transport_for(WRITE_TOKEN)
    async with http_client:
        async with Client(transport) as client:
            assert str(client.protocol_version) == "2026-07-28", client.protocol_version

            tools_result = await client.list_tools()
            names = {tool.name for tool in tools_result.tools}
            assert {
                "list_graphs",
                "read_graph",
                "write_graph",
                "batch_graph",
                "delete_graph",
            }.issubset(names), names

            result = await client.call_tool(
                "write_graph",
                {
                    "graph": GRAPH,
                    "cypher": (
                        "CREATE (p:Project {name:'T6',source:'mcp'}), "
                        "(t:Task {name:'reverse shaders',state:'DISCOVERED'}), "
                        "(p)-[:HAS_TASK]->(t) "
                        "RETURN p.name,t.name,t.state"
                    ),
                },
            )
            assert not result.is_error, result
            assert result.structured_content["rows"] == [
                ["T6", "reverse shaders", "DISCOVERED"]
            ], result.structured_content

            result = await client.call_tool(
                "read_graph",
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project)-[:HAS_TASK]->(t:Task) "
                        "RETURN p.name,p.source,t.name,t.state"
                    ),
                },
            )
            assert not result.is_error, result
            assert result.structured_content["rows"] == [
                ["T6", "mcp", "reverse shaders", "DISCOVERED"]
            ], result.structured_content

            batch = await client.call_tool(
                "batch_graph",
                {
                    "queries": [
                        {
                            "graph": GRAPH,
                            "cypher": "MATCH (p:Project) RETURN p.name",
                            "read_only": True,
                        },
                        {
                            "graph": GRAPH,
                            "cypher": (
                                "MATCH (t:Task {name:'reverse shaders'}) "
                                "SET t.state='UNDERSTOOD' RETURN t.name,t.state"
                            ),
                            "read_only": False,
                        },
                    ]
                },
            )
            assert not batch.is_error, batch
            results = batch.structured_content["results"]
            assert results[0]["ok"] is True, results
            assert results[0]["result"]["rows"] == [["T6"]], results
            assert results[1]["ok"] is True, results
            assert results[1]["result"]["rows"] == [
                ["reverse shaders", "UNDERSTOOD"]
            ], results

            graphs = await client.call_tool("list_graphs", {})
            assert not graphs.is_error, graphs
            assert GRAPH in graphs.structured_content["graphs"], graphs.structured_content

    print("CHATGPT_MCP_MODERN_WRITE_PASS")


async def legacy_read_scope_phase():
    http_client, transport = transport_for(READ_TOKEN)
    async with http_client:
        async with Client(transport, mode="legacy") as client:
            assert str(client.protocol_version) == "2025-11-25", client.protocol_version

            tools_result = await client.list_tools()
            names = {tool.name for tool in tools_result.tools}
            assert "list_graphs" in names, names
            assert "read_graph" in names, names
            assert "batch_graph" in names, names
            assert "write_graph" not in names, names
            assert "delete_graph" not in names, names

            read = await client.call_tool(
                "read_graph",
                {
                    "graph": GRAPH,
                    "cypher": "MATCH (t:Task) RETURN t.name,t.state",
                },
            )
            assert not read.is_error, read
            assert read.structured_content["rows"] == [
                ["reverse shaders", "UNDERSTOOD"]
            ], read.structured_content

            batch = await client.call_tool(
                "batch_graph",
                {
                    "queries": [
                        {
                            "graph": GRAPH,
                            "cypher": "MATCH (p:Project) RETURN p.name,p.source",
                        },
                        {
                            "graph": GRAPH,
                            "cypher": "MATCH (t:Task) RETURN t.name,t.state",
                        },
                    ]
                },
            )
            assert not batch.is_error, batch
            results = batch.structured_content["results"]
            assert results[0]["result"]["rows"] == [["T6", "mcp"]], results
            assert results[1]["result"]["rows"] == [
                ["reverse shaders", "UNDERSTOOD"]
            ], results

    print("CHATGPT_MCP_LEGACY_READ_SCOPE_PASS")


async def phase_write():
    await modern_write_phase()
    await legacy_read_scope_phase()
    print("CHATGPT_MCP_WRITE_PASS")


async def phase_read():
    http_client, transport = transport_for(READ_TOKEN)
    async with http_client:
        async with Client(transport) as client:
            assert str(client.protocol_version) == "2026-07-28", client.protocol_version
            result = await client.call_tool(
                "read_graph",
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project)-[:HAS_TASK]->(t:Task) "
                        "RETURN p.name,p.source,t.name,t.state"
                    ),
                },
            )
            assert not result.is_error, result
            assert result.structured_content["rows"] == [
                ["T6", "mcp", "reverse shaders", "UNDERSTOOD"]
            ], result.structured_content

    print("CHATGPT_MCP_RESTART_PASS")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    args = parser.parse_args()
    if args.phase == "write":
        anyio.run(phase_write)
    else:
        anyio.run(phase_read)
