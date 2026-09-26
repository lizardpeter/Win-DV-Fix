# FalkorDB Native Windows Standalone Port

This directory contains the native Windows standalone FalkorDB-derived graph backend used for the reverse-engineering graph work.

## Status

**Verified working on native Windows/MSVC without Redis, Memurai, WSL, Docker, Hyper-V, or a VM.**

Current verified Windows CI evidence:

- Workflow run: `36198470377`
- Verified network-capable commit: `833eb680b2335feca473e448a8f845ce699e6100`
- Standalone marker: `NATIVE_WINDOWS_FULL_STANDALONE_PASS`
- Official-client write marker: `OFFICIAL_FALKORDB_CLIENT_WRITE_PASS`
- Official-client restart marker: `OFFICIAL_FALKORDB_CLIENT_RESTART_PASS`
- Final network marker: `NATIVE_WINDOWS_NETWORK_FALKORDB_CLIENT_PASS`
- mTLS marker: `NATIVE_WINDOWS_MTLS_FALKORDB_CLIENT_PASS`
- ChatGPT HTTPS API marker: `NATIVE_WINDOWS_CHATGPT_HTTPS_API_PASS`

The successful run compiled the FalkorDB `graph` crate and the native host with
`FALKORDB_SKIP_REDISEARCH=1`, passed the standalone Cypher/index/WAL suite,
started the RESP/TCP server with password authentication, connected with the
unmodified official `falkordb-py` client, created and queried graph/index data,
terminated and restarted the server process, and verified the same data and
indexes again through the official client.

## What Redis used to provide

Upstream FalkorDB is normally distributed as a Redis module, but the actual
graph engine lives in the Rust `graph` crate. The standalone port hosts that
engine directly.

Redis/Redis-module responsibilities are replaced as follows:

| Upstream host responsibility | Standalone replacement |
|---|---|
| Redis module command hosting | native Rust host |
| Redis graph lifecycle | `NativeGraph` / graph catalog |
| RediSearch | pure-Rust native index backend |
| Redis replication/persistence integration | native effects WAL + replay |
| Redis fork/BGSAVE behavior | native restart/recovery path |
| Redis server process | not required |

The Cypher parser, planner, runtime, MVCC graph engine, effects machinery,
GraphBLAS-backed graph representation, and graph algorithms remain FalkorDB
code.

## Verified capabilities

The Windows integration suite proves all of the following:

- basic Cypher execution
- node CREATE/MATCH/update/delete
- relationship CREATE/MATCH/update/delete
- numeric range indexes
- string/equality indexes
- Cypher-visible indexed WHERE predicates
- full-text node indexes
- full-text relationship indexes
- vector node indexes
- vector relationship indexes
- indexed updates
- indexed deletion cleanup
- WAL write/replay
- index DDL replay
- persistent graph reopen/restart
- range index query after restart
- full-text query after restart

The Stage 6 integration test creates range, full-text, and vector indexes on
both nodes and relationships, mutates indexed properties, deletes indexed
entities, then opens a persistent graph twice and verifies indexed data after
WAL recovery.

## Important verification markers

`diagnose.ps1` requires:

- `NATIVE_SMOKE_OK`
- `NATIVE_CYPHER_INDEX_INTEGRATION_PASS`
- `NATIVE_WAL_INDEX_RESTART_PASS`

Only after all required stages pass does it print:

`NATIVE_WINDOWS_FULL_STANDALONE_PASS`

## Layout

- `native_host/` - standalone Rust host and persistence/index tests
- `native_host/src/bin/smoke.rs` - core Cypher smoke executable
- `native_host/src/bin/indexed_smoke.rs` - native index + WAL/restart integration
- `native_host/src/wal.rs` - durable effects WAL
- `patches/native_index_mod.rs` - pure-Rust FalkorDB index backend replacement
- `scripts/bootstrap_windows.ps1` - pinned Windows dependency/bootstrap build
- `scripts/apply_windows_foundation.py` - FalkorDB source portability/index patches
- `scripts/diagnose.ps1` - authoritative compile/link/test runner
- `.github/workflows/falkordb-native-win-ci.yml` - Windows CI entrypoint

## Running on Windows

From the repository root on a Windows x64 machine with the Visual Studio C++
toolchain, CMake, Python, Git, and Rust installed:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\ci\falkordb-native-win\scripts\bootstrap_windows.ps1
.\ci\falkordb-native-win\scripts\diagnose.ps1
```

A valid end-to-end run must finish with:

```text
NATIVE_WINDOWS_FULL_STANDALONE_PASS
```


## Network server

The standalone host now includes a Redis RESP-compatible TCP server so normal
FalkorDB clients can connect without a Redis or FalkorDB server process.

Verified network behavior includes:

- RESP2 command transport over TCP
- Redis-style `AUTH <password>` and `AUTH <username> <password>`
- `INFO server` reporting standalone mode for client discovery
- `GRAPH.QUERY`
- `GRAPH.RO_QUERY`
- `GRAPH.LIST`
- `GRAPH.DELETE`
- `FLUSHDB` / `FLUSHALL`
- compact typed FalkorDB result encoding
- real Node and Edge decoding by the official Python client
- graph-name routing to separate persistent WALs
- range and full-text index queries over the network
- restart/recovery while preserving remotely created graph and index state

Build the tested server with the normal Windows bootstrap/diagnostic path. The
server executable is `server.exe` in the Cargo release target directory.

Example remote-server launch:

```powershell
$env:FALKORDB_PASSWORD = "replace-with-a-long-random-secret"
.\server.exe --bind 0.0.0.0:6379 --data-dir D:\FalkorDBNative\data
```

Example official Python client:

```python
from falkordb import FalkorDB

