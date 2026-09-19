#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Dump every immutable segment blob of a Stannum index to files.

Reads the index's relation file directly (after a CHECKPOINT), so it needs
filesystem access to the data directory: the pgrx development server runs as
the developer, which is the intended use. The blobs feed the `segment` crate's
`breakdown` example:

    script/dump-segments.py --dbname mydb --index documents_body_idx --out /tmp/blobs
    cargo run -p segment --release --example breakdown -- /tmp/blobs/*.segment

Connection settings come from the usual PG* environment variables.
"""
import argparse
import struct
import subprocess
from pathlib import Path

PAGE_SIZE = 8192
PAGE_HEADER = 24
SPECIAL_SIZE = 8
MAGIC = 0x4C445032
VERSION = 2
KIND_META = 1
KIND_RUN = 3
SPEC_BYTES = 8
NONE = 0xFFFFFFFF
FILE_BLOCKS = (1 << 30) // PAGE_SIZE
ENTRY_BYTES = 12 + 12 + 12 + 4 + 8 + 4


def psql(dbname, sql):
    result = subprocess.run(
        ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-d", dbname, "-c", sql],
        check=True, text=True, capture_output=True,
    )
    return result.stdout.strip()


class Relation:
    def __init__(self, path):
        self.path = Path(path)

    def page(self, block):
        suffix = "" if block < FILE_BLOCKS else f".{block // FILE_BLOCKS}"
        with open(str(self.path) + suffix, "rb") as file:
            file.seek((block % FILE_BLOCKS) * PAGE_SIZE)
            page = file.read(PAGE_SIZE)
        if len(page) != PAGE_SIZE:
            raise SystemExit(f"block {block}: short read")
        lower, = struct.unpack_from("<H", page, 12)
        special = PAGE_SIZE - SPECIAL_SIZE
        magic, kind, version = struct.unpack_from("<IBB", page, special)
        if magic != MAGIC or version != VERSION:
            raise SystemExit(f"block {block}: not an LDP2 page")
        return kind, page[PAGE_HEADER:lower]

    def run(self, first, blocks, nbytes):
        out = bytearray()
        block = first
        for _ in range(blocks):
            if block == NONE:
                raise SystemExit("run chain ends early")
            kind, payload = self.page(block)
            if kind != KIND_RUN:
                raise SystemExit(f"block {block}: kind {kind} is not a run page")
            block, = struct.unpack_from("<I", payload, 0)
            out += payload[4:4 + min(len(payload) - 4, nbytes - len(out))]
        if len(out) != nbytes:
            raise SystemExit(f"run: {len(out)} of {nbytes} bytes")
        return bytes(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--dbname", required=True)
    parser.add_argument("--index", required=True)
    parser.add_argument("--out", required=True, help="directory for gen<N>.segment files")
    args = parser.parse_args()
    psql(args.dbname, "CHECKPOINT")
    data_directory = psql(args.dbname, "SHOW data_directory")
    relative = psql(args.dbname, f"SELECT pg_relation_filepath('{args.index}')")
    relation = Relation(Path(data_directory) / relative)
    kind, meta = relation.page(0)
    if kind != KIND_META:
        raise SystemExit("block 0 is not the meta page")
    at = 8 + SPEC_BYTES + 28
    _next_generation, segment_count, _pending = struct.unpack_from("<III", meta, at)
    at += 12
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    total = 0
    for _ in range(segment_count):
        first, blocks, nbytes = struct.unpack_from("<III", meta, at)
        docs, total_length, generation = struct.unpack_from("<IQI", meta, at + 36)
        at += ENTRY_BYTES
        blob = relation.run(first, blocks, nbytes)
        path = out / f"gen{generation}.segment"
        path.write_bytes(blob)
        total += len(blob)
        print(f"{path}: {len(blob)} bytes in {blocks} pages, {docs} documents, "
              f"{total_length} tokens, {blob[:4].decode('ascii', 'replace')}")
    print(f"{segment_count} segments, {total} bytes")


if __name__ == "__main__":
    main()
