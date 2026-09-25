# Network Deployment

Verified baseline:

- Windows CI run: `36190878880`
- code commit: `3989ee1c8ec252bacd7e24fb51416cf7498c1874`
- final marker: `NATIVE_WINDOWS_NETWORK_FALKORDB_CLIENT_PASS`
- tested client: official `FalkorDB/falkordb-py`

## Server

The standalone executable accepts FalkorDB-style commands over TCP/RESP and
routes each graph name to a persistent native graph.

Example:

```powershell
$env:FALKORDB_PASSWORD = "use-a-long-random-secret"
.\server.exe --bind 0.0.0.0:6379 --data-dir D:\FalkorDBNative\data
```

Optional username:

```powershell
$env:FALKORDB_USERNAME = "graphuser"
$env:FALKORDB_PASSWORD = "use-a-long-random-secret"
.\server.exe --bind 0.0.0.0:6379 --data-dir D:\FalkorDBNative\data
```

The default username is `default`.

Each graph is persisted beneath the data directory with an encoded graph-name
WAL filename. On restart the graph catalog discovers those files automatically.

## Official Python client

```python
from falkordb import FalkorDB

db = FalkorDB(
    host="server.example.internal",
    port=6379,
    username="default",
    password="use-a-long-random-secret",
    protocol=2,
)

graph = db.select_graph("reversal")
graph.query("CREATE (:Project {name:'T6'})")
rows = graph.query("MATCH (n:Project) RETURN n.name").result_set
print(rows)
```

The CI proof also verifies that compact wire results decode into the official
client's real `Node` and `Edge` objects, including labels, relationship
types, and properties.

## Verified restart behavior

The integration test:

1. starts the native Windows server;
2. proves a wrong password is rejected;
3. connects with the official FalkorDB client;
4. creates nodes, relationships, a range index, and a full-text index;
5. runs indexed queries remotely;
6. terminates the server process;
7. starts a new server process against the same data directory;
8. reconnects with the official client;
9. verifies graph data and indexed queries after WAL replay.

A separate regression also covers indexes whose CREATE INDEX DDL occurs after
the indexed data already existed.

## Network security

Redis RESP AUTH is authentication, not transport encryption. The current
verified server therefore should be exposed only on a trusted/private network,
through a VPN such as WireGuard/Tailscale, or behind TLS termination.

The server refuses a non-loopback unauthenticated bind unless the explicit
`--allow-unauthenticated-remote` override is supplied. Do not use that
override on an untrusted network.

## Current compatibility scope

The verified compatibility surface is sufficient for normal remote graph
ingestion/query work:

- AUTH
- PING / INFO server
- GRAPH.QUERY
- GRAPH.RO_QUERY
- GRAPH.LIST
- GRAPH.DELETE
- range/full-text index DDL through Cypher
- typed compact result sets
- FLUSHDB / FLUSHALL
- persistent multi-graph catalog

This is not a claim that every Redis administrative command or every optional
FalkorDB command is implemented. Add additional commands to the compatibility
layer only when a real client/workflow requires them.
