#!/usr/bin/env python3
"""Import a current standalone FalkorDB dump.rdb into the native Windows host.

This parser is deliberately specialized for FalkorDB graph databases. It reads
Redis RDB metadata plus module-v2 values, reassembles FalkorDB graphdata /
graphmeta v19 fragments in on-disk order, restores each graph through the
native server's migration-only GRAPH.RESTORE.RDB command, and restores UDF
library source from FalkorDB's module AUX record.

It fails loudly if ordinary non-module Redis keys or unsupported module types
are present; those cannot be represented by the standalone graph host and must
never be silently discarded.
"""

from __future__ import annotations

import argparse
import struct
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path

from falkordb import FalkorDB
from redis import Redis
from redis.exceptions import ResponseError

RDB_OPCODE_SLOT_INFO = 244
RDB_OPCODE_FUNCTION2 = 245
RDB_OPCODE_FUNCTION_PRE_GA = 246
RDB_OPCODE_MODULE_AUX = 247
RDB_OPCODE_IDLE = 248
RDB_OPCODE_FREQ = 249
RDB_OPCODE_AUX = 250
RDB_OPCODE_RESIZEDB = 251
RDB_OPCODE_EXPIRETIME_MS = 252
RDB_OPCODE_EXPIRETIME = 253
RDB_OPCODE_SELECTDB = 254
RDB_OPCODE_EOF = 255

RDB_TYPE_MODULE_PRE_GA = 6
RDB_TYPE_MODULE_2 = 7

RDB_MODULE_OPCODE_EOF = 0
RDB_MODULE_OPCODE_SINT = 1
RDB_MODULE_OPCODE_UINT = 2
RDB_MODULE_OPCODE_FLOAT = 3
RDB_MODULE_OPCODE_DOUBLE = 4
RDB_MODULE_OPCODE_STRING = 5

RDB_ENC_INT8 = 0
RDB_ENC_INT16 = 1
RDB_ENC_INT32 = 2
RDB_ENC_LZF = 3

MODULE_CHARSET = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"

TYPE_BYTES = 0
TYPE_FLOAT = 1
TYPE_DOUBLE = 2
TYPE_SIGNED = 3
TYPE_UNSIGNED = 4
TYPE_LONG_DOUBLE = 5
TYPE_BLOB = 6


@dataclass
class GraphHeader:
    name: str
    node_count: int
    edge_count: int
    key_count: int


@dataclass
class ParsedRdb:
    version: int
    graphs: "OrderedDict[str, list[bytes]]"
    headers: dict[str, GraphHeader]
    udfs: dict[str, str]


class RdbReader:
    def __init__(self, data: bytes):
        self.data = data
        self.pos = 0

    def remaining(self) -> int:
        return len(self.data) - self.pos

    def read(self, n: int) -> bytes:
        end = self.pos + n
        if n < 0 or end > len(self.data):
            raise ValueError(
                f"truncated RDB at offset {self.pos}: need {n} bytes, "
                f"have {self.remaining()}"
            )
        out = self.data[self.pos:end]
        self.pos = end
        return out

    def u8(self) -> int:
        return self.read(1)[0]

    def read_len(self) -> tuple[int, bool]:
        first = self.u8()
        kind = first >> 6
        if kind == 0:
            return first & 0x3F, False
        if kind == 1:
            return ((first & 0x3F) << 8) | self.u8(), False
        if kind == 2:
            if first == 0x80:
                return int.from_bytes(self.read(4), "big"), False
            if first == 0x81:
                return int.from_bytes(self.read(8), "big"), False
            raise ValueError(f"unknown RDB length encoding byte 0x{first:02x}")
        return first & 0x3F, True

    def string(self) -> bytes:
        value, encoded = self.read_len()
        if not encoded:
            return self.read(value)
        if value == RDB_ENC_INT8:
            return str(struct.unpack("<b", self.read(1))[0]).encode()
        if value == RDB_ENC_INT16:
            return str(struct.unpack("<h", self.read(2))[0]).encode()
        if value == RDB_ENC_INT32:
            return str(struct.unpack("<i", self.read(4))[0]).encode()
        if value == RDB_ENC_LZF:
            compressed_len, c_encoded = self.read_len()
            original_len, o_encoded = self.read_len()
            if c_encoded or o_encoded:
                raise ValueError("encoded length inside RDB LZF string")
            return lzf_decompress(self.read(compressed_len), original_len)
        raise ValueError(f"unsupported RDB string encoding {value}")


