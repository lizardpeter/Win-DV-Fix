#!/usr/bin/env python3
"""Create and restore a portable, lossless FalkorDB migration bundle.

A bundle contains ordinary Redis/FalkorDB DUMP payloads, semantic graph
signatures, and UDF source code. This allows migration when the source and
native Windows destination cannot be online at the same time.
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
from redis.exceptions import ResponseError

from migrate_current_falkordb import (
    Endpoint,
    canonical,
    graph_signature,
    parse_udf_rows,
    require_standalone,
    text,
)

BUNDLE_FORMAT = "falkordb-native-migration"
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


def stable_graph_dump(
    raw: Redis,
    db: FalkorDB,
    raw_name,
    name: str,
    attempts: int = 3,
) -> tuple[bytes, dict]:
    for attempt in range(1, attempts + 1):
        before = graph_signature(db, name)
        payload = raw.dump(raw_name)
        if payload is None:
            raise RuntimeError(f"graph disappeared during export: {name!r}")
        after = graph_signature(db, name)
        if before == after:
            return payload, after
        print(
            f"source graph {name!r} changed while being exported; "
            f"retrying ({attempt}/{attempts})"
        )
    raise RuntimeError(
        f"source graph {name!r} kept changing during export; quiesce writers and retry"
    )


def export_bundle(args) -> None:
    ep = endpoint_from_args(args, "source")
    raw = Redis(**ep.redis_kwargs())
    db = FalkorDB(**ep.falkor_kwargs())
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)

    try:
        require_standalone(raw, "source")
        graph_names = raw.execute_command("GRAPH.LIST")
        graphs: list[dict] = []

        with tempfile.NamedTemporaryFile(
            prefix=output.name + ".tmp-",
            suffix=".zip",
            dir=output.parent,
            delete=False,
        ) as temp:
            temp_path = Path(temp.name)

        try:
            with zipfile.ZipFile(
                temp_path,
                "w",
                compression=zipfile.ZIP_DEFLATED,
                compresslevel=6,
            ) as zf:
                for raw_name in graph_names:
                    name = text(raw_name)
                    payload, signature = stable_graph_dump(raw, db, raw_name, name)
                    name_hash = hashlib.sha256(name.encode("utf-8")).hexdigest()
                    entry = f"graphs/{name_hash}.dump"
                    zf.writestr(entry, payload)
                    graphs.append(
                        {
                            "name": name,
                            "entry": entry,
                            "bytes": len(payload),
                            "sha256": sha256(payload),
                            "signature": canonical(signature),
                        }
                    )
                    print(
                        f"exported {name!r}: {len(payload):,} bytes "
                        f"sha256={sha256(payload)}"
                    )

                udfs = {} if args.skip_udfs else parse_udf_rows(
                    db.udf_list(with_code=True)
                )
                udf_entry = "udfs.json"
                zf.writestr(
                    udf_entry,
                    json.dumps(
                        udfs,
                        ensure_ascii=False,
                        sort_keys=True,
                        separators=(",", ":"),
                    ).encode("utf-8"),
                )

                manifest = {
                    "format": BUNDLE_FORMAT,
                    "version": BUNDLE_VERSION,
                    "graphs": graphs,
                    "udfs_entry": udf_entry,
                    "udf_count": len(udfs),
                }
                manifest_bytes = json.dumps(
                    manifest,
                    ensure_ascii=False,
                    sort_keys=True,
                    indent=2,
                ).encode("utf-8")
                zf.writestr("manifest.json", manifest_bytes)

            os.replace(temp_path, output)
        except Exception:
            try:
                temp_path.unlink()
            except FileNotFoundError:
                pass
            raise

        print(
            f"BUNDLE_EXPORT_COMPLETE graphs={len(graphs)} "
            f"udfs={manifest['udf_count']} path={output}"
        )
    finally:
        db.close()
        raw.close()


def load_bundle(path: Path) -> tuple[dict, zipfile.ZipFile]:
    zf = zipfile.ZipFile(path, "r")
    try:
        manifest = json.loads(zf.read("manifest.json"))
    except Exception:
        zf.close()
        raise

    if manifest.get("format") != BUNDLE_FORMAT:
        zf.close()
        raise RuntimeError(
            f"unsupported bundle format: {manifest.get('format')!r}"
        )
    if manifest.get("version") != BUNDLE_VERSION:
        zf.close()
        raise RuntimeError(
            f"unsupported bundle version: {manifest.get('version')!r}"
        )
    if not isinstance(manifest.get("graphs"), list):
        zf.close()
        raise RuntimeError("bundle manifest graphs field is invalid")
    return manifest, zf


def destination_graph_names(raw: Redis) -> set[str]:
    return {text(v) for v in raw.execute_command("GRAPH.LIST")}


def restore_one_graph(
    raw: Redis,
    db: FalkorDB,
    name: str,
    payload: bytes,
    signature: dict,
    replace: bool,
) -> None:
    names = destination_graph_names(raw)
    exists = name in names
    if exists and not replace:
        raise RuntimeError(
            f"destination graph {name!r} already exists; rerun with --replace"
        )

    old_dump = raw.dump(name) if exists else None
    try:
        reply = raw.restore(name, 0, payload, replace=replace)
        if reply not in (True, b"OK", "OK"):
            raise RuntimeError(f"unexpected RESTORE reply for {name!r}: {reply!r}")
        actual = canonical(graph_signature(db, name))
        if actual != canonical(signature):
            raise RuntimeError(
                f"semantic verification failed for {name!r}\n"
                f"bundle={json.dumps(signature, sort_keys=True)}\n"
                f"destination={json.dumps(actual, sort_keys=True)}"
            )
    except Exception as exc:
        try:
            if old_dump is not None:
                raw.restore(name, 0, old_dump, replace=True)
            elif name in destination_graph_names(raw):
                raw.execute_command("GRAPH.DELETE", name)
        except Exception as rollback_exc:
            raise RuntimeError(
                f"import failed for {name!r}: {exc}; rollback failed: {rollback_exc}"
            ) from exc
        raise


def restore_udfs(
    db: FalkorDB,
    libraries: dict[str, str],
    replace: bool,
) -> None:
    existing = parse_udf_rows(db.udf_list(with_code=True))
    for name, code in libraries.items():
        previous = existing.get(name)
        if previous is not None and not replace:
            raise RuntimeError(
                f"destination UDF library {name!r} already exists; rerun with --replace"
            )
        try:
            db.udf_load(name, code, replace)
            verified = parse_udf_rows(db.udf_list(name, with_code=True))
            if verified.get(name) != code:
                raise RuntimeError(
                    f"destination UDF library {name!r} did not preserve source code"
                )
        except Exception as exc:
            try:
                if previous is not None:
                    db.udf_load(name, previous, True)
                else:
                    current = parse_udf_rows(db.udf_list(name, with_code=True))
                    if name in current:
                        db.udf_delete(name)
            except Exception as rollback_exc:
                raise RuntimeError(
                    f"UDF import failed for {name!r}: {exc}; "
                    f"rollback failed: {rollback_exc}"
                ) from exc
            raise
        existing[name] = code


def import_bundle(args) -> None:
    ep = endpoint_from_args(args, "destination")
    raw = Redis(**ep.redis_kwargs())
    db = FalkorDB(**ep.falkor_kwargs())
    manifest, zf = load_bundle(args.input.resolve())

    try:
        require_standalone(raw, "destination")

        for item in manifest["graphs"]:
            name = item.get("name")
            entry = item.get("entry")
            expected_hash = item.get("sha256")
            signature = item.get("signature")
            if not isinstance(name, str) or not isinstance(entry, str):
                raise RuntimeError(f"invalid graph manifest entry: {item!r}")
            if not isinstance(signature, dict):
                raise RuntimeError(f"missing graph signature for {name!r}")

            payload = zf.read(entry)
            actual_hash = sha256(payload)
            if actual_hash != expected_hash:
                raise RuntimeError(
                    f"bundle graph {name!r} sha256 mismatch: "
                    f"expected {expected_hash}, got {actual_hash}"
                )
            if len(payload) != item.get("bytes"):
                raise RuntimeError(
                    f"bundle graph {name!r} length mismatch: "
                    f"expected {item.get('bytes')}, got {len(payload)}"
                )

            print(
                f"importing {name!r}: {len(payload):,} bytes "
                f"sha256={actual_hash}"
            )
            restore_one_graph(
                raw,
                db,
                name,
                payload,
                signature,
                args.replace,
            )
            print(f"verified {name!r}: data/schema/index/constraint signature matches")

        if not args.skip_udfs:
            udf_entry = manifest.get("udfs_entry", "udfs.json")
            libraries = json.loads(zf.read(udf_entry))
            if not isinstance(libraries, dict):
                raise RuntimeError("bundle UDF payload is invalid")
            restore_udfs(
                db,
                {str(name): str(code) for name, code in libraries.items()},
                args.replace,
            )
            print(f"imported {len(libraries)} UDF libraries")

        print(
            f"BUNDLE_IMPORT_COMPLETE graphs={len(manifest['graphs'])} "
            f"path={args.input.resolve()}"
        )
    finally:
        zf.close()
        db.close()
        raw.close()


def inspect_bundle(args) -> None:
    manifest, zf = load_bundle(args.input.resolve())
    try:
        print(json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True))
    finally:
        zf.close()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Portable offline FalkorDB migration bundles"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    export = sub.add_parser("export", help="export a current FalkorDB server")
    add_endpoint_args(export, "source")
    export.add_argument("--output", type=Path, required=True)
    export.add_argument("--skip-udfs", action="store_true")

    imp = sub.add_parser("import", help="import into the native Windows server")
    add_endpoint_args(imp, "destination")
    imp.add_argument("--input", type=Path, required=True)
    imp.add_argument("--replace", action="store_true")
    imp.add_argument("--skip-udfs", action="store_true")

    inspect = sub.add_parser("inspect", help="print bundle manifest")
    inspect.add_argument("--input", type=Path, required=True)

    args = parser.parse_args()
    if args.command == "export":
        export_bundle(args)
    elif args.command == "import":
        import_bundle(args)
    else:
        inspect_bundle(args)


if __name__ == "__main__":
    main()
