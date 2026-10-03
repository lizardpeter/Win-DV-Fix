# Performance and concurrency

## What is shared with FalkorDB

The native Windows host embeds the FalkorDB Rust graph engine from upstream
commit `55204c94bb6c8bc1684ada3d712a61f73f324067`.

As of 2026-10-01, upstream `main` is only two commits ahead of that pin and
both later commits are test/CI packaging changes, not graph-engine changes.
The planner, runtime, MVCC graph, matrix operations, and core query semantics
are therefore effectively current upstream FalkorDB code.

## Concurrency model

- Each named graph owns an independent `NativeGraph`, MVCC state, lock, WAL,
  checkpoint stream, and index state.
- Read queries execute against immutable MVCC snapshots and may run concurrently.
- Writes are serialized per graph.
- Writes to different graphs may run concurrently.
- The process has a global CPU admission limit based on logical CPU count.
- The graph-aware scheduler prevents multiple same-graph writes from consuming
  global worker slots while waiting for the same graph lock.
- A waiting writer receives priority over later readers on the same graph,
  preventing writer starvation.
- Under global saturation, queued work from graphs with fewer running queries is
  promoted first so one hot graph cannot monopolize every worker.

The persistent concurrency smoke opens multiple WAL-backed graphs, runs multiple
competing writers and readers against every graph simultaneously, verifies no
lost increments, closes every graph, reopens from persistence, and verifies the
same final values after recovery.

## Network model

The RESP/TLS listener currently uses one OS thread per connected client. Query
execution is still globally bounded by the query scheduler, so connected clients
cannot oversubscribe query CPU indefinitely.

This is intentionally recorded as a scalability difference from Redis/FalkorDB's
event-loop + blocked-client architecture. It is expected to be practical for
normal agent/browser workloads and hundreds of connections, but thousands of
simultaneously idle or queued connections should be benchmarked before claiming
equivalent connection scalability. An async/mio transport is a future option if
measurements justify it.

## Performance measurement

Do not compare wall-clock numbers from unrelated machines. FalkorDB's own
benchmark documentation reports a 1.46x host-to-host difference for byte-identical
engines.

Two performance layers are provided:

### CI regression smoke

`scripts/perf_smoke.ps1` starts the custom server on loopback and records a
small representative workload into the diagnostics artifact. It measures:

- `RETURN 1`
- indexed point lookup
- indexed range count
- one-hop traversal
- scan/aggregation
- single writes
- concurrent reads
- concurrent writes across multiple named graphs

This is for detecting large regressions between custom-host commits, not for
claiming stock-vs-custom parity.

### Same-machine stock comparison

`tools/perf_compare.py` accepts two endpoints and runs the exact same data
setup and workload against each one:

```powershell
$env:CANDIDATE_PASSWORD = "<custom password>"
$env:BASELINE_PASSWORD  = "<stock FalkorDB password>"

python tools/perf_compare.py \
  --candidate 127.0.0.1:6379 \
  --baseline 127.0.0.1:6380 \
  --nodes 10000 \
  --reps 500 \
  --workers 8 \
  --out perf-comparison.json
```

The baseline should be stock FalkorDB running on the same physical machine at
the same time window. The report includes p50/p95/p99 latency, QPS, and
candidate/baseline ratios.

## Performance claims

Until a same-machine comparison is run, the safe claim is:

- core graph execution uses essentially current FalkorDB engine code;
- the custom host has additional persistence, MCP, HTTP/TLS, and Windows
  integration layers whose overhead is independently measurable;
- concurrency correctness and multi-graph isolation are CI-gated;
- exact stock-vs-custom throughput/latency parity is not assumed without data.