def lzf_decompress(data: bytes, expected_len: int) -> bytes:
    out = bytearray()
    ip = 0
    while ip < len(data):
        ctrl = data[ip]
        ip += 1
        if ctrl < 32:
            length = ctrl + 1
            end = ip + length
            if end > len(data):
                raise ValueError("truncated RDB LZF literal")
            out.extend(data[ip:end])
            ip = end
            continue

        length = ctrl >> 5
        high = (ctrl & 0x1F) << 8
        if length == 7:
            if ip >= len(data):
                raise ValueError("truncated RDB LZF extended length")
            length += data[ip]
            ip += 1
        if ip >= len(data):
            raise ValueError("truncated RDB LZF offset")
        low = data[ip]
        ip += 1
        length += 2
        offset = high | low
        ref = len(out) - offset - 1
        if ref < 0:
            raise ValueError("invalid RDB LZF back-reference")
        for i in range(length):
            source = ref + i
            if source < 0 or source >= len(out):
                raise ValueError("invalid overlapping RDB LZF back-reference")
            out.append(out[source])
            if len(out) > expected_len:
                raise ValueError("RDB LZF output exceeds advertised length")

    if len(out) != expected_len:
        raise ValueError(
            f"RDB LZF output length {len(out)} != expected {expected_len}"
        )
    return bytes(out)


def redis_crc64(data: bytes) -> int:
    poly = 0xAD93_D235_94C9_35A9
    crc = 0
    for byte in data:
        for mask in (1, 2, 4, 8, 16, 32, 64, 128):
            high = bool(crc & 0x8000_0000_0000_0000)
            bit = bool(byte & mask)
            crc = (crc << 1) & 0xFFFF_FFFF_FFFF_FFFF
            if high ^ bit:
                crc ^= poly
    # reverse 64 bits, matching Redis crc64.c
    value = crc
    reversed_value = 0
    for _ in range(64):
        reversed_value = (reversed_value << 1) | (value & 1)
        value >>= 1
    return reversed_value


def module_name(module_id: int) -> str:
    value = module_id >> 10
    chars = bytearray(9)
    for index in range(8, -1, -1):
        chars[index] = MODULE_CHARSET[value & 63]
        value >>= 6
    return chars.decode("ascii")


def read_module_records(reader: RdbReader) -> list[tuple[int, object]]:
    records: list[tuple[int, object]] = []
    while True:
        opcode, encoded = reader.read_len()
        if encoded:
            raise ValueError("encoded module opcode is invalid")
        if opcode == RDB_MODULE_OPCODE_EOF:
            return records
        if opcode in (RDB_MODULE_OPCODE_SINT, RDB_MODULE_OPCODE_UINT):
            value, value_encoded = reader.read_len()
            if value_encoded:
                raise ValueError("encoded module integer is invalid")
            records.append((opcode, value))
        elif opcode == RDB_MODULE_OPCODE_STRING:
            records.append((opcode, reader.string()))
        elif opcode == RDB_MODULE_OPCODE_FLOAT:
            records.append((opcode, reader.read(4)))
        elif opcode == RDB_MODULE_OPCODE_DOUBLE:
            records.append((opcode, reader.read(8)))
        else:
            raise ValueError(f"unknown Redis module opcode {opcode}")


