#!/usr/bin/env python3
"""Export/import a complete FalkorDB database as one portable archive.

This is the offline counterpart to migrate_current_falkordb.py. It preserves
FalkorDB graphs as exact Redis DUMP byte strings rather than reconstructing
nodes/relationships through Cypher.

Bundle v1 contains:
  * manifest.json
  * one exact Redis DUMP payload per graph
  * process-global UDF library source code in the manifest
  * semantic graph signatures for post-import verification
  * SHA-256 for every binary graph payload

The source and destination do not need to be reachable at the same time.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import tempfile
import zipfile
from pathlib import Path

from falkordb import FalkorDB
from redis import Redis

from migrate_current_falkordb import (
    Endpoint,
    graph_signature,
    parse_udf_rows,
    require_standalone,
    text,
)

BUNDLE_VERSION = 1


def add_endpoint_args(parser: argparse.ArgumentParser, prefix: str) -> None:
    parser.add_argument(f"--{prefix}-host", required=True)
    parser.add_argument(f"--{prefix}-port", type=int, default=6379)
    parser.add_argument(f"--{prefix}-username")
    parser.add_argument(f"--{prefix}-password")
    parser.add_argument(f"--{prefix}-ssl", action="store_true")
    parser.add_argument(f"--{prefix}-ca")
    parser.add_argument(f"--{prefix}-cert")
    parser.add_argument(f"--{prefix}-key")


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


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def consistent_dump(
    redis_client: Redis,
    db: FalkorDB,
    raw_name,
    name: str,
    *,
    verify: bool,
) -> tuple[bytes, dict | None]:
    if not verify:
        payload = redis_client.dump(raw_name)
        if payload is None:
            raise RuntimeError(f"graph disappeared during export: {name!r}")
        return payload, None

    for attempt in range(1, 4):
        before = graph_signature(db, name)
        payload = redis_client.dump(raw_name)
        if payload is None:
            raise RuntimeError(f"graph disappeared during export: {name!r}")
        after = graph_signature(db, name)
        if before == after:
            return payload, after
        print(
            f"source graph {name!r} changed while being exported; "
            f"retrying ({attempt}/3)"
        )

    raise RuntimeError(
        f"source graph {name!r} kept changing during export; "
        "quiesce writers and retry"
    )


def export_bundle(args) -> int:
    source_ep = endpoint_from_args(args, "source")
    source = Redis(**source_ep.redis_kwargs())
    source_db = FalkorDB(**source_ep.falkor_kwargs())

    bundle = args.bundle.resolve()
    bundle.parent.mkdir(parents=True, exist_ok=True)

    temp_path: Path | None = None
    try:
        require_standalone(source, "source")
        raw_graph_names = source.execute_command("GRAPH.LIST")
        udf_libraries = (
            {}
            if args.skip_udfs
            else parse_udf_rows(source_db.udf_list(with_code=True))
        )

        fd, temp_name = tempfile.mkstemp(
            prefix=bundle.name + ".tmp-",
            suffix=".zip",
            dir=bundle.parent,
        )
        os.close(fd)
        temp_path = Path(temp_name)

        graph_entries: list[dict] = []
        with zipfile.ZipFile(
            temp_path,
            "w",
            compression=zipfile.ZIP_STORED,
            allowZip64=True,
        ) as archive:
            for index, raw_name in enumerate(raw_graph_names):
                name = text(raw_name)
                payload, signature = consistent_dump(
                    source,
                    source_db,
                    raw_name,
                    name,
                    verify=not args.skip_verify,
                )
                archive_path = f"graphs/{index:06d}.dump"
                payload_hash = sha256(payload)

                # Write each DUMP immediately. Export memory therefore scales
                # with the largest graph rather than the sum of all graphs.
                archive.writestr(archive_path, payload)
                graph_entries.append(
                    {
                        "name": name,
                        "archive_path": archive_path,
                        "bytes": len(payload),
                        "sha256": payload_hash,
                        "signature": signature,
                    }
                )
                print(
                    f"exported {name!r}: {len(payload):,} bytes "
                    f"sha256={payload_hash[:16]}..."
                )
                del payload

            server_info = source.info(section="server")
            manifest = {
                "bundle_version": BUNDLE_VERSION,
                "format": "falkordb-portable-dump-bundle",
                "graphdata_encoding": 19,
                "source": {
                    "redis_version": text(
                        server_info.get("redis_version")
                        or server_info.get(b"redis_version")
                        or ""
                    ),
                    "redis_mode": text(
                        server_info.get("redis_mode")
                        or server_info.get(b"redis_mode")
                        or ""
                    ),
                },
                "graphs": graph_entries,
                "udf_libraries": udf_libraries,
            }
            archive.writestr(
                "manifest.json",
                json.dumps(
                    manifest,
                    sort_keys=True,
                    ensure_ascii=False,
                    indent=2,
                ).encode("utf-8"),
            )

        # Flush the completed archive before atomically publishing its name.
        with open(temp_path, "rb") as handle:
            os.fsync(handle.fileno())
        os.replace(temp_path, bundle)
        temp_path = None

        print(
            f"BUNDLE_EXPORT_COMPLETE path={bundle} "
            f"graphs={len(graph_entries)} udfs={len(udf_libraries)}"
        )
        return 0
    finally:
        if temp_path is not None and temp_path.exists():
            temp_path.unlink()
        source.close()
        source_db.close()


def load_bundle(bundle: Path) -> tuple[dict, dict[str, bytes]]:
    with zipfile.ZipFile(bundle, "r") as archive:
        try:
            manifest = json.loads(archive.read("manifest.json"))
        except KeyError as exc:
            raise RuntimeError("bundle is missing manifest.json") from exc

        if manifest.get("bundle_version") != BUNDLE_VERSION:
            raise RuntimeError(
                f"unsupported bundle version {manifest.get('bundle_version')!r}; "
                f"expected {BUNDLE_VERSION}"
            )
        if manifest.get("format") != "falkordb-portable-dump-bundle":
            raise RuntimeError("archive is not a FalkorDB portable bundle")

        payloads: dict[str, bytes] = {}
        seen_names: set[str] = set()
        for entry in manifest.get("graphs", []):
            name = entry.get("name")
            archive_path = entry.get("archive_path")
            expected_hash = entry.get("sha256")
            if not isinstance(name, str) or not name:
                raise RuntimeError(f"invalid graph name in bundle: {name!r}")
            if name in seen_names:
                raise RuntimeError(f"duplicate graph name in bundle: {name!r}")
            seen_names.add(name)
            if not isinstance(archive_path, str) or not archive_path.startswith("graphs/"):
                raise RuntimeError(
                    f"invalid graph archive path for {name!r}: {archive_path!r}"
                )
            try:
                payload = archive.read(archive_path)
            except KeyError as exc:
                raise RuntimeError(
                    f"bundle is missing graph payload {archive_path!r}"
                ) from exc
            if sha256(payload) != expected_hash:
                raise RuntimeError(
                    f"SHA-256 mismatch for bundled graph {name!r}"
                )
            if len(payload) != int(entry.get("bytes", -1)):
                raise RuntimeError(
                    f"byte-length mismatch for bundled graph {name!r}"
                )
            payloads[name] = payload

    return manifest, payloads


def rollback_graph(
    destination: Redis,
    name: str,
    previous_dump: bytes | None,
) -> None:
    if previous_dump is not None:
        destination.restore(name, 0, previous_dump, replace=True)
        return
    current = {text(v) for v in destination.execute_command("GRAPH.LIST")}
    if name in current:
        destination.execute_command("GRAPH.DELETE", name)


def import_bundle(args) -> int:
    manifest, payloads = load_bundle(args.bundle.resolve())

    destination_ep = endpoint_from_args(args, "destination")
    destination = Redis(**destination_ep.redis_kwargs())
    destination_db = FalkorDB(**destination_ep.falkor_kwargs())

    graph_backups: dict[str, bytes | None] = {}
    udf_backups: dict[str, str | None] = {}
    imported_graphs: list[str] = []
    imported_udfs: list[str] = []

    try:
        require_standalone(destination, "destination")
        destination_graphs = {
            text(v) for v in destination.execute_command("GRAPH.LIST")
        }

        # Preflight all conflicts and capture rollback state before mutating any
        # graph. This makes bundle import all-or-rollback at the graph/UDF level.
        for entry in manifest.get("graphs", []):
            name = entry["name"]
            if name in destination_graphs:
                if not args.replace:
                    raise RuntimeError(
                        f"destination graph {name!r} already exists; "
                        "rerun with --replace"
                    )
                previous = destination.dump(name)
                if previous is None:
                    raise RuntimeError(
                        f"could not back up destination graph {name!r}"
                    )
                graph_backups[name] = previous
            else:
                graph_backups[name] = None

        existing_udfs = parse_udf_rows(
            destination_db.udf_list(with_code=True)
        )
        bundle_udfs: dict[str, str] = manifest.get("udf_libraries", {})
        for name in bundle_udfs:
            previous = existing_udfs.get(name)
            if previous is not None and not args.replace:
                raise RuntimeError(
                    f"destination UDF library {name!r} already exists; "
                    "rerun with --replace"
                )
            udf_backups[name] = previous

        try:
            for entry in manifest.get("graphs", []):
                name = entry["name"]
                payload = payloads[name]
                destination.restore(
                    name,
                    0,
                    payload,
                    replace=graph_backups[name] is not None,
                )
                imported_graphs.append(name)

                signature = entry.get("signature")
                if signature is not None and not args.skip_verify:
                    actual = graph_signature(destination_db, name)
                    if actual != signature:
                        raise RuntimeError(
                            f"semantic verification failed for {name!r}\n"
                            f"bundle={json.dumps(signature, sort_keys=True)}\n"
                            f"destination={json.dumps(actual, sort_keys=True)}"
                        )
                print(f"imported {name!r}: {len(payload):,} bytes")

            for name, code in bundle_udfs.items():
                destination_db.udf_load(
                    name,
                    code,
                    udf_backups[name] is not None,
                )
                imported_udfs.append(name)
                current = parse_udf_rows(
                    destination_db.udf_list(name, with_code=True)
                )
                if current.get(name) != code:
                    raise RuntimeError(
                        f"destination UDF library {name!r} did not preserve source code"
                    )

        except Exception as exc:
            rollback_errors: list[str] = []

            for name in reversed(imported_udfs):
                try:
                    previous = udf_backups[name]
                    if previous is None:
                        destination_db.udf_delete(name)
                    else:
                        destination_db.udf_load(name, previous, True)
                except Exception as rollback_exc:
                    rollback_errors.append(
                        f"UDF {name!r}: {rollback_exc}"
                    )

            for name in reversed(imported_graphs):
                try:
                    rollback_graph(
                        destination,
                        name,
                        graph_backups[name],
                    )
                except Exception as rollback_exc:
                    rollback_errors.append(
                        f"graph {name!r}: {rollback_exc}"
                    )

            if rollback_errors:
                raise RuntimeError(
                    f"bundle import failed: {exc}; rollback errors: "
                    + "; ".join(rollback_errors)
                ) from exc
            raise

        print(
            f"BUNDLE_IMPORT_COMPLETE graphs={len(imported_graphs)} "
            f"udfs={len(imported_udfs)}"
        )
        return 0
    finally:
        destination.close()
        destination_db.close()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Export/import a complete FalkorDB database as one archive"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    export = sub.add_parser(
        "export",
        help="export a running FalkorDB source to a portable bundle",
    )
    add_endpoint_args(export, "source")
    export.add_argument("--bundle", type=Path, required=True)
    export.add_argument("--skip-udfs", action="store_true")
    export.add_argument("--skip-verify", action="store_true")

    imp = sub.add_parser(
        "import",
        help="import a portable bundle into the native Windows server",
    )
    add_endpoint_args(imp, "destination")
    imp.add_argument("--bundle", type=Path, required=True)
    imp.add_argument("--replace", action="store_true")
    imp.add_argument("--skip-verify", action="store_true")

    args = parser.parse_args()
    if args.command == "export":
        return export_bundle(args)
    return import_bundle(args)


if __name__ == "__main__":
    raise SystemExit(main())
