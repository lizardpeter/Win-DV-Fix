#!/usr/bin/env python3
"""Print PE import/delay-import DLL names using only the Python standard library."""

from __future__ import annotations

import struct
import sys
from pathlib import Path


class PeError(RuntimeError):
    pass


def u16(data: bytes, offset: int) -> int:
    if offset < 0 or offset + 2 > len(data):
        raise PeError(f"truncated PE while reading u16 at 0x{offset:x}")
    return struct.unpack_from("<H", data, offset)[0]


def u32(data: bytes, offset: int) -> int:
    if offset < 0 or offset + 4 > len(data):
        raise PeError(f"truncated PE while reading u32 at 0x{offset:x}")
    return struct.unpack_from("<I", data, offset)[0]


def c_string(data: bytes, offset: int) -> str:
    if offset < 0 or offset >= len(data):
        raise PeError(f"invalid string offset 0x{offset:x}")
    end = data.find(b"\0", offset)
    if end < 0:
        raise PeError(f"unterminated string at 0x{offset:x}")
    return data[offset:end].decode("ascii", errors="strict")


def parse(path: Path) -> list[str]:
    data = path.read_bytes()
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise PeError("missing DOS MZ header")

    pe = u32(data, 0x3C)
    if pe + 24 > len(data) or data[pe:pe + 4] != b"PE\0\0":
        raise PeError("missing PE signature")

    coff = pe + 4
    section_count = u16(data, coff + 2)
    optional_size = u16(data, coff + 16)
    optional = coff + 20
    if optional + optional_size > len(data):
        raise PeError("truncated optional header")

    magic = u16(data, optional)
    if magic == 0x20B:  # PE32+
        data_directory = optional + 112
    elif magic == 0x10B:  # PE32
        data_directory = optional + 96
    else:
        raise PeError(f"unsupported optional-header magic 0x{magic:04x}")

    section_table = optional + optional_size
    sections: list[tuple[int, int, int, int]] = []
    for index in range(section_count):
        off = section_table + index * 40
        if off + 40 > len(data):
            raise PeError("truncated section table")
        virtual_size = u32(data, off + 8)
        virtual_address = u32(data, off + 12)
        raw_size = u32(data, off + 16)
        raw_pointer = u32(data, off + 20)
        sections.append((virtual_address, max(virtual_size, raw_size), raw_pointer, raw_size))

    def rva_to_offset(rva: int) -> int:
        if rva == 0:
            raise PeError("zero RVA has no file offset")
        for virtual_address, span, raw_pointer, raw_size in sections:
            if virtual_address <= rva < virtual_address + span:
                delta = rva - virtual_address
                if delta >= raw_size:
                    raise PeError(f"RVA 0x{rva:x} points beyond section raw data")
                offset = raw_pointer + delta
                if offset >= len(data):
                    raise PeError(f"RVA 0x{rva:x} maps beyond file")
                return offset
        # Header RVAs are legal in PE images.
        if rva < section_table:
            return rva
        raise PeError(f"could not map RVA 0x{rva:x}")

    def directory(index: int) -> tuple[int, int]:
        entry = data_directory + index * 8
        if entry + 8 > optional + optional_size:
            return 0, 0
        return u32(data, entry), u32(data, entry + 4)

    imports: set[str] = set()

    # IMAGE_DIRECTORY_ENTRY_IMPORT = 1
    import_rva, import_size = directory(1)
    if import_rva and import_size:
        off = rva_to_offset(import_rva)
        limit = min(len(data), off + import_size)
        while off + 20 <= limit:
            fields = struct.unpack_from("<IIIII", data, off)
            if fields == (0, 0, 0, 0, 0):
                break
            name_rva = fields[3]
            imports.add(c_string(data, rva_to_offset(name_rva)))
            off += 20

    # IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT = 13. Delay descriptors are 32 bytes.
    delay_rva, delay_size = directory(13)
    if delay_rva and delay_size:
        off = rva_to_offset(delay_rva)
        limit = min(len(data), off + delay_size)
        while off + 32 <= limit:
            fields = struct.unpack_from("<IIIIIIII", data, off)
            if fields == (0, 0, 0, 0, 0, 0, 0, 0):
                break
            attributes, name_value = fields[0], fields[1]
            # dlattrRva (bit 0) means fields are RVAs. Modern linkers set it.
            if attributes & 1:
                name_off = rva_to_offset(name_value)
            else:
                image_base = (
                    struct.unpack_from("<Q", data, optional + 24)[0]
                    if magic == 0x20B
                    else u32(data, optional + 28)
                )
                if name_value < image_base:
                    raise PeError("invalid VA in delay import descriptor")
                name_off = rva_to_offset(name_value - image_base)
            imports.add(c_string(data, name_off))
            off += 32

    return sorted(imports, key=str.casefold)


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {Path(sys.argv[0]).name} <pe-file>", file=sys.stderr)
        return 2
    try:
        deps = parse(Path(sys.argv[1]))
    except (OSError, PeError, UnicodeError) as exc:
        print(f"PE_IMPORT_SCAN_FAILED: {exc}", file=sys.stderr)
        return 1
    for dep in deps:
        print(dep)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