def buffered_chunks_to_v19(chunks: list[bytes]) -> bytes:
    if not chunks:
        raise ValueError("FalkorDB module value contains no serializer chunks")

    out = bytearray()
    chunk_index = 0
    while chunk_index < len(chunks):
        chunk = chunks[chunk_index]
        pos = 0
        while pos < len(chunk):
            tag = chunk[pos]
            pos += 1
            if tag == TYPE_BYTES:
                if pos + 8 > len(chunk):
                    raise ValueError("truncated FalkorDB byte-buffer length")
                length = int.from_bytes(chunk[pos:pos + 8], "little")
                pos += 8
                end = pos + length
                if end > len(chunk):
                    raise ValueError("truncated FalkorDB byte buffer")
                out.append(TYPE_BYTES)
                out.extend(length.to_bytes(8, "little"))
                out.extend(chunk[pos:end])
                pos = end
            elif tag == TYPE_FLOAT:
                end = pos + 4
                if end > len(chunk):
                    raise ValueError("truncated FalkorDB float")
                out.append(tag)
                out.extend(chunk[pos:end])
                pos = end
            elif tag in (TYPE_DOUBLE, TYPE_SIGNED, TYPE_UNSIGNED):
                end = pos + 8
                if end > len(chunk):
                    raise ValueError("truncated FalkorDB fixed-width value")
                out.append(tag)
                out.extend(chunk[pos:end])
                pos = end
            elif tag == TYPE_LONG_DOUBLE:
                raise ValueError(
                    "unexpected long-double tag in current FalkorDB graphdata v19"
                )
            elif tag == TYPE_BLOB:
                if pos != len(chunk):
                    raise ValueError(
                        "FalkorDB blob sentinel was not at end of serializer chunk"
                    )
                chunk_index += 1
                if chunk_index >= len(chunks):
                    raise ValueError("FalkorDB blob sentinel has no following chunk")
                blob = chunks[chunk_index]
                out.append(TYPE_BYTES)
                out.extend(len(blob).to_bytes(8, "little"))
                out.extend(blob)
                break
            else:
                raise ValueError(f"unknown FalkorDB serializer type tag {tag}")
        chunk_index += 1

    return bytes(out)


class V19Reader:
    def __init__(self, data: bytes):
        self.data = data
        self.pos = 0

    def _take(self, n: int) -> bytes:
        end = self.pos + n
        if end > len(self.data):
            raise ValueError("truncated FalkorDB v19 fragment header")
        out = self.data[self.pos:end]
        self.pos = end
        return out

    def buffer(self) -> bytes:
        if self._take(1)[0] != TYPE_BYTES:
            raise ValueError("FalkorDB v19 header expected byte-buffer tag")
        length = int.from_bytes(self._take(8), "little")
        return self._take(length)

    def unsigned(self) -> int:
        if self._take(1)[0] != TYPE_UNSIGNED:
            raise ValueError("FalkorDB v19 header expected unsigned tag")
        return int.from_bytes(self._take(8), "little")


def parse_fragment_header(payload: bytes) -> GraphHeader:
    r = V19Reader(payload)
    name_bytes = r.buffer()
    if name_bytes.endswith(b"\0"):
        name_bytes = name_bytes[:-1]
    name = name_bytes.decode("utf-8")
    node_count = r.unsigned()
    edge_count = r.unsigned()
    _deleted_nodes = r.unsigned()
    _deleted_edges = r.unsigned()
    _label_count = r.unsigned()
    relationship_count = r.unsigned()
    for _ in range(relationship_count):
        r.unsigned()
    key_count = r.unsigned()
    return GraphHeader(name, node_count, edge_count, key_count)


def parse_falkordb_aux(
    module: str,
    encver: int,
    records: list[tuple[int, object]],
    udfs: dict[str, str],
) -> None:
    if module != "graphdata":
        raise ValueError(
            f"RDB contains unsupported module AUX data for {module!r}"
        )
    if encver != 19:
        raise ValueError(
            f"RDB contains FalkorDB graphdata AUX version {encver}; "
            "current raw importer requires v19"
        )

    # First record after the module id is Redis' AUX 'when' marker.
    if len(records) < 2:
        return
    when_opcode, _when = records[0]
    if when_opcode != RDB_MODULE_OPCODE_UINT:
        raise ValueError("FalkorDB module AUX has invalid when opcode")

    body = records[1:]
    if not body:
        return
    if body[0][0] != RDB_MODULE_OPCODE_UINT:
        raise ValueError("FalkorDB module AUX expected UDF count")
    count = int(body[0][1])

    # AFTER_RDB writes one unsigned zero placeholder. BEFORE_RDB with zero UDFs
    # is intentionally indistinguishable and also needs no work.
    if count == 0:
        return

    expected = 1 + count * 2
    if len(body) != expected:
        raise ValueError(
            f"FalkorDB UDF AUX declared {count} libraries but has "
            f"{len(body) - 1} value records"
        )
    for i in range(count):
        name_rec = body[1 + i * 2]
        script_rec = body[2 + i * 2]
        if (
            name_rec[0] != RDB_MODULE_OPCODE_STRING
            or script_rec[0] != RDB_MODULE_OPCODE_STRING
        ):
            raise ValueError("FalkorDB UDF AUX expected name/script strings")
        name = bytes(name_rec[1]).rstrip(b"\0").decode("utf-8")
        script = bytes(script_rec[1]).rstrip(b"\0").decode("utf-8")
        existing = udfs.get(name)
        if existing is not None and existing != script:
            raise ValueError(f"conflicting UDF definitions for {name!r}")
        udfs[name] = script