db = FalkorDB(
    host="SERVER_IP_OR_DNS",
    port=6379,
    password="replace-with-a-long-random-secret",
)
graph = db.select_graph("reversal")
graph.query("CREATE (:Project {name:'T6'})")
print(graph.query("MATCH (n:Project) RETURN n.name").result_set)
```

The server refuses an unauthenticated non-loopback bind by default. Remote deployment now supports TLS directly, including mutual TLS for the FalkorDB/RESP listener. The verified CI path requires a trusted client certificate plus Redis-style AUTH. A separate HTTPS JSON API with Bearer-token read-only/read-write scopes is available for ChatGPT/tool access.

## Migrating an existing FalkorDB database

The preferred migration path preserves FalkorDB's binary graph serialization.
It does **not** recreate nodes and relationships row by row. Current FalkorDB
`graphdata` v19 Redis `DUMP` payloads are decoded directly, validated before
replacement, converted into native checkpoints/WAL, and then verified.

### Live source -> native Windows server

Both endpoints may use password authentication and TLS/mTLS.

```powershell
python .\scripts\migrate_current_falkordb.py `
  --source-host OLD_FALKORDB_HOST --source-port 6379 `
  --source-password "source-secret" `
  --destination-host NEW_WINDOWS_HOST --destination-port 6379 `
  --destination-password "destination-secret"
```

The utility:

- enumerates every graph with `GRAPH.LIST`
- obtains the exact Redis `DUMP` bytes for each graph
- retries if the source graph changes while its semantic signature is captured
- restores into the native server
- compares node/relationship counts, labels, relationship types, property keys,
  index definitions, and constraints
- migrates process-global UDF libraries and verifies their source
- backs up and rolls back replaced destination graphs/UDFs on failure

Use `--replace` only when existing destination graphs/libraries should be
replaced. Verification is on by default.

### Offline portable archive

When source and destination cannot be online at the same time, export one
portable archive on the source side:

```powershell
python .\scripts\falkordb_portable_bundle.py export `
  --source-host OLD_FALKORDB_HOST --source-port 6379 `
  --source-password "source-secret" `
  --bundle D:\Transfer\database.falkor.zip
```

Move that single file to the Windows host, then import it:

```powershell
python .\scripts\falkordb_portable_bundle.py import `
  --destination-host 127.0.0.1 --destination-port 6379 `
  --destination-password "destination-secret" `
  --bundle D:\Transfer\database.falkor.zip
```

Bundle v1 stores each graph as its exact Redis `DUMP` payload plus a manifest
containing SHA-256 hashes, semantic graph signatures, and UDF library source.
Import validates every payload before mutation and rolls back already imported
graphs/libraries if a later item fails.

A standalone current FalkorDB endpoint is required for export/live migration.
For an old on-disk `dump.rdb`, load it with a compatible current FalkorDB
instance first; that instance will decode supported legacy graph encodings and
emit current v19 `DUMP` payloads for migration.

## Design direction for the reversal graph

This backend is intended to host the universal reversal graph for T6, Destiny,
Xbox 360 Avatar, renderer/asset work, and future reverse-engineering projects.

Large binaries/assets should not be stored directly as graph properties.
Store immutable large artifacts in a content-addressed store and keep
hash/path/type/provenance in the graph.

The graph should track entities such as projects, builds, modules, functions,
basic blocks, instructions, symbols, types, assets, shaders, materials,
textures, models, animations, evidence, tool runs, tests, tasks, and commits.

Completion is evidence-based: discovered -> extracted -> parsed -> understood
-> implemented -> verified.

## Next work

The standalone database is proven. Remaining work is productization and
performance work, including:

- optimized native range index hot paths
- snapshot compaction in addition to WAL replay
- graph catalog/multi-project lifecycle (network graph-name routing is complete; schema/project conventions remain)
- CAS artifact store integration
- observability/progress metrics
- packaging/install/update flow
- optional native TLS termination (private-network/VPN deployment is supported now)
- benchmarking against the original FalkorDB/RediSearch deployment
- stress/crash/fuzz testing


## ChatGPT HTTPS API

The native server also exposes an authenticated HTTPS JSON API intended for ChatGPT/tool access without requiring a Redis protocol client.

Verified behavior includes:

- TLS
- Bearer authentication
- separate read-only and read-write tokens
- authorization before graph lookup for write attempts
- `GET /healthz`
- `GET /openapi.json`
- `GET /v1/capabilities`
- `GET /v1/graphs`
- `POST /v1/query`
- `POST /v1/batch`
- `POST /v1/graphs/delete`
- hard server-process restart followed by WAL-backed query recovery

The same Windows CI run proves both the official FalkorDB client over mTLS and the HTTPS API against the same persistent graph backend.
