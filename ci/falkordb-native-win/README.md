# custom-falkor-db

Canonical private source repository for the custom native FalkorDB/Graph MCP executable.

This repository consolidates the Windows-native host, MCP surface, RDB import/restore tooling, index and recovery fixes, generic bulk-ingestion support, packaging, CI, and migration utilities that were previously spread across `lizardpeter/Win-DV-Fix` branches.

## Design rules

- The executable is graph-agnostic and schema-agnostic.
- Raw Cypher remains available.
- Named graphs remain isolated unless an explicit operation targets them.
- Bulk ingestion must support arbitrary node/relationship schemas and arbitrary graph names.
- Import paths must be idempotent, resumable where practical, verifiable, and checkpoint-friendly.
- No game-specific, medical-specific, or project-specific ontology logic belongs in the executable.

## Consolidation sources

Initial baseline: `lizardpeter/Win-DV-Fix` branch `chatgpt/falkordb-mcp-rdb-import`.
Additional unique work to merge: `chatgpt/falkordb-native-parity-next`, `chatgpt/falkordb-raw-rdb-import`, migration/index/recovery branches, and other verified FalkorDB fixes.


## Size and status visibility

The server exposes both persistent and in-memory size information. These are
separate metrics and should not be treated as interchangeable.

- MCP `database_stats` reports exact on-disk bytes for the complete data
  directory and, per graph, WAL bytes, checkpoint bytes/count, latest checkpoint
  size, persistent bytes, node/relationship counts, and graph version.
- `database_stats` can additionally report sampled estimated in-memory graph
  bytes; use `include_memory=false` when only storage/cardinality information
  is needed.
- Redis/FalkorDB-compatible clients can use
  `GRAPH.MEMORY USAGE <graph> [SAMPLES <count>]` for FalkorDB-style memory
  breakdowns and `GRAPH.INFO` for running/waiting query and object-pool status.

For large reverse-engineering graphs this makes growth observable without
exporting or serializing the database through MCP.


## Secure graph visualization

For interactive web inspection, use the official FalkorDB Browser with the
native server's dedicated read-only `viewer` account. The database enforces
the viewer restriction server-side; the web UI does not receive the admin
credential.

See [docs/SECURE_BROWSER.md](docs/SECURE_BROWSER.md) for the deployment model,
viewer credentials, Browser compatibility, and remote-access guidance.


## AI clients and agents

The same secured MCP endpoint can be used by ChatGPT/OpenAI, Claude/Anthropic,
and other remote MCP clients. Interactive OAuth is supported for the known
ChatGPT and Claude Code client identities, and scoped Bearer tokens are
available for generic MCP clients.

See [docs/AI_CLIENTS.md](docs/AI_CLIENTS.md) for connection examples, scopes,
and security guidance.


## Native operations dashboard

The HTTPS API also serves a self-contained read-only operations dashboard at
`/dashboard`. It shows graph cardinality, schema/index/constraint counts,
persistent/WAL/checkpoint storage, sampled memory, live query activity,
redacted slow-query history, and storage-accounting warnings without rendering
the entire graph.

Authenticate with the dedicated read-only viewer account or a read-only API
token. See [docs/DASHBOARD.md](docs/DASHBOARD.md).