def parse_rdb(path: Path) -> ParsedRdb:
    data = path.read_bytes()
    if len(data) < 17 or not data.startswith(b"REDIS"):
        raise ValueError("not a Redis RDB file")
    try:
        version = int(data[5:9].decode("ascii"))
    except ValueError as exc:
        raise ValueError("invalid Redis RDB version header") from exc
    if version < 5 or version > 15:
        raise ValueError(f"unsupported Redis RDB version {version}")

    stored_crc = int.from_bytes(data[-8:], "little")
    if stored_crc:
        actual_crc = redis_crc64(data[:-8])
        if actual_crc != stored_crc:
            raise ValueError(
                f"RDB CRC64 mismatch: expected {stored_crc:016x}, "
                f"computed {actual_crc:016x}"
            )

    reader = RdbReader(data[:-8])
    magic = reader.read(9)
    if magic != data[:9]:
        raise ValueError("RDB header read mismatch")

    graphs: "OrderedDict[str, list[bytes]]" = OrderedDict()
    headers: dict[str, GraphHeader] = {}
    udfs: dict[str, str] = {}
    current_db = 0

    while True:
        record_type = reader.u8()
        if record_type == RDB_OPCODE_EOF:
            if reader.remaining() != 0:
                raise ValueError(
                    f"unexpected {reader.remaining()} bytes after RDB EOF"
                )
            break

        if record_type == RDB_OPCODE_AUX:
            reader.string()
            reader.string()
            continue
        if record_type == RDB_OPCODE_RESIZEDB:
            reader.read_len()
            reader.read_len()
            continue
        if record_type == RDB_OPCODE_SELECTDB:
            current_db, encoded = reader.read_len()
            if encoded:
                raise ValueError("encoded SELECTDB value")
            continue
        if record_type == RDB_OPCODE_EXPIRETIME_MS:
            reader.read(8)
            continue
        if record_type == RDB_OPCODE_EXPIRETIME:
            reader.read(4)
            continue
        if record_type == RDB_OPCODE_IDLE:
            reader.read_len()
            continue
        if record_type == RDB_OPCODE_FREQ:
            reader.read(1)
            continue
        if record_type == RDB_OPCODE_SLOT_INFO:
            reader.read_len()
            reader.read_len()
            reader.read_len()
            continue
        if record_type == RDB_OPCODE_FUNCTION_PRE_GA:
            raise ValueError("pre-release Redis function RDB format is unsupported")
        if record_type == RDB_OPCODE_FUNCTION2:
            # Redis functions are not FalkorDB graph/UDF libraries. The native
            # graph host cannot preserve them, so fail rather than discard.
            code = reader.string()
            raise ValueError(
                "RDB contains a Redis FUNCTION library "
                f"({len(code)} bytes); native graph host cannot preserve it"
            )
        if record_type == RDB_OPCODE_MODULE_AUX:
            module_id, encoded = reader.read_len()
            if encoded:
                raise ValueError("encoded module AUX id")
            records = read_module_records(reader)
            parse_falkordb_aux(
                module_name(module_id),
                module_id & 1023,
                records,
                udfs,
            )
            continue

        # Key-value record: the type byte is followed by key then value.
        key = reader.string()
        key_text = key.decode("utf-8", errors="replace")

        if current_db != 0:
            raise ValueError(
                f"RDB contains key {key_text!r} in Redis DB {current_db}; "
                "native FalkorDB host has one graph catalog and raw import "
                "currently requires DB 0"
            )

        if record_type == RDB_TYPE_MODULE_PRE_GA:
            raise ValueError(
                f"key {key_text!r} uses unsupported pre-GA Redis module format"
            )
        if record_type != RDB_TYPE_MODULE_2:
            raise ValueError(
                f"RDB contains ordinary Redis key {key_text!r} of type "
                f"{record_type}; raw FalkorDB import refuses to silently drop "
                "non-graph Redis data"
            )

        module_id, encoded = reader.read_len()
        if encoded:
            raise ValueError(f"encoded module id for key {key_text!r}")
        module = module_name(module_id)
        encver = module_id & 1023
        records = read_module_records(reader)

        if module not in ("graphdata", "graphmeta"):
            raise ValueError(
                f"RDB key {key_text!r} uses unsupported module {module!r}"
            )
        if encver != 19:
            raise ValueError(
                f"RDB key {key_text!r} uses FalkorDB {module} encoding "
                f"v{encver}; current raw importer requires v19"
            )
        chunks = [
            bytes(value)
            for opcode, value in records
            if opcode == RDB_MODULE_OPCODE_STRING
        ]
        if len(chunks) != len(records):
            raise ValueError(
                f"FalkorDB graph key {key_text!r} contains unexpected "
                "non-string Redis module records"
            )

        payload = buffered_chunks_to_v19(chunks)
        header = parse_fragment_header(payload)
        previous = headers.get(header.name)
        if previous is None:
            headers[header.name] = header
            graphs[header.name] = []
        else:
            if (
                previous.node_count != header.node_count
                or previous.edge_count != header.edge_count
                or previous.key_count != header.key_count
            ):
                raise ValueError(
                    f"inconsistent RDB fragment header for graph {header.name!r}"
                )
        graphs[header.name].append(payload)

    for name, fragments in graphs.items():
        expected = headers[name].key_count
        if len(fragments) != expected:
            raise ValueError(
                f"graph {name!r} is incomplete in RDB: "
                f"{len(fragments)}/{expected} fragments"
            )

    if not graphs:
        raise ValueError("RDB contains no current FalkorDB graphdata/graphmeta keys")

    return ParsedRdb(version, graphs, headers, udfs)


