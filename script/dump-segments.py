#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Dump every immutable segment blob of a Stannum index to files.

Reads the index's relation file directly (after a CHECKPOINT), so it needs
filesystem access to the data directory: the pgrx development server runs as
the developer, which is the intended use; for a server in a container, pass
the bind-mounted directory as --data-directory. The blobs feed the `segment`
crate's `breakdown` example and the `bench` crate's offline replay:

    script/dump-segments.py --dbname mydb --index documents_body_idx --out /tmp/blobs
    cargo run -p segment --release --example breakdown -- /tmp/blobs/*.segment
    script/dump-segments.py --dbname mydb --index documents_body_idx --id-column id --out /tmp/dump
    cargo run -p bench --release --bin replay -- --dump /tmp/dump --trace trace.tsv

Besides gen<N>.segment per directory entry it writes each entry's dead list
as gen<N>.dead, and manifest.tsv: the tokenizer settings the meta page
records, the BM25 and stop-word reloptions, the documents in the write
buffer (which is not dumped) and the entries in directory order. With
--id-column, ids.tsv maps every heap location of the indexed table to that
column, so the replay can report rows as the table names them.

Connection settings come from the usual PG* environment variables; --dbname
defaults to PGDATABASE.
"""
import argparse
import struct
import subprocess
from pathlib import Path

PAGE_SIZE = 8192
PAGE_HEADER = 24
SPECIAL_SIZE = 8
MAGIC = 0x4C445032
# Page layout 19 (17 to 19 changed the segments' records, not the pages).
VERSION = 19
KIND_META = 1
KIND_RUN = 3
SPEC_BYTES = 8
NONE = 0xFFFFFFFF
FILE_BLOCKS = (1 << 30) // PAGE_SIZE
# storage/layout.rs (pages as of version 16): a run is its first page, its pages,
# its bytes and its last page. An entry is the segment's run (its bytes in 64
# bits), its page table's run (no bytes: four a page of the segment), its
# dead list's run (bytes in 32 bits), the dead list's stamp, documents, total
# length, generation and origin (a byte padded to four).
WIDE_RUN_BYTES = 20
TABLE_BYTES = 12
RUN_BYTES = 16
ENTRY_BYTES = WIDE_RUN_BYTES + TABLE_BYTES + RUN_BYTES + 4 + 4 + 8 + 4 + 4
# The meta page: identity, tokenizer settings, the write buffer's version,
# epoch, head, tail, tail use, bytes and documents, then the next generation
# and the segment, pending, sealed and retired counts.
BUFFER_AT = 8 + SPEC_BYTES
COUNTS_AT = BUFFER_AT + 28
COUNTS_BYTES = 20


def psql(dbname, sql, **variables):
    """Run one statement; psql quotes the :'name' variables it references."""
    command = ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"]
    if dbname:
        command += ["-d", dbname]
    for name, value in variables.items():
        command += ["-v", f"{name}={value}"]
    result = subprocess.run(command, input=sql, check=True, text=True, capture_output=True)
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

    def run(self, first, blocks, nbytes, out=None):
        """The run's bytes, or written to the binary file `out` as they
        are read (a segment can be 16 GiB), returning its first bytes."""
        held = bytearray()
        written = 0
        block = first
        for _ in range(blocks):
            if block == NONE:
                raise SystemExit("run chain ends early")
            kind, payload = self.page(block)
            if kind != KIND_RUN:
                raise SystemExit(f"block {block}: kind {kind} is not a run page")
            block, = struct.unpack_from("<I", payload, 0)
            data = payload[4:4 + min(len(payload) - 4, nbytes - written)]
            written += len(data)
            if out is None:
                held += data
            else:
                out.write(data)
                if not held:
                    held += data[:4]
        if written != nbytes:
            raise SystemExit(f"run: {written} of {nbytes} bytes")
        return bytes(held)


def reloptions(dbname, index):
    """The index's reloptions the replay needs, as name -> text."""
    text = psql(dbname, "SELECT array_to_string(reloptions, E'\\n') FROM pg_class "
                "WHERE oid = :'index'::regclass", index=index)
    options = {}
    for line in text.splitlines():
        name, _, value = line.partition("=")
        if name in ("k1", "b", "score_stop_words"):
            options[name] = value
    return options


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--dbname", help="database name (default: PGDATABASE)")
    parser.add_argument("--index", required=True, help="index name, schema-qualified if not on search_path")
    parser.add_argument("--out", required=True, help="directory for the dump")
    parser.add_argument("--data-directory", help="the server's data directory as this host sees it, "
                        "for a server in a container whose directory is bind-mounted")
    parser.add_argument("--id-column", help="write ids.tsv mapping each heap location of the "
                        "indexed table to this column")
    args = parser.parse_args()
    psql(args.dbname, "CHECKPOINT")
    data_directory = args.data_directory or psql(args.dbname, "SHOW data_directory")
    relative = psql(args.dbname, "SELECT pg_relation_filepath(:'index'::regclass)", index=args.index)
    relation = Relation(Path(data_directory) / relative)
    kind, meta = relation.page(0)
    if kind != KIND_META:
        raise SystemExit("block 0 is not the meta page")
    spec = meta[8:8 + SPEC_BYTES]
    buffer_docs, = struct.unpack_from("<I", meta, BUFFER_AT + 24)
    _next_generation, segment_count, _pending, sealed, _retired = struct.unpack_from("<IIIII", meta, COUNTS_AT)
    at = COUNTS_AT + COUNTS_BYTES
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    manifest = ["format\tstannum-dump 1", f"spec\t{spec.hex()}"]
    manifest += [f"{name}\t{value}" for name, value in sorted(reloptions(args.dbname, args.index).items())]
    manifest.append(f"buffer_docs\t{buffer_docs}")
    total = 0
    for _ in range(segment_count):
        first, blocks, nbytes, _last = struct.unpack_from("<IIQI", meta, at)
        dead_at = at + WIDE_RUN_BYTES + TABLE_BYTES
        dead_first, dead_blocks, dead_bytes, _dead_last = struct.unpack_from("<IIII", meta, dead_at)
        docs, total_length, generation = struct.unpack_from("<IQI", meta, dead_at + RUN_BYTES + 4)
        at += ENTRY_BYTES
        path = out / f"gen{generation}.segment"
        with path.open("wb") as file:
            head = relation.run(first, blocks, nbytes, out=file)
        total += nbytes
        dead_name = "-"
        if dead_first != NONE:
            dead_name = f"gen{generation}.dead"
            (out / dead_name).write_bytes(relation.run(dead_first, dead_blocks, dead_bytes))
        manifest.append(f"segment\t{generation}\t{docs}\t{path.name}\t{dead_name}")
        print(f"{path}: {nbytes} bytes in {blocks} pages, {docs} documents, "
              f"{total_length} tokens, {head[:4].decode('ascii', 'replace')}"
              + (f", dead list of {dead_bytes} bytes" if dead_first != NONE else ""))
    (out / "manifest.tsv").write_text("\n".join(manifest) + "\n")
    if buffer_docs:
        print(f"warning: the write buffer holds {buffer_docs} documents, which are not dumped")
    if sealed:
        print(f"warning: {sealed} sealed write segments are not dumped")
    if args.id_column:
        table = psql(args.dbname, "SELECT indrelid::regclass FROM pg_index "
                     "WHERE indexrelid = :'index'::regclass", index=args.index)
        column = psql(args.dbname, "SELECT quote_ident(:'column')", column=args.id_column)
        with (out / "ids.tsv").open("w") as ids:
            subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", *(["-d", args.dbname] if args.dbname else []),
                            "-c", f"COPY (SELECT (ctid::text::point)[0]::bigint, (ctid::text::point)[1]::bigint, "
                                  f"{column} FROM {table}) TO STDOUT"],
                           check=True, stdout=ids, text=True)
    print(f"{segment_count} segments, {total} bytes")


if __name__ == "__main__":
    main()
