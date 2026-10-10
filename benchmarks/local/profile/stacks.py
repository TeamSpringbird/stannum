"""Folds `perf script` output and answers questions about it.

    stacks.py SCRIPT.txt incl REGEX...        inclusive share of frames matching each regex
    stacks.py SCRIPT.txt callers REGEX [N]    self samples in frames matching REGEX, by caller chain (N frames up)
    stacks.py SCRIPT.txt under REGEX [TOP]    self samples of leaf frames under a frame matching REGEX
"""
import collections
import re
import sys


def stacks(path):
    cur = []
    import gzip
    f = gzip.open(path, "rt", errors="replace") if path.endswith(".gz") else open(path, errors="replace")
    for line in f:
        line = line.rstrip("\n")
        if not line.strip():
            if cur:
                yield cur
            cur = []
            continue
        if line[0] not in " \t":
            cur = []
            continue
        parts = line.strip().split(None, 1)
        if len(parts) < 2:
            continue
        sym = parts[1].rsplit(" (", 1)[0]
        sym = re.sub(r"\+0x[0-9a-f]+$", "", sym)
        if not cur or cur[-1] != sym:
            cur.append(sym)
    if cur:
        yield cur


def main():
    path, cmd = sys.argv[1], sys.argv[2]
    all_stacks = list(stacks(path))
    total = len(all_stacks)
    print(f"{total} samples")
    if cmd == "incl":
        for rx in sys.argv[3:]:
            r = re.compile(rx)
            n = sum(1 for s in all_stacks if any(r.search(f) for f in s))
            print(f"{100 * n / total:6.2f}%  {rx}")
    elif cmd == "callers":
        r = re.compile(sys.argv[3])
        depth = int(sys.argv[4]) if len(sys.argv) > 4 else 3
        c = collections.Counter()
        for s in all_stacks:
            if r.search(s[0]):
                c[" <- ".join(s[: depth + 1])] += 1
        for k, v in c.most_common(40):
            print(f"{100 * v / total:6.2f}%  {k}")
    elif cmd == "children":
        r = re.compile(sys.argv[3])
        c = collections.Counter()
        n = 0
        for s in all_stacks:
            idx = [i for i, f in enumerate(s) if r.search(f)]
            if idx:
                n += 1
                i = idx[-1]  # outermost match
                c[s[i - 1] if i > 0 else "(self)"] += 1
        print(f"under: {100 * n / total:.2f}%")
        for k, v in c.most_common(int(sys.argv[4]) if len(sys.argv) > 4 else 30):
            print(f"{100 * v / total:6.2f}%  {k}")
    elif cmd == "under":
        r = re.compile(sys.argv[3])
        top = int(sys.argv[4]) if len(sys.argv) > 4 else 40
        c = collections.Counter()
        n = 0
        for s in all_stacks:
            if any(r.search(f) for f in s):
                n += 1
                c[s[0]] += 1
        print(f"under: {100 * n / total:.2f}%")
        for k, v in c.most_common(top):
            print(f"{100 * v / total:6.2f}%  {k}")


if __name__ == "__main__":
    main()
