# Native operations dashboard

The custom Windows server includes a small self-hosted operations dashboard at:

```text
https://<api-host>/dashboard
```

The dashboard is intentionally separate from FalkorDB Browser.

- **FalkorDB Browser** is the interactive graph/query/schema/canvas UI.
- **Native Operations Dashboard** is the storage, cardinality, memory, query,
  persistence, and server-health UI.

Using both gives a better operator experience than trying to force all
operational data into the graph canvas.

## Security

The HTML, CSS, and JavaScript shell may be loaded without authentication.
No graph or database information is embedded in those files.

Actual data is fetched from the authenticated read-only API endpoint:

```text
GET /v1/overview
```

For human use, the dashboard can authenticate with the same dedicated
`viewer` username/password used by FalkorDB Browser. The HTTPS API maps that
HTTP Basic credential to read-only scope only. A `FALKORDB_API_READ_TOKEN`
Bearer token remains available as an alternative.

The resulting Authorization header is stored in browser `sessionStorage`
only. It is not written to localStorage, cookies, the URL, or server-side
dashboard state, and it disappears when the browser tab/session ends.

For remote use:

1. Use HTTPS.
2. Use the dedicated read-only viewer credential for human access, or a
   dedicated `FALKORDB_API_READ_TOKEN`.
3. Keep the RESP/admin listener private.
4. Never reuse the admin database password for the viewer/dashboard login.

The page uses a restrictive Content Security Policy and loads no CDN,
third-party JavaScript, fonts, analytics, or remote assets.

## Information shown

The dashboard reports:

- graph count;
- total nodes and relationships across loaded graphs;
- exact database data-directory size;
- graph storage versus import staging versus other storage;
- attributed graph bytes versus unattributed/orphan residue;
- per-graph node and relationship counts;
- graph version and schema version;
- label count, relationship-type count, property-key count;
- index-definition count and constraint count;
- per-graph WAL size;
- checkpoint size/count and latest checkpoint metadata;
- per-graph persistent footprint;
- sampled in-memory size estimate;
- currently running queries;
- waiting/queued queries;
- bounded recent slow-query entries across graphs;
- query elapsed/waiting time;
- host package version;
- available CPU parallelism.

Query text returned to the page is capped to a bounded preview so an unusually
large Cypher payload cannot make the dashboard response unbounded. The overview
also redacts CYPHER parameter preambles and quoted string literal contents, and
does not expose slowlog parameter blobs. This keeps monitoring useful without
turning a read-only dashboard credential into a view of transient client
secrets.

## Refresh behavior

The page supports manual refresh and 5-second, 15-second, or 60-second
auto-refresh. The default is 15 seconds.

Memory size uses sampled graph-engine accounting and should be treated as an
estimate. Persistent WAL/checkpoint/data-directory sizes are exact filesystem
byte counts.

## Large graphs

The operations dashboard never attempts to serialize or render the whole graph.
It reads constant-time graph cardinalities and bounded operational metadata.
Graph visualization should remain query-driven in FalkorDB Browser.
