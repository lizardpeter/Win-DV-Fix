# Architecture

## Standalone stack

```text
Reverse-engineering applications
        |
        v
FalkorDB Native Host (Rust)
  - graph catalog/lifecycle
  - query entrypoint
  - writer escalation / MVCC integration
  - WAL / recovery
  - native index implementation
        |
        v
FalkorDB graph crate
  - Cypher parser
  - planner / optimizer
  - runtime
  - MVCC graph
  - effects
  - algorithms
        |
        v
GraphBLAS / LAGraph
```

There is no Redis or RediSearch process in this path.

## Persistence

Durability ordering is:

```text
execute against private MVCC version
        -> serialize FalkorDB EffectsBuffer
        -> append framed WAL record
        -> flush/fsync
        -> publish committed MVCC graph
```

Recovery replays deterministic FalkorDB effects rather than re-running the
original Cypher text.

Each WAL frame contains versioning, sequence information, graph identity,
payload length, checksum, and the FalkorDB effects payload. Torn final frames
may be repaired; complete corrupted frames must fail loudly.

## Indexing

The standalone backend preserves FalkorDB's planner/runtime index contract while
removing the RediSearch FFI implementation.

Verified categories:
- numeric range/equality
- strings
- node and relationship indexes
- full text
- vectors
- index update/delete maintenance
- index recovery through WAL replay

The correctness-first native implementation can later be optimized beneath the
same API without changing Cypher or planner behavior.

## Reverse-engineering graph storage model

Hot graph state remains memory-resident for low-latency graph traversal.

Large immutable artifacts belong in CAS/object storage:
- binaries
- extracted sections
- assembly dumps
- decompiler output blobs
- shaders
- textures
- models
- captures
- generated render artifacts

Graph nodes reference those artifacts by content hash, storage path/URI, size,
type, provenance, source build, and producing tool run.

## Why this is safe without Redis

Redis was the upstream host/container. The standalone build explicitly replaces
the services required by the graph engine instead of merely removing them.

The graph engine itself, including Cypher, planner/runtime, MVCC, effects, and
GraphBLAS integration, is directly hosted in Rust and has been exercised on a
native Windows runner.
