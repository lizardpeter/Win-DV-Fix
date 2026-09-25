# FalkorDB Native Windows Standalone Port

This directory contains the native Windows standalone FalkorDB-derived graph backend used for the reverse-engineering graph work.

## Status

**Verified working on native Windows/MSVC without Redis, Memurai, WSL, Docker, Hyper-V, or a VM.**

Known-good Windows CI evidence:

- Workflow run: `36163782345`
- Verified commit: `d99c2761f6b9bbb6952ca54cf50732952587bb47`
- Final marker: `NATIVE_WINDOWS_FULL_STANDALONE_PASS`
- Current branch restored to the verified binding configuration at commit:
  `f3cb92e0ceeea8c13f0319e16300059cd63e31bd`

The successful run compiled the FalkorDB `graph` crate and the native host with
`FALKORDB_SKIP_REDISEARCH=1`, linked and executed the Windows binaries, then
passed the standalone Cypher/index/WAL restart integration suite.

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
- graph catalog/multi-project lifecycle
- CAS artifact store integration
- observability/progress metrics
- packaging/install/update flow
- benchmarking against the original FalkorDB/RediSearch deployment
- stress/crash/fuzz testing
