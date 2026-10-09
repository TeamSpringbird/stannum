#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Compare two recorded answer directories case by case.

    python3 conformance/compare_recordings.py conformance/expected/tin-1.0.3 conformance/expected/tin-1.0.4

Prints every case whose recorded answer differs between the two directories,
capture by capture, with the kind of change (ids, count, ranked order, score
bits, highlight text, ERROR SQLSTATE or message, value, script step, crash),
then the cases only one directory holds and a count per kind. Reads only
the JSON files; connects to nothing.
"""

import argparse
import json
from pathlib import Path


def load(directory):
    source, records = None, {}
    for path in sorted(Path(directory).glob("*.json")):
        document = json.loads(path.read_text())
        source = source or document.get("source") or {}
        for record in document.get("cases") or []:
            records[record["id"]] = record
    return source, records


def brief(value, width=160):
    text = json.dumps(value, ensure_ascii=False, sort_keys=True)
    return text if len(text) <= width else text[:width - 3] + "..."


def is_error(value):
    return isinstance(value, dict) and ("error" in value or "sqlstate" in value)


def error_of(value):
    return value.get("error") or value


def classify(name, old, new):
    """The kinds of change between two answers of one capture."""
    if is_error(old) and is_error(new):
        old_error, new_error = error_of(old), error_of(new)
        if old_error.get("sqlstate") != new_error.get("sqlstate"):
            return ["error sqlstate"]
        return ["error message"]
    if is_error(old):
        return ["error -> answer"]
    if is_error(new):
        return ["answer -> error"]
    if isinstance(old, dict) and isinstance(new, dict) and "variants_disagree" in (set(old) | set(new)):
        return ["variants"]
    if name.startswith("scores") or (isinstance(old, dict) and isinstance(new, dict)
                                     and all(isinstance(v, str) for v in list(old.values()) + list(new.values()))):
        if isinstance(old, dict) and isinstance(new, dict) and set(old) == set(new):
            return ["score bits"]
        return ["matches"]
    if isinstance(old, list) and isinstance(new, list):
        if old and new and all(isinstance(x, int) for x in old + new):
            if sorted(old) == sorted(new):
                return ["order"]
            return ["ids"]
        if any(isinstance(row, list) and len(row) == 2 and isinstance(row[1], str) for row in old + new):
            if [r[0] for r in old if isinstance(r, list)] == [r[0] for r in new if isinstance(r, list)]:
                return ["highlight/value text"]
        return ["value"]
    if isinstance(old, int) and isinstance(new, int):
        return ["count"]
    return ["value"]


def compare(old_record, new_record):
    """Returns [(capture, kinds, old, new)] for one case."""
    for key in ("server_crashed", "corpus_error"):
        if bool(old_record.get(key)) != bool(new_record.get(key)):
            return [(key, [key + (" fixed" if old_record.get(key) else " new")],
                     old_record.get(key) or old_record.get("captures"),
                     new_record.get(key) or new_record.get("captures"))]
        if old_record.get(key):
            if old_record[key] != new_record[key]:
                return [(key, [key], old_record[key], new_record[key])]
            return []
    old_caps, new_caps = old_record.get("captures") or {}, new_record.get("captures") or {}
    changes = []
    for name in list(dict.fromkeys(list(old_caps) + list(new_caps))):
        if name not in old_caps or name not in new_caps:
            changes.append((name, ["capture added" if name not in old_caps else "capture removed"],
                            old_caps.get(name), new_caps.get(name)))
            continue
        old, new = old_caps[name], new_caps[name]
        if old == new:
            continue
        if isinstance(old, list) and isinstance(new, list) and name.startswith("script") or (
                name == "connection_lost"):
            kinds = []
            for step, (a, b) in enumerate(zip(old, new), 1):
                if a != b:
                    kinds.append(f"step {step}: " + ", ".join(classify("value", a, b)))
            if len(old) != len(new):
                kinds.append("step count")
            changes.append((name, kinds or ["value"], old, new))
            continue
        changes.append((name, classify(name, old, new), old, new))
    return changes


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("old")
    parser.add_argument("new")
    parser.add_argument("--width", type=int, default=160, help="truncate answers to this many characters")
    args = parser.parse_args()
    old_source, old = load(args.old)
    new_source, new = load(args.new)
    print(f"old: {old_source.get('engine')} {old_source.get('extension_version')} ({args.old})")
    print(f"new: {new_source.get('engine')} {new_source.get('extension_version')} ({args.new})")
    print()
    tally, changed = {}, 0
    for case_id in [i for i in old if i in new]:
        changes = compare(old[case_id], new[case_id])
        if not changes:
            continue
        changed += 1
        print(case_id)
        for name, kinds, before, after in changes:
            for kind in kinds:
                tally[kind.split(":")[-1].strip()] = tally.get(kind.split(":")[-1].strip(), 0) + 1
            print(f"  {name}: {', '.join(kinds)}")
            print(f"    old {brief(before, args.width)}")
            print(f"    new {brief(after, args.width)}")
    only_old = [i for i in old if i not in new]
    only_new = [i for i in new if i not in old]
    print()
    print(f"{changed} of {len([i for i in old if i in new])} cases in both differ")
    if tally:
        print("changes by kind: " + ", ".join(f"{kind} {count}" for kind, count in sorted(tally.items())))
    if only_old:
        print(f"only in {args.old}: {', '.join(only_old)}")
    if only_new:
        print(f"only in {args.new}: {', '.join(only_new)}")


if __name__ == "__main__":
    main()
