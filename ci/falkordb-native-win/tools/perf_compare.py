#!/usr/bin/env python3
"""Same-machine FalkorDB endpoint benchmark.

Run the exact same workload against the custom native endpoint and a stock
FalkorDB endpoint on the same host. Cross-machine numbers are intentionally not
treated as comparable.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import os
import statistics
import time
from dataclasses import asdict, dataclass
from typing import Callable

from falkordb import FalkorDB
from redis import Redis


@dataclass
class Endpoint:
    name: str
    host: str
    port: int
    username: str | None
    password: str | None


@dataclass
class Metric:
    name: str
    ops: int
    seconds: float
    qps: float
    p50_ms: float
    p95_ms: float
    p99_ms: float


def parse_endpoint(name: str, value: str, prefix: str) -> Endpoint:
    if ":" not in value:
        raise SystemExit(f"{name} endpoint must be HOST:PORT")
    host, port_text = value.rsplit(":", 1)
    return Endpoint(
        name=name,
        host=host,
        port=int(port_text),
        username=os.environ.get(f"{prefix}_USERNAME") or None,
        password=os.environ.get(f"{prefix}_PASSWORD") or None,
    )


def db(ep: Endpoint) -> FalkorDB:
    kwargs = dict(host=ep.host, port=ep.port)
    if ep.username:
        kwargs["username"] = ep.username
    if ep.password:
        kwargs["password"] = ep.password
    return FalkorDB(**kwargs)


def redis(ep: Endpoint) -> Redis:
    kwargs = dict(host=ep.host, port=ep.port, decode_responses=True)
    if ep.username:
        kwargs["username"] = ep.username
    if ep.password:
        kwargs["password"] = ep.password
    return Redis(**kwargs)


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(0, min(len(ordered) - 1, math.ceil(pct * len(ordered)) - 1))
    return ordered[rank]


def time_ops(name: str, fn: Callable[[], None], warmup: int, reps: int) -> Metric:
    for _ in range(warmup):
        fn()
    samples = []
    start = time.perf_counter()
    for _ in range(reps):
        t0 = time.perf_counter_ns()
        fn()
        samples.append((time.perf_counter_ns() - t0) / 1_000_000.0)
    seconds = time.perf_counter() - start
    return Metric(
        name=name,
        ops=reps,
        seconds=seconds,
        qps=reps / seconds if seconds else float("inf"),
        p50_ms=statistics.median(samples),
        p95_ms=percentile(samples, 0.95),
        p99_ms=percentile(samples, 0.99),
    )


def setup_graph(ep: Endpoint, graph_name: str, n: int) -> None:
    client = db(ep)
    r = redis(ep)
    try:
        try:
            r.execute_command("GRAPH.DELETE", graph_name)
        except Exception:
            pass
        g = client.select_graph(graph_name)
        g.query("CREATE INDEX FOR (n:Bench) ON (n.id)")
        g.query(
            f"UNWIND range(0,{n - 1}) AS i "
            "CREATE (:Bench {id:i, grp:i % 100, value:i})"
        )
        g.query(
            f"UNWIND range(0,{n - 2}) AS i "
            "MATCH (a:Bench {id:i}),(b:Bench {id:i+1}) "
            "CREATE (a)-[:NEXT]->(b)"
        )
    finally:
        r.close()


def run_concurrent_reads(
    ep: Endpoint, graph_name: str, node_count: int, workers: int, per_worker: int
) -> Metric:
    latencies: list[float] = []

    def worker(worker_id: int) -> list[float]:
        client = db(ep)
        g = client.select_graph(graph_name)
        local = []
        for i in range(per_worker):
            node_id = (worker_id * per_worker + i) % node_count
            t0 = time.perf_counter_ns()
            g.ro_query(f"MATCH (n:Bench {{id:{node_id}}}) RETURN n.value")
            local.append((time.perf_counter_ns() - t0) / 1_000_000.0)
        return local

    start = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        for local in pool.map(worker, range(workers)):
            latencies.extend(local)
    seconds = time.perf_counter() - start
    ops = workers * per_worker
    return Metric(
        name=f"concurrent_read_{workers}",
        ops=ops,
        seconds=seconds,
        qps=ops / seconds,
        p50_ms=statistics.median(latencies),
        p95_ms=percentile(latencies, 0.95),
        p99_ms=percentile(latencies, 0.99),
    )


def run_multigraph_writes(ep: Endpoint, graph_prefix: str, graphs: int, writes: int) -> Metric:
    client = db(ep)
    r = redis(ep)
    names = [f"{graph_prefix}_mg_{i}" for i in range(graphs)]
    try:
        for name in names:
            try:
                r.execute_command("GRAPH.DELETE", name)
            except Exception:
                pass
            client.select_graph(name).query("CREATE (:Counter {id:0,value:0})")

        latencies: list[float] = []

        def worker(name: str) -> list[float]:
            local_client = db(ep)
            g = local_client.select_graph(name)
            local = []
            for _ in range(writes):
                t0 = time.perf_counter_ns()
                g.query(
                    "MATCH (n:Counter {id:0}) "
                    "SET n.value=n.value+1 RETURN n.value"
                )
                local.append((time.perf_counter_ns() - t0) / 1_000_000.0)
            return local

        start = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(max_workers=graphs) as pool:
            for local in pool.map(worker, names):
                latencies.extend(local)
        seconds = time.perf_counter() - start

        for name in names:
            value = client.select_graph(name).ro_query(
                "MATCH (n:Counter {id:0}) RETURN n.value"
            ).result_set[0][0]
            if int(value) != writes:
                raise RuntimeError(f"{ep.name}/{name}: expected {writes}, got {value}")

        ops = graphs * writes
        return Metric(
            name=f"multigraph_write_{graphs}",
            ops=ops,
            seconds=seconds,
            qps=ops / seconds,
            p50_ms=statistics.median(latencies),
            p95_ms=percentile(latencies, 0.95),
            p99_ms=percentile(latencies, 0.99),
        )
    finally:
        for name in names:
            try:
                r.execute_command("GRAPH.DELETE", name)
            except Exception:
                pass
        r.close()


def run_endpoint(ep: Endpoint, n: int, reps: int, workers: int) -> dict:
    graph_name = f"perf_{ep.name}_{os.getpid()}"
    setup_graph(ep, graph_name, n)
    client = db(ep)
    r = redis(ep)
    g = client.select_graph(graph_name)
    middle = n // 2

    workloads = [
        ("return_1", lambda: g.ro_query("RETURN 1")),
        (
            "point_lookup",
            lambda: g.ro_query(f"MATCH (n:Bench {{id:{middle}}}) RETURN n.value"),
        ),
        (
            "range_count",
            lambda: g.ro_query(
                f"MATCH (n:Bench) WHERE n.id >= {middle - 100} "
                f"AND n.id < {middle + 100} RETURN count(n)"
            ),
        ),
        (
            "one_hop",
            lambda: g.ro_query(
                f"MATCH (:Bench {{id:{middle}}})-[:NEXT]->(b) RETURN b.id"
            ),
        ),
        (
            "scan_aggregate",
            lambda: g.ro_query(
                "MATCH (n:Bench) WHERE n.grp=42 RETURN sum(n.value)"
            ),
        ),
        (
            "single_write",
            lambda: g.query(
                "MATCH (n:Bench {id:0}) SET n.value=n.value+1 RETURN n.value"
            ),
        ),
    ]

    metrics = []
    try:
        for name, fn in workloads:
            metrics.append(time_ops(name, fn, warmup=max(5, reps // 20), reps=reps))
        metrics.append(
            run_concurrent_reads(
                ep,
                graph_name,
                node_count=n,
                workers=workers,
                per_worker=max(25, reps // 2),
            )
        )
        metrics.append(
            run_multigraph_writes(ep, graph_name, graphs=min(4, workers), writes=max(20, reps // 4))
        )
        return {
            "endpoint": asdict(ep) | {"password": bool(ep.password)},
            "graph_nodes": n,
            "metrics": [asdict(metric) for metric in metrics],
        }
    finally:
        try:
            r.execute_command("GRAPH.DELETE", graph_name)
        except Exception:
            pass
        r.close()


def compare(candidate: dict, baseline: dict) -> list[dict]:
    base = {m["name"]: m for m in baseline["metrics"]}
    rows = []
    for current in candidate["metrics"]:
        other = base[current["name"]]
        rows.append(
            {
                "name": current["name"],
                "candidate_p50_ms": current["p50_ms"],
                "baseline_p50_ms": other["p50_ms"],
                "p50_ratio_candidate_over_baseline": (
                    current["p50_ms"] / other["p50_ms"] if other["p50_ms"] else None
                ),
                "candidate_qps": current["qps"],
                "baseline_qps": other["qps"],
                "qps_ratio_candidate_over_baseline": (
                    current["qps"] / other["qps"] if other["qps"] else None
                ),
            }
        )
    return rows


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", required=True, help="HOST:PORT")
    parser.add_argument("--baseline", help="HOST:PORT for stock FalkorDB on SAME machine")
    parser.add_argument("--nodes", type=int, default=10_000)
    parser.add_argument("--reps", type=int, default=200)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--out", default="")
    args = parser.parse_args()

    candidate_ep = parse_endpoint("custom", args.candidate, "CANDIDATE")
    result = {
        "warning": (
            "Only same-machine candidate/baseline runs are comparable. "
            "Do not compare this JSON with results from another host."
        ),
        "candidate": run_endpoint(candidate_ep, args.nodes, args.reps, args.workers),
    }

    if args.baseline:
        baseline_ep = parse_endpoint("stock", args.baseline, "BASELINE")
        result["baseline"] = run_endpoint(
            baseline_ep, args.nodes, args.reps, args.workers
        )
        result["comparison"] = compare(result["candidate"], result["baseline"])

    text = json.dumps(result, indent=2, sort_keys=True)
    print(text)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as handle:
            handle.write(text)
            handle.write("\n")


if __name__ == "__main__":
    main()
