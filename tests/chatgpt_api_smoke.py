import argparse
import json
import pathlib
import ssl
import urllib.error
import urllib.request
import os

HOST = "localhost"
PORT = 8443
WRITE_TOKEN = "native-api-write-secret"
READ_TOKEN = "native-api-read-secret"
GRAPH = "chatgpt-api-smoke"

TLS_DIR = pathlib.Path(os.environ["FALKORDB_TEST_TLS_DIR"]).resolve()
CA = TLS_DIR / "ca.pem"


def context():
    return ssl.create_default_context(cafile=str(CA))


def request(method, path, body=None, token=None):
    data = None
    headers = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"

    req = urllib.request.Request(
        f"https://{HOST}:{PORT}{path}",
        data=data,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(req, context=context(), timeout=10) as response:
            raw = response.read()
            return response.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        return exc.code, json.loads(raw) if raw else None


def phase_write():
    status, health = request("GET", "/healthz")
    assert status == 200, (status, health)
    assert health["ok"] is True

    status, spec = request("GET", "/openapi.json")
    assert status == 200, (status, spec)
    assert spec["openapi"].startswith("3.")
    assert "queryGraph" == spec["paths"]["/v1/query"]["post"]["operationId"]

    status, body = request("GET", "/v1/graphs", token="wrong-token")
    assert status == 401, (status, body)

    # Read-only token can inspect but cannot write.
    status, body = request("GET", "/v1/graphs", token=READ_TOKEN)
    assert status == 200, (status, body)

    status, body = request(
        "POST",
        "/v1/query",
        {
            "graph": GRAPH,
            "cypher": "CREATE (:ShouldNotExist)",
            "read_only": False,
        },
        token=READ_TOKEN,
    )
    assert status == 403, (status, body)

    status, body = request(
        "POST",
        "/v1/query",
        {
            "graph": GRAPH,
            "cypher": (
                "CREATE (p:Project {name:'T6',status:'ACTIVE'}), "
                "(a:Artifact {kind:'binary',hash:'abc123'}), "
                "(p)-[:CONTAINS]->(a) "
                "RETURN p.name, a.kind, a.hash"
            ),
        },
        token=WRITE_TOKEN,
    )
    assert status == 200, (status, body)
    assert body["rows"] == [["T6", "binary", "abc123"]], body

    status, batch = request(
        "POST",
        "/v1/batch",
        {
            "queries": [
                {
                    "graph": GRAPH,
                    "cypher": "MATCH (p:Project) RETURN p.name, p.status",
                    "read_only": True,
                },
                {
                    "graph": GRAPH,
                    "cypher": (
                        "MATCH (p:Project {name:'T6'}) "
                        "CREATE (t:Task {name:'decode shaders',state:'DISCOVERED'}), "
                        "(p)-[:HAS_TASK]->(t) RETURN t.name"
                    ),
                },
            ]
        },
        token=WRITE_TOKEN,
    )
    assert status == 200, (status, batch)
    assert batch["results"][0]["status"] == 200, batch
    assert batch["results"][0]["body"]["rows"] == [["T6", "ACTIVE"]], batch
    assert batch["results"][1]["status"] == 200, batch
    assert batch["results"][1]["body"]["rows"] == [["decode shaders"]], batch

    # Read-only credential can query the existing graph.
    status, body = request(
        "POST",
        "/v1/query",
        {
            "graph": GRAPH,
            "cypher": "MATCH (p:Project)-[:HAS_TASK]->(t:Task) RETURN p.name,t.name,t.state",
            "read_only": True,
        },
        token=READ_TOKEN,
    )
    assert status == 200, (status, body)
    assert body["rows"] == [["T6", "decode shaders", "DISCOVERED"]], body

    print("CHATGPT_HTTPS_API_WRITE_PASS")


def phase_read():
    status, body = request(
        "POST",
        "/v1/query",
        {
            "graph": GRAPH,
            "cypher": (
                "MATCH (p:Project {name:'T6'})-[:CONTAINS]->(a:Artifact) "
                "RETURN p.status,a.kind,a.hash"
            ),
            "read_only": True,
        },
        token=READ_TOKEN,
    )
    assert status == 200, (status, body)
    assert body["rows"] == [["ACTIVE", "binary", "abc123"]], body

    status, body = request(
        "POST",
        "/v1/query",
        {
            "graph": GRAPH,
            "cypher": "MATCH (t:Task) RETURN t.name,t.state",
            "read_only": True,
        },
        token=READ_TOKEN,
    )
    assert status == 200, (status, body)
    assert body["rows"] == [["decode shaders", "DISCOVERED"]], body

    print("CHATGPT_HTTPS_API_RESTART_PASS")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    args = parser.parse_args()
    if args.phase == "write":
        phase_write()
    else:
        phase_read()
