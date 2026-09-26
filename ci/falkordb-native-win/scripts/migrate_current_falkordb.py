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
import json
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


def canonical(value):
    """Convert FalkorDB/Redis response values into deterministic JSON-safe data."""
    if isinstance(value, bytes):
        return value.decode("utf-8")
    if isinstance(value, dict):
        return {
            text(key): canonical(item)
            for key, item in sorted(value.items(), key=lambda pair: text(pair[0]))
        }
    if isinstance(value, (list, tuple)):
        return [canonical(item) for item in value]
    return value


def canonical_rows(rows) -> list[str]:
    return sorted(
        json.dumps(canonical(row), sort_keys=True, separators=(",", ":"))
        for row in rows
    )


def graph_signature(db: FalkorDB, name: str) -> dict:
    """Capture migration-critical graph structure without depending on row order."""
    graph = db.select_graph(name)

    def scalar(query: str) -> int:
        result = graph.ro_query(query).result_set
        if len(result) != 1 or len(result[0]) != 1:
            raise RuntimeError(
                f"unexpected scalar result for {name!r}: {query!r} -> {result!r}"
            )
        return int(result[0][0])

    labels = canonical_rows(graph.ro_query("CALL db.labels()").result_set)
    relationship_types = canonical_rows(
        graph.ro_query("CALL db.relationshipTypes()").result_set
    )
    property_keys = canonical_rows(
        graph.ro_query("CALL db.propertyKeys()").result_set
    )

    # Exclude transient population status. Everything else here describes the
    # persistent index definition and must survive a lossless migration.
    indexes = canonical_rows(
        graph.ro_query(
            "CALL db.indexes() "
            "YIELD label, properties, types, language, stopwords, entitytype, info "
            "RETURN label, properties, types, language, stopwords, entitytype, info"
        ).result_set
    )

    constraints = canonical_rows(graph.list_constraints())

    return {
        "nodes": scalar("MATCH (n) RETURN count(n)"),
        "relationships": scalar("MATCH ()-[r]->() RETURN count(r)"),
        "labels": labels,
        "relationship_types": relationship_types,
        "property_keys": property_keys,
        "indexes": indexes,
        "constraints": constraints,
    }


def verify_graph(source_db: FalkorDB, destination_db: FalkorDB, name: str) -> None:
    source_signature = graph_signature(source_db, name)
    destination_signature = graph_signature(destination_db, name)
    if source_signature != destination_signature:
        raise RuntimeError(
            "semantic verification failed for "
            f"{name!r}\nsource={json.dumps(source_signature, sort_keys=True)}"
            f"\ndestination={json.dumps(destination_signature, sort_keys=True)}"
        )


def rollback_graph(
    destination: Redis,
    raw_name,
    name: str,
    previous_dump: bytes | None,
) -> None:
    """Best-effort transactional rollback for one graph migration."""
    try:
        current_graphs = {
            text(v) for v in destination.execute_command("GRAPH.LIST")
        }
        if previous_dump is not None:
            destination.restore(raw_name, 0, previous_dump, replace=True)
            print(f"rolled back previous destination graph {name!r}")
        elif name in current_graphs:
            destination.execute_command("GRAPH.DELETE", raw_name)
            print(f"removed failed migrated graph {name!r}")
    except Exception as rollback_exc:
        raise RuntimeError(
            f"migration failed for {name!r} and rollback also failed: {rollback_exc}"
        ) from rollback_exc


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
    parser.add_argument(
        "--skip-verify",
        action="store_true",
        help="skip semantic source/destination verification after each graph",
    )
    args = parser.parse_args()

    source_ep = endpoint_from_args(args, "source")
    destination_ep = endpoint_from_args(args, "destination")

    source = Redis(**source_ep.redis_kwargs())
    destination = Redis(**destination_ep.redis_kwargs())
    source_db = FalkorDB(**source_ep.falkor_kwargs())
    destination_db = FalkorDB(**destination_ep.falkor_kwargs())

    try:
        graph_names = source.execute_command("GRAPH.LIST")
        print(f"found {len(graph_names)} graph(s)")

        for raw_name in graph_names:
            name = text(raw_name)
            payload = source.dump(raw_name)
            if payload is None:
                raise RuntimeError(f"graph disappeared during migration: {name}")

            destination_graphs_before = {
                text(v) for v in destination.execute_command("GRAPH.LIST")
            }
            destination_had_graph = name in destination_graphs_before
            if destination_had_graph and not args.replace:
                raise RuntimeError(
                    f"destination graph {name!r} already exists; rerun with --replace"
                )

            previous_dump = None
            if destination_had_graph:
                previous_dump = destination.dump(raw_name)
                if previous_dump is None:
                    raise RuntimeError(
                        f"could not back up existing destination graph {name!r}"
                    )

            print(f"migrating {name!r}: {len(payload):,} byte DUMP payload")
            try:
                destination.restore(
                    raw_name,
                    0,
                    payload,
                    replace=args.replace,
                )

                # Verify the destination accepted it as a graph before moving on.
                dest_graphs = {
                    text(v) for v in destination.execute_command("GRAPH.LIST")
                }
                if name not in dest_graphs:
                    raise RuntimeError(
                        f"destination did not register restored graph {name!r}"
                    )

                if not args.skip_verify:
                    verify_graph(source_db, destination_db, name)
                    print(
                        f"verified {name!r}: "
                        "data/schema/index/constraint signature matches"
                    )
            except Exception as exc:
                try:
                    rollback_graph(destination, raw_name, name, previous_dump)
                except Exception as rollback_exc:
                    raise RuntimeError(
                        f"{exc}; additionally, {rollback_exc}"
                    ) from exc
                if isinstance(exc, ResponseError):
                    raise RuntimeError(
                        f"RESTORE failed for graph {name!r}: {exc}"
                    ) from exc
                raise

        if not args.skip_udfs:
            moved_udfs = migrate_udfs(source_ep, destination_ep, args.replace)
            print(f"migrated {moved_udfs} UDF librar{'y' if moved_udfs == 1 else 'ies'}")

        print(f"MIGRATION_COMPLETE graphs={len(graph_names)}")
        return 0
    finally:
        source.close()
        destination.close()
        source_db.close()
        destination_db.close()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"MIGRATION_FAILED: {exc}", file=sys.stderr)
        raise
