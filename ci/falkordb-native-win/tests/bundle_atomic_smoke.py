#!/usr/bin/env python3
"""Prove a failed later graph import rolls back all earlier bundle changes."""

from __future__ import annotations

import argparse
import json
import ssl
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

from falkordb import FalkorDB
from redis import Redis

SCRIPTS_DIR = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS_DIR))
from migrate_current_falkordb import canonical, graph_signature, parse_udf_rows


def redis_client(args) -> Redis:
    kwargs = {
        "host": args.host,
        "port": args.port,
        "password": args.password,
        "decode_responses": False,
        "socket_connect_timeout": 5,
        "socket_timeout": 30,
    }
    if args.ssl:
        kwargs.update(
            ssl=True,
            ssl_ca_certs=args.ca,
            ssl_certfile=args.cert,
            ssl_keyfile=args.key,
            ssl_cert_reqs="required",
        )
    return Redis(**kwargs)


def falkordb_client(args) -> FalkorDB:
    kwargs = {
        "host": args.host,
        "port": args.port,
        "password": args.password,
    }
    if args.ssl:
        kwargs.update(
            ssl=True,
            ssl_ca_certs=args.ca,
            ssl_certfile=args.cert,
            ssl_keyfile=args.key,
            ssl_cert_reqs="required",
        )
    return FalkorDB(**kwargs)


def utility_args(args, bundle: Path) -> list[str]:
    out = [
        sys.executable,
        str(args.utility),
        "import",
        "--destination-host",
        args.host,
        "--destination-port",
        str(args.port),
        "--destination-password",
        args.password,
        "--input",
        str(bundle),
        "--replace",
    ]
    if args.ssl:
        out.extend(
            [
                "--destination-ssl",
                "--destination-ca",
                args.ca,
                "--destination-cert",
                args.cert,
                "--destination-key",
                args.key,
            ]
        )
    return out


def tamper_last_signature(source: Path, target: Path) -> list[str]:
    with zipfile.ZipFile(source, "r") as src:
        manifest = json.loads(src.read("manifest.json"))
        graphs = manifest.get("graphs")
        if not isinstance(graphs, list) or len(graphs) < 2:
            raise RuntimeError(
                "atomic rollback smoke requires at least two graphs in the bundle"
            )
        names = [str(item["name"]) for item in graphs]
        signature = graphs[-1].get("signature")
        if not isinstance(signature, dict) or "nodes" not in signature:
            raise RuntimeError("last graph has no mutable semantic signature")
        signature["nodes"] = int(signature["nodes"]) + 1

        with zipfile.ZipFile(
            target, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6
        ) as dst:
            for info in src.infolist():
                if info.filename == "manifest.json":
                    dst.writestr(
                        info,
                        json.dumps(
                            manifest,
                            ensure_ascii=False,
                            sort_keys=True,
                            indent=2,
                        ).encode("utf-8"),
                    )
                else:
                    dst.writestr(info, src.read(info.filename))
    return names


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--utility", type=Path, required=True)
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--password", required=True)
    parser.add_argument("--ssl", action="store_true")
    parser.add_argument("--ca")
    parser.add_argument("--cert")
    parser.add_argument("--key")
    args = parser.parse_args()

    source = args.bundle.resolve()
    if not source.is_file():
        raise RuntimeError(f"bundle not found: {source}")
    args.utility = args.utility.resolve()

    with tempfile.TemporaryDirectory(prefix="falkordb-bundle-atomic-") as td:
        tampered = Path(td) / "tampered.zip"
        names = tamper_last_signature(source, tampered)

        raw = redis_client(args)
        db = falkordb_client(args)
        try:
            existing = {
                (value.decode() if isinstance(value, bytes) else str(value))
                for value in raw.execute_command("GRAPH.LIST")
            }
            missing = [name for name in names if name not in existing]
            if missing:
                raise RuntimeError(
                    f"destination is missing bundle graph(s) before rollback test: {missing}"
                )
            before = {
                name: canonical(graph_signature(db, name))
                for name in names
            }
            before_udfs = parse_udf_rows(db.udf_list(with_code=True))

            result = subprocess.run(
                utility_args(args, tampered),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=120,
                check=False,
            )
            print(result.stdout)
            if result.returncode == 0:
                raise RuntimeError("tampered bundle unexpectedly imported successfully")
            if "all prior bundle changes were rolled back" not in result.stdout:
                raise RuntimeError(
                    "failed import did not report successful whole-bundle rollback"
                )

            after = {
                name: canonical(graph_signature(db, name))
                for name in names
            }
            changed = [name for name in names if after[name] != before[name]]
            if changed:
                raise RuntimeError(
                    "whole-bundle rollback did not restore graph semantics: "
                    + ", ".join(changed)
                )

            after_udfs = parse_udf_rows(db.udf_list(with_code=True))
            if after_udfs != before_udfs:
                raise RuntimeError(
                    "whole-bundle rollback did not restore UDF source state"
                )
        finally:
            db.close()
            raw.close()

    print("BUNDLE_ATOMIC_ROLLBACK_PASS")


if __name__ == "__main__":
    main()
