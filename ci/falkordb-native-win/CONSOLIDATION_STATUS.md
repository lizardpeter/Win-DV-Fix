# Consolidation status — 2026-10-01

## Canonical repository

This private repository is now the canonical home for the custom native FalkorDB / Graph MCP executable.

Source history being consolidated from `lizardpeter/Win-DV-Fix`:
- `chatgpt/falkordb-mcp-rdb-import` — main baseline for MCP + RDB import
- `chatgpt/falkordb-native-parity-next` — unique bulk engine + query scheduler work
- `chatgpt/falkordb-raw-rdb-import` — unique raw-RDB fixes to audit
- migration/index/recovery branches — remaining verified fixes to audit and preserve

## Already migrated into this repo

- Native Rust host Cargo project
- MCP implementation
- RDB-file import implementation
- Redis/FalkorDB DUMP/RESTORE implementation
- native server
- property/range index code
- snapshot/WAL persistence
- slowlog / UDF / wire layers
- build/bootstrap/package utilities
- Windows CI workflow
- generic upstream-compatible `GRAPH.BULK` decoder/engine from parity branch
- generic query scheduler from parity branch

## Integration work already completed here

- `bulk.rs` and `query_scheduler.rs` added to the newer RDB/MCP baseline
- staged checkpoint publication restored without replacing newer snapshot/RDB code
- replay-safe WAL checkpoint marker restored
- `NativeGraph::bulk_insert` wired into the newer host
- scheduler permits wired into normal query, read-only query, and profile paths
- `GRAPH.BULK` exposed by the newer native server

## Important architecture rule

The executable must remain graph-agnostic and schema-agnostic.

It must not know what SMG, T6, Destiny, uregraph, medgraph, neurosurgery, or any project ontology means.

Generic capabilities belong in the EXE:
- arbitrary named graphs
- raw Cypher
- schema/index/constraint introspection
- native full-graph RDB import
- generic high-throughput node/relationship file ingestion
- verification, dry-run, resumability, checkpointing, and import reports
- stable MCP capability/tool discovery

Schema and ontology rules belong in each graph/project.

## Consolidation gates

Completed in the canonical repository:

1. Audited the raw-RDB, migration-hardening, native-parity, static-CRT,
   networking, command-parity, portable-bundle, live-OAuth, and native-index
   branch families; the branch audit ledger below records their disposition.
2. Verified recovered-index readiness handling and restart coverage.
3. Preserved grouped `COUNT(property)` missing/null-property regression
   coverage.
4. Preserved current RDB allocation/count verification and Redis LZF
   extended-backreference decoding coverage.
5. Adapted CI/build/package scripts from the old
   `ci/falkordb-native-win` layout to this repository's root layout.
6. Added schema-agnostic server-local JSONL bulk ingestion with arbitrary
   Cypher shaping, dry-run/hash validation, batching/resume controls, reports,
   and optional checkpointing.
7. Exposed that ingestion through MCP beside `import_falkordb_rdb_file`.
8. Added explicit MCP server/toolset fingerprints; the final combined toolset
   uses `2026-10-01.4`.
9. Added exact database/storage visibility and the remaining audited runtime,
   migration, OAuth, and index improvements in PR #6.

Optional future import-format extensions, not release blockers, are CSV and
Parquet front ends over the same generic server-local ingestion contract.

The only remaining release gate is a green full Windows workflow on the exact
PR #6 head, followed by the deployable ZIP artifact from that run.

## Why this matters

The current SMG retained Ghidra corpus is already complete at 23,345/23,345 functions. The remaining graph projection should be one server-local import job rather than thousands of interactive MCP/Cypher payloads. The same generic mechanism must also work for unrelated schemas and future graphs.


## Branch audit ledger

The old `Win-DV-Fix` FalkorDB branches have been compared against the
consolidated baseline. Do not re-import entire old host trees; carry forward
only the explicitly unique behavior below.

- `chatgpt/falkordb-raw-rdb-import`: superseded by the newer MCP/RDB baseline.
  The canonical repo already has the later telemetry-stream parser, CRC path,
  fragment reassembly, and native RDB import surface.
- `chatgpt/falkordb-migration-hardening`: superseded by the newer migration,
  RDB, snapshot, and portable-path code.
- `chatgpt/falkordb-native-parity-next`: `bulk.rs` and
  `query_scheduler.rs` are byte-for-byte represented in the canonical repo;
  current snapshot/WAL code is newer.
- `chatgpt/falkordb-static-crt`: superseded; current bootstrap/diagnostics
  already build Rust and native dependencies with the static MSVC CRT.
- `chatgpt/falkordb-network-tls`: superseded; current server has the broader
  TLS/mTLS/auth implementation.
- `chatgpt/falkordb-command-parity`: unique `GRAPH.INFO` and
  `GRAPH.MEMORY USAGE` work is isolated in canonical PR #3, together with the
  new MCP `database_stats` persistent-size/status surface.
- `chatgpt/falkordb-native-index-perf`: unique B-tree postings and HNSW vector
  index work is isolated in canonical PR #2. The canonical PR also restores
  full-text prefix-query and generation-safe recreation fixes from
  `chatgpt/falkordb-native-index-dev`.
- `chatgpt/falkordb-portable-bundle`: unique whole-bundle rollback behavior is
  isolated in canonical PR #4 while preserving the newer bundle format and
  path-confinement code.
- `chatgpt/falkordb-live-oauth-probe`: unique MCP OPTIONS/CORS preflight and
  restart-scoped OAuth pairing-code behavior is isolated in canonical PR #5.
  Newer MCP tool discovery, RDB import, generic file import, and toolset
  fingerprinting remain authoritative.

Current release policy: merge these isolated PRs only after the full Windows
fixture/build/smoke/package workflow passes, then run the same workflow once on
the combined `main` branch before declaring the EXE release-ready.


## Final integration candidate

PR #6 (`chatgpt/final-consolidation`) combines the audited unique work into
one release candidate so overlapping MCP/server changes are validated together.
It includes:

- `GRAPH.INFO` and `GRAPH.MEMORY USAGE` parity.
- MCP `database_stats` with exact persistent data/WAL/checkpoint accounting,
  live node/relationship/version counts, orphan-storage visibility, and optional
  sampled in-memory usage.
- Restart-scoped OAuth pairing plus MCP OPTIONS/CORS preflight.
- Atomic portable bundle replacement with disk-spooled rollback baselines and
  one-graph-at-a-time payload memory.
- Optimized native range/full-text postings and USearch HNSW vector indexes,
  while retaining prefix full-text correctness and generation-safe rebuilds.
- Correct canonical-repository Windows packaging paths and a hardened Rust cache
  that never persists partially unpacked registry source trees from failed jobs.

Release status remains **pending** until the full Windows workflow for PR #6
passes upstream-fixture generation, native compile/link/unit/index smoke,
official-client parity, MCP/OAuth, migration and rollback tests, portable
packaging, PE dependency audit, and artifact upload on the exact PR head.
