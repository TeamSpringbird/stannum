#!/usr/bin/env python3
"""Check or apply the notices in source-provenance.json to tracked source files."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "source-provenance.json"
SOURCE_SUFFIXES = {".rs", ".py", ".sh", ".sql", ".pest", ".toml", ".yml", ".yaml", ".control"}
OWNERS = {
    "planetscale": ["Copyright (C) 2026 PlanetScale"],
    "ben": ["Copyright (C) 2026 Ben Weis <ben@springbird.app>"],
    "mixed": [
        "Copyright (C) 2026 Ben Weis <ben@springbird.app>",
        "Based on Lead, copyright (C) 2026 PlanetScale",
    ],
}
LICENSE_LINE = "See LICENSE in the repository root for license terms."


def is_source(path, content):
    file = Path(path)
    return (file.suffix in SOURCE_SUFFIXES or file.name == "Dockerfile"
            or content.startswith(b"#!"))


def notice(path, owner):
    lines = [*OWNERS[owner], "", LICENSE_LINE]
    suffix = Path(path).suffix
    if suffix == ".md":
        return ("<!--\n" + "\n".join(lines) + "\n-->\n\n").encode()
    prefix = "//" if suffix in {".rs", ".pest"} else "--" if suffix == ".sql" else "#"
    return ("\n".join(prefix + (" " + line if line else "") for line in lines) + "\n\n").encode()


def preamble(path, content):
    """Leave interpreter and Python encoding declarations in their required positions."""
    lines = content.splitlines(keepends=True)
    count = int(bool(lines and lines[0].startswith(b"#!")))
    if Path(path).suffix == ".py":
        for index, line in enumerate(lines[:2]):
            if re.match(br"^[ \t\f]*#.*?coding[:=][ \t]*[-\w.]+", line):
                count = max(count, index + 1)
    return b"".join(lines[:count]), b"".join(lines[count:])


def rewrite(path, content, owner):
    prefix, body = preamble(path, content)
    for known_owner in OWNERS:
        header = notice(path, known_owner)
        if body.startswith(header):
            body = body[len(header):]
            break
    else:
        # An unfamiliar notice needs human review, never automatic replacement.
        comment_lines = b"\n".join(
            line for line in body[:2048].splitlines()
            if line.lstrip().startswith((b"//", b"#", b"--", b"/*", b"*"))
            or Path(path).suffix == ".md"
        )
        if re.search(br"copyright|SPDX-License-Identifier", comment_lines, re.I):
            raise ValueError(f"{path}: existing notice requires review")
    return prefix + notice(path, owner) + body


def tracked_files(root):
    output = subprocess.check_output(["git", "ls-files", "-z"], cwd=root)
    return [name.decode() for name in output.split(b"\0") if name]


def check(root, files, entries, write=False):
    errors = []
    tracked = set(files)
    for path in sorted(set(entries) - tracked):
        errors.append(f"{path}: provenance entry has no tracked file")
    for path in files:
        file = root / path
        if file.is_symlink():
            if path in entries or Path(path).suffix in SOURCE_SUFFIXES:
                errors.append(f"{path}: source symlink requires explicit review")
            continue
        content = file.read_bytes()
        if not is_source(path, content) and path not in entries:
            continue
        entry = entries.get(path)
        if not entry:
            errors.append(f"{path}: source file has no reviewed provenance entry")
            continue
        owner = entry.get("owner")
        if owner not in OWNERS or not entry.get("evidence"):
            errors.append(f"{path}: invalid owner or missing provenance evidence")
            continue
        if owner in {"planetscale", "mixed"} and not entry.get("upstream_path"):
            errors.append(f"{path}: missing upstream source path")
            continue
        try:
            expected = rewrite(path, content, owner)
        except ValueError as error:
            errors.append(str(error))
            continue
        if content != expected:
            if write:
                file.write_bytes(expected)
            else:
                errors.append(f"{path}: missing or incorrect source header")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="apply reviewed notices in place")
    args = parser.parse_args()
    entries = json.loads(MANIFEST.read_text())["files"]
    errors = check(ROOT, tracked_files(ROOT), entries, args.write)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"Source headers {'applied' if args.write else 'checked'}: {len(entries)} files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
