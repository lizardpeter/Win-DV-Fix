import base64
import hashlib
import json
import os
import pathlib
import ssl
import urllib.error
import urllib.parse
import urllib.request

HOST = "localhost"
PORT = 8443
BASE = f"https://{HOST}:{PORT}"
ADMIN_SECRET = "native-ci-secret"
WRITE_TOKEN = "native-api-write-secret"
READ_TOKEN = "native-api-read-secret"
GRAPH = "mcp-oauth-smoke"
COPY_GRAPH = "mcp-oauth-smoke-copy"
REDIRECT_URI = "https://chatgpt.com/connector/oauth/test-callback"

TLS_DIR = pathlib.Path(os.environ["FALKORDB_TEST_TLS_DIR"]).resolve()
CA = TLS_DIR / "ca.pem"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def context():
    return ssl.create_default_context(cafile=str(CA))


def opener(no_redirect=False):
    handlers = [urllib.request.HTTPSHandler(context=context())]
    if no_redirect:
        handlers.insert(0, NoRedirect())
    return urllib.request.build_opener(*handlers)


def raw_request(method, path, body=None, headers=None, no_redirect=False):
    headers = dict(headers or {})
    req = urllib.request.Request(
        BASE + path,
        data=body,
        headers=headers,
        method=method,
    )
    try:
        with opener(no_redirect=no_redirect).open(req, timeout=10) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as exc:
        return exc.code, dict(exc.headers), exc.read()


def json_request(method, path, payload=None, token=None):
    body = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    status, response_headers, raw = raw_request(
        method, path, body=body, headers=headers
    )
    decoded = json.loads(raw) if raw else None
    return status, response_headers, decoded


def form_request(method, path, values, no_redirect=False):
    body = urllib.parse.urlencode(values).encode("utf-8")
    return raw_request(
        method,
        path,
        body=body,
        headers={
            "Accept": "application/json,text/html",
            "Content-Type": "application/x-www-form-urlencoded",
        },
        no_redirect=no_redirect,
    )


def mcp_call(token, method, params=None, request_id=1):
    payload = {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
    }
    if params is not None:
        payload["params"] = params
    status, _, body = json_request(
        "POST",
        "/mcp",
        payload,
        token=token,
    )
    assert status == 200, (status, body)
    assert body["jsonrpc"] == "2.0", body
    assert body["id"] == request_id, body
    return body


def tool_call(token, name, arguments, request_id):
    body = mcp_call(
        token,
        "tools/call",
        {"name": name, "arguments": arguments},
        request_id=request_id,
    )
    assert "result" in body, body
    return body["result"]


