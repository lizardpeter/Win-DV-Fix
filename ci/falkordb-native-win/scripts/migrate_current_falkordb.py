#!/usr/bin/env python3
"""Losslessly migrate a current FalkorDB deployment into the native Windows host.

Graphs move as Redis DUMP payloads, not as reconstructed Cypher rows. That keeps
FalkorDB's graph schema, indexes, constraints, internal graph representation,
and property values inside the upstream graphdata v19 serialization.

The destination implements Redis RESTORE for graphdata, so this script works
without a custom export plugin on the source FalkorDB server.
"""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass

from falkordb import FalkorDB
from redis import Redis
from redis.exceptions import ResponseError


@dataclass
class Endpoint:
    host: str
    port: int
    username: str | None
    password: str | None
    ssl: bool
    ca: str | None
    cert: str | None
    key: str | None

    def redis_kwargs(self) -> dict:
        kwargs = {
            "host": self.host,
            "port": self.port,
            "username": self.username,
            "password": self.password,
            "socket_connect_timeout": 10,
            "socket_timeout": 300,
            "protocol": 2,
            "decode_responses": False,
            "ssl": self.ssl,
        }
        if self.ssl:
            if self.ca:
                kwargs["ssl_ca_certs"] = self.ca
                kwargs["ssl_cert_reqs"] = "required"
            if self.cert:
                kwargs["ssl_certfile"] = self.cert
            if self.key:
                kwargs["ssl_keyfile"] = self.key
        return kwargs

    def falkor_kwargs(self) -> dict:
        kwargs = self.redis_kwargs()
        kwargs.pop("decode_responses", None)
        return kwargs


def endpoint_from_args(args, prefix: str) -> Endpoint:
    return Endpoint(
        host=getattr(args, f"{prefix}_host"),
        port=getattr(args, f"{prefix}_port"),
        username=getattr(args, f"{prefix}_username"),
        password=getattr(args, f"{prefix}_password"),
        ssl=getattr(args, f"{prefix}_ssl"),
        ca=getattr(args, f"{prefix}_ca"),
        cert=getattr(args, f"{prefix}_cert"),
        key=getattr(args, f"{prefix}_key"),
    )


def text(value) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8")
    return str(value)


def migrate_udfs(source_ep: Endpoint, destination_ep: Endpoint, replace: bool) -> int:
    """Move process-global FalkorDB UDF libraries through the public API."""
    source = FalkorDB(**source_ep.falkor_kwargs())
    destination = FalkorDB(**destination_ep.falkor_kwargs())
    moved = 0
    try:
        libraries = source.udf_list(with_code=True)
        for row in libraries:
            # Current FalkorDB returns alternating labeled fields:
            # ["library_name", name, "functions", [...],
            #  "library_code", source].
            if len(row) < 4 or len(row) % 2 != 0:
                raise RuntimeError(f"unexpected GRAPH.UDF LIST WITHCODE row: {row!r}")
            fields = {
                text(row[i]).lower(): row[i + 1]
                for i in range(0, len(row), 2)
            }
            if "library_name" not in fields or "library_code" not in fields:
                raise RuntimeError(
                    f"GRAPH.UDF LIST WITHCODE omitted required fields: {row!r}"
                )
            name = text(fields["library_name"])
            code = text(fields["library_code"])
            destination.udf_load(name, code, replace)
            moved += 1
    finally:
        source.close()
        destination.close()
    return moved


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Migrate all current FalkorDB graphs into the native Windows host"
    )
    for prefix, default_port in (("source", 6379), ("destination", 6379)):
        group = parser.add_argument_group(prefix)
        group.add_argument(f"--{prefix}-host", required=True)
        group.add_argument(f"--{prefix}-port", type=int, default=default_port)
        group.add_argument(f"--{prefix}-username")
        group.add_argument(f"--{prefix}-password")
        group.add_argument(f"--{prefix}-ssl", action="store_true")
        group.add_argument(f"--{prefix}-ca")
        group.add_argument(f"--{prefix}-cert")
        group.add_argument(f"--{prefix}-key")

    parser.add_argument(
        "--replace",
        action="store_true",
        help="replace destination graphs/UDF libraries with matching names",
    )
    parser.add_argument(
        "--skip-udfs",
        action="store_true",
        help="migrate graph keys only",
    )
    args = parser.parse_args()

    source_ep = endpoint_from_args(args, "source")
    destination_ep = endpoint_from_args(args, "destination")

    source = Redis(**source_ep.redis_kwargs())
    destination = Redis(**destination_ep.redis_kwargs())

    try:
        graph_names = source.execute_command("GRAPH.LIST")
        print(f"found {len(graph_names)} graph(s)")

        for raw_name in graph_names:
            name = text(raw_name)
            payload = source.dump(raw_name)
            if payload is None:
                raise RuntimeError(f"graph disappeared during migration: {name}")

            print(f"migrating {name!r}: {len(payload):,} byte DUMP payload")
            try:
                destination.restore(
                    raw_name,
                    0,
                    payload,
                    replace=args.replace,
                )
            except ResponseError as exc:
                raise RuntimeError(f"RESTORE failed for graph {name!r}: {exc}") from exc

            # Verify the destination accepted it as a graph before moving on.
            dest_graphs = {text(v) for v in destination.execute_command("GRAPH.LIST")}
            if name not in dest_graphs:
                raise RuntimeError(f"destination did not register restored graph {name!r}")

        if not args.skip_udfs:
            moved_udfs = migrate_udfs(source_ep, destination_ep, args.replace)
            print(f"migrated {moved_udfs} UDF librar{'y' if moved_udfs == 1 else 'ies'}")

        print(f"MIGRATION_COMPLETE graphs={len(graph_names)}")
        return 0
    finally:
        source.close()
        destination.close()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"MIGRATION_FAILED: {exc}", file=sys.stderr)
        raise
