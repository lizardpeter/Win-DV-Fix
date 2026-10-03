# Secure FalkorDB Browser viewer

This project supports a dedicated read-only RESP account intended for
FalkorDB Browser and other visualization clients.

## Security model

Recommended deployment:

1. Keep the native database listener private, ideally on `127.0.0.1` or a
   private network.
2. Configure an admin credential for agents/tools and a separate viewer
   credential for the web UI.
3. Connect FalkorDB Browser with the viewer account.
4. Expose only the Browser over HTTPS. Do not expose the RESP port to the
   public Internet solely for visualization.
5. Prefer manual Browser login. Do not enable Browser auto-connect unless the
   Browser itself is protected by an outer authentication layer.

The viewer account is enforced by the database server, not only by UI hiding.
A viewer connection is denied all write/admin commands. Browser role detection
is supported intentionally:

- `ACL GETUSER` -> `NOPERM` for the viewer
- `GRAPH.QUERY` -> `NOPERM` for the viewer
- `GRAPH.RO_QUERY` -> allowed

This makes FalkorDB Browser identify the account as Read-Only and switch its
query path accordingly.

## Viewer-compatible commands

The viewer account permits the command surface needed for normal Browser
inspection:

- `PING`, `HELLO`, `INFO`, `CLIENT`, `COMMAND`, `SELECT`, `ECHO`
- `MODULE LIST`
- `GRAPH.LIST`
- `GRAPH.RO_QUERY`
- `GRAPH.EXPLAIN`
- `GRAPH.INFO`
- `GRAPH.MEMORY`
- `DUMP`, `EXISTS`, `TYPE`
- `TTL`, `PTTL`, `EXPIRETIME`

All other commands are denied for the viewer role. In particular, the viewer
cannot execute `GRAPH.QUERY`, `GRAPH.DELETE`, `GRAPH.BULK`, `RESTORE`,
`DEL`, `FLUSHDB`, constraints, UDF mutations, or configuration changes.

## Configuration

Environment:

```text
FALKORDB_USERNAME=default
FALKORDB_PASSWORD=<admin-secret>

FALKORDB_VIEWER_USERNAME=viewer
FALKORDB_VIEWER_PASSWORD=<separate-viewer-secret>
```

The viewer password may also be placed in `falkordb-secrets.txt`:

```text
FALKORDB_PASSWORD=<admin-secret>
FALKORDB_VIEWER_PASSWORD=<viewer-secret>
FALKORDB_API_TOKEN=<agent-read-write-token>
FALKORDB_API_READ_TOKEN=<agent-read-only-token>
```

Equivalent CLI options:

```text
--viewer-username viewer
--viewer-password <viewer-secret>
```

## FalkorDB Browser

Use the official FalkorDB Browser project rather than rebuilding graph
visualization from scratch. It provides the graph canvas, query editor,
metadata/schema/table views, history, styling, and graph navigation.

For a local Browser instance, connect using:

```text
host:      127.0.0.1
port:      6379
username:  viewer
password:  <viewer-secret>
```

When TLS is used, configure the Browser with the server CA and the TLS-enabled
FalkorDB endpoint.

For remote viewing, terminate HTTPS at the Browser/reverse proxy and keep the
database listener private. A VPN/private overlay or an authenticated reverse
proxy is preferred over exposing the database listener.

## Large graphs

Do not attempt to render an entire multi-hundred-thousand-node graph at once.
Visualization should remain query-driven: select a project, function,
neighborhood, call chain, evidence cluster, or bounded subgraph and render that
result.

The FalkorDB Canvas component supports force/tree/radial layouts and large-graph
viewport culling, so it is also suitable for a future embedded dashboard if a
lighter single-purpose viewer is desired.