def get_oauth_token():
    # Protected-resource discovery is public and advertises the auth server.
    status, _, metadata = json_request(
        "GET", "/.well-known/oauth-protected-resource/mcp"
    )
    assert status == 200, (status, metadata)
    assert metadata["resource"] == BASE + "/mcp", metadata
    assert BASE in metadata["authorization_servers"], metadata

    # Unauthenticated MCP access must challenge with OAuth metadata.
    status, headers, body = json_request(
        "POST",
        "/mcp",
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcp-smoke", "version": "1"},
            },
        },
    )
    assert status == 401, (status, body)
    challenge = headers.get("WWW-Authenticate") or headers.get("Www-Authenticate")
    assert challenge and "oauth-protected-resource" in challenge, headers

    # Dynamic client registration, matching ChatGPT's private MCP flow.
    status, _, registration = json_request(
        "POST",
        "/oauth/register",
        {
            "redirect_uris": [REDIRECT_URI],
            "token_endpoint_auth_method": "none",
        },
    )
    assert status == 201, (status, registration)
    client_id = registration["client_id"]

    verifier = (
        "mcp-smoke-pkce-verifier-"
        + "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    )[:96]
    challenge = base64.urlsafe_b64encode(
        hashlib.sha256(verifier.encode("ascii")).digest()
    ).rstrip(b"=").decode("ascii")

    auth_values = {
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": REDIRECT_URI,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "scope": "graph.read graph.write graph.admin",
        "state": "mcp-smoke-state",
        "resource": BASE + "/mcp",
    }

    status, _, html = raw_request(
        "GET",
        "/oauth/authorize?" + urllib.parse.urlencode(auth_values),
        headers={"Accept": "text/html"},
    )
    assert status == 200, (status, html[:200])
    assert b"Authorize ChatGPT FalkorDB access" in html, html[:500]

    approval_values = dict(auth_values)
    approval_values["admin_secret"] = ADMIN_SECRET
    status, headers, _ = form_request(
        "POST",
        "/oauth/authorize",
        approval_values,
        no_redirect=True,
    )
    assert status == 302, (status, headers)
    location = headers.get("Location") or headers.get("location")
    assert location, headers
    redirect = urllib.parse.urlparse(location)
    params = urllib.parse.parse_qs(redirect.query)
    code = params["code"][0]
    assert params["state"][0] == "mcp-smoke-state"

    status, _, token_body = form_request(
        "POST",
        "/oauth/token",
        {
            "grant_type": "authorization_code",
            "client_id": client_id,
            "redirect_uri": REDIRECT_URI,
            "code": code,
            "code_verifier": verifier,
        },
    )
    assert status == 200, (status, token_body)
    token = json.loads(token_body)
    assert token["token_type"].lower() == "bearer", token
    assert token["scope"] == "graph.read graph.write graph.admin", token
    assert token["access_token"] == WRITE_TOKEN
    return token["access_token"]


def main():
    token = get_oauth_token()

    initialized = mcp_call(
        token,
        "initialize",
        {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "mcp-smoke", "version": "1"},
        },
        request_id=10,
    )
    assert initialized["result"]["protocolVersion"] == "2025-06-18", initialized
    assert initialized["result"]["serverInfo"]["name"] == "houseofkublai-falkordb"

    tools = mcp_call(token, "tools/list", {}, request_id=11)["result"]["tools"]
    names = {tool["name"] for tool in tools}
    expected = {
        "list_graphs",
        "graph_info",
        "create_graph",
        "query_graph_read",
        "query_graph_write",
        "batch_queries",
        "explain_query",
        "copy_graph",
        "checkpoint_graph",
        "checkpoint_all",
        "delete_graph",
        "flush_database",
    }
    assert expected <= names, sorted(names)

    # Clean stale smoke graphs from a prior interrupted CI run.
    for graph in (COPY_GRAPH, GRAPH):
        result = tool_call(token, "delete_graph", {"graph": graph}, request_id=20)
        assert result["isError"] is False, result

    result = tool_call(token, "create_graph", {"graph": GRAPH}, request_id=21)
    assert result["isError"] is False, result

    result = tool_call(
        token,
        "query_graph_write",
        {
            "graph": GRAPH,
            "cypher": (
                "CREATE (p:Project {name:'T6',status:'ACTIVE'}), "
                "(a:Artifact {kind:'shader',hash:'abc123'}), "
                "(p)-[:CONTAINS]->(a) "
                "RETURN p.name,a.kind,a.hash"
            ),
        },
        request_id=22,
    )
    assert result["isError"] is False, result
    structured = result["structuredContent"]
    assert structured["rows"] == [["T6", "shader", "abc123"]], structured

    result = tool_call(
        token,
        "query_graph_read",
        {
            "graph": GRAPH,
            "cypher": (
                "MATCH (p:Project)-[:CONTAINS]->(a:Artifact) "
                "RETURN p.name,p.status,a.kind,a.hash"
            ),
        },
        request_id=23,
    )
    assert result["isError"] is False, result
    assert result["structuredContent"]["rows"] == [
        ["T6", "ACTIVE", "shader", "abc123"]
    ], result

    result = tool_call(
        token,
        "batch_queries",
        {
            "queries": [
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project {name:'T6'}) "
                        "CREATE (t:Task {name:'decode shaders',state:'ACTIVE'}), "
                        "(p)-[:HAS_TASK]->(t) RETURN t.name"
                    ),
                    "read_only": False,
                },
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project)-[:HAS_TASK]->(t:Task) "
                        "RETURN p.name,t.name,t.state"
                    ),
                    "read_only": True,
                },
            ]
        },
        request_id=24,
    )
    assert result["isError"] is False, result
    batch = result["structuredContent"]["results"]
    assert batch[0]["ok"] is True and batch[1]["ok"] is True, batch
    assert batch[1]["result"]["rows"] == [
        ["T6", "decode shaders", "ACTIVE"]
    ], batch

    result = tool_call(
        token,
        "explain_query",
        {
            "graph": GRAPH,
            "cypher": "MATCH (p:Project) RETURN p.name",
        },
        request_id=25,
    )
    assert result["isError"] is False, result
    assert result["structuredContent"]["plan"], result

    result = tool_call(
        token,
        "graph_info",
        {"graph": GRAPH},
        request_id=26,
    )
    assert result["isError"] is False, result
    assert result["structuredContent"]["wal_bytes"] >= 0, result

    result = tool_call(
        token,
        "copy_graph",
        {"source": GRAPH, "destination": COPY_GRAPH},
        request_id=27,
    )
    assert result["isError"] is False, result

    result = tool_call(
        token,
        "checkpoint_graph",
        {"graph": GRAPH},
        request_id=28,
    )
    assert result["isError"] is False, result

    result = tool_call(token, "checkpoint_all", {}, request_id=29)
    assert result["isError"] is False, result

    # The existing read-only credential may invoke read tools, but MCP itself
    # must reject full-control write tools under that scope.
    result = tool_call(
        READ_TOKEN,
        "query_graph_read",
        {
            "graph": GRAPH,
            "cypher": "MATCH (p:Project) RETURN p.name",
        },
        request_id=30,
    )
    assert result["isError"] is False, result

    result = tool_call(
        READ_TOKEN,
        "query_graph_write",
        {
            "graph": GRAPH,
            "cypher": "CREATE (:MustNotExist)",
        },
        request_id=31,
    )
    assert result["isError"] is True, result
    assert "write/admin authorization required" in result["content"][0]["text"], result

    for graph in (COPY_GRAPH, GRAPH):
        result = tool_call(token, "delete_graph", {"graph": graph}, request_id=32)
        assert result["isError"] is False, result
        assert result["structuredContent"]["deleted"] is True, result

    print("MCP_OAUTH_FULL_CONTROL_PASS")


if __name__ == "__main__":
    main()