def endpoint_kwargs(args) -> dict:
    kwargs = {
        "host": args.destination_host,
        "port": args.destination_port,
        "username": args.destination_username,
        "password": args.destination_password,
        "decode_responses": False,
        "protocol": 2,
        "socket_connect_timeout": 10,
        "socket_timeout": 600,
        "ssl": args.destination_ssl,
    }
    if args.destination_ssl:
        if args.destination_ca:
            kwargs["ssl_ca_certs"] = args.destination_ca
            kwargs["ssl_cert_reqs"] = "required"
        if args.destination_cert:
            kwargs["ssl_certfile"] = args.destination_cert
        if args.destination_key:
            kwargs["ssl_keyfile"] = args.destination_key
    return kwargs


def falkor_kwargs(args) -> dict:
    kwargs = endpoint_kwargs(args)
    kwargs.pop("decode_responses", None)
    kwargs.pop("protocol", None)
    return kwargs


def import_rdb(args) -> None:
    parsed = parse_rdb(args.rdb)
    print(
        f"parsed Redis RDB v{parsed.version}: "
        f"{len(parsed.graphs)} graph(s), {len(parsed.udfs)} UDF library/libraries"
    )

    raw = Redis(**endpoint_kwargs(args))
    db = FalkorDB(**falkor_kwargs(args))
    try:
        mode = raw.info(section="server").get("redis_mode")
        if isinstance(mode, bytes):
            mode = mode.decode()
        if mode not in (None, "standalone"):
            raise RuntimeError(
                f"destination redis_mode={mode!r}; raw import requires standalone"
            )

        existing = {
            v.decode("utf-8") if isinstance(v, bytes) else str(v)
            for v in raw.execute_command("GRAPH.LIST")
        }
        conflicts = sorted(set(parsed.graphs) & existing)
        if conflicts:
            raise RuntimeError(
                "destination already contains graph(s): "
                + ", ".join(repr(v) for v in conflicts)
                + "; raw dump.rdb import is intentionally non-destructive"
            )

        imported: list[str] = []
        try:
            for name, fragments in parsed.graphs.items():
                print(
                    f"importing graph {name!r}: "
                    f"{len(fragments)}/{parsed.headers[name].key_count} fragments"
                )
                reply = raw.execute_command("GRAPH.RESTORE.RDB", *fragments)
                restored = (
                    reply.decode("utf-8") if isinstance(reply, bytes) else str(reply)
                )
                if restored != name:
                    raise RuntimeError(
                        f"GRAPH.RESTORE.RDB returned {restored!r}, expected {name!r}"
                    )

                graph = db.select_graph(name)
                nodes = graph.ro_query("MATCH (n) RETURN count(n)").result_set[0][0]
                edges = graph.ro_query("MATCH ()-[r]->() RETURN count(r)").result_set[0][0]
                header = parsed.headers[name]
                if nodes != header.node_count or edges != header.edge_count:
                    raise RuntimeError(
                        f"count verification failed for {name!r}: "
                        f"nodes {nodes}/{header.node_count}, "
                        f"edges {edges}/{header.edge_count}"
                    )
                imported.append(name)
                print(
                    f"verified {name!r}: nodes={nodes}, relationships={edges}"
                )

            if not args.skip_udfs:
                existing_udfs = {
                    str(row[1].decode() if isinstance(row[1], bytes) else row[1])
                    for row in db.udf_list(with_code=True)
                    if len(row) >= 2
                }
                conflicts = sorted(set(parsed.udfs) & existing_udfs)
                if conflicts:
                    raise RuntimeError(
                        "destination already contains UDF library/libraries: "
                        + ", ".join(repr(v) for v in conflicts)
                    )
                for name, script in parsed.udfs.items():
                    db.udf_load(name, script, False)
                    print(f"restored UDF library {name!r}")

        except Exception:
            # Raw import is non-destructive. Remove graphs created by this run
            # if a later graph/UDF verification fails.
            for name in reversed(imported):
                try:
                    raw.execute_command("GRAPH.DELETE", name)
                except Exception:
                    pass
            raise

        print(
            f"RAW_RDB_IMPORT_COMPLETE graphs={len(imported)} "
            f"udfs={0 if args.skip_udfs else len(parsed.udfs)}"
        )
    finally:
        db.close()
        raw.close()


def inspect_rdb(args) -> None:
    parsed = parse_rdb(args.rdb)
    print(f"Redis RDB version: {parsed.version}")
    for name, fragments in parsed.graphs.items():
        header = parsed.headers[name]
        print(
            f"graph {name!r}: fragments={len(fragments)}/{header.key_count} "
            f"nodes={header.node_count} relationships={header.edge_count}"
        )
    for name in sorted(parsed.udfs):
        print(f"UDF {name!r}")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Import a current FalkorDB dump.rdb into the native host"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    inspect = sub.add_parser("inspect")
    inspect.add_argument("--rdb", type=Path, required=True)

    imp = sub.add_parser("import")
    imp.add_argument("--rdb", type=Path, required=True)
    imp.add_argument("--destination-host", required=True)
    imp.add_argument("--destination-port", type=int, default=6379)
    imp.add_argument("--destination-username")
    imp.add_argument("--destination-password")
    imp.add_argument("--destination-ssl", action="store_true")
    imp.add_argument("--destination-ca")
    imp.add_argument("--destination-cert")
    imp.add_argument("--destination-key")
    imp.add_argument("--skip-udfs", action="store_true")

    args = parser.parse_args()
    if args.command == "inspect":
        inspect_rdb(args)
    else:
        import_rdb(args)


if __name__ == "__main__":
    try:
        main()
    except (ResponseError, ValueError, RuntimeError) as exc:
        raise SystemExit(f"RAW_RDB_IMPORT_FAILED: {exc}")
