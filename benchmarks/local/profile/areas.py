# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Area breakdown and folded stacks from `perf script -F ip,sym` output.

    areas.py table PROFILE_PREFIX...        area shares + top functions per area (markdown)
    areas.py fold SCRIPT.txt OUT.folded      folded stacks (root first) for inferno-flamegraph

Each sample goes to one area: planner if any frame is under the planner;
otherwise the first frame from the leaf up that matches an area rule.
Generic frames (allocator, memcpy/memset, core iterators, TLS, pgrx/sigsetjmp
glue, the blob byte readers) are passed through, so their cost lands on the
code that asked for them (e.g. malloc in Footer::parse is footer parse; the
stitching memcpy of a footer read is footer parse).
"""
import collections
import json
import re
import sys

sys.path.insert(0, __import__("os").path.dirname(__file__))
from stacks import stacks  # noqa: E402

PLANNER = re.compile(r"^(standard_planner|pg_plan_query|planner|subquery_planner|GetCachedPlan|BuildCachedPlan)$"
                     r"|stannum::selectivity|stannum_text_restrict")
AREAS = [
    ("pins/bufmgr", r"stannum::storage::(pin_block|unpin|shared_hints|count_use|RunSource::(pin_page|release_held))"
                    r"|RunSource as segment::source::Source>::pinned_page|^ReadBuffer|^ReadRecentBuffer|^PinBuffer"
                    r"|^UnpinBuffer|^ReleaseBuffer|^BufTable|PrivateRefCount|^ResourceOwner(Forget|Remember|Enlarge|AddToHash)"
                    r"|^StartReadBuffer|^PinBufferForBlock|^WaitReadBuffers|^LockBuffer|^IncrBufferRefCount|^BufferAlloc"
                    r"|^GetVictimBuffer|^StrategyGetBuffer|^LWLock"),
    ("positions", r"PosCursor|SpanCheck|span_check|tinshape::positions::|verify_pending|boldi|Boldi"),
    ("footer/directory parse", r"Postings::footer|Footer::parse|parse_groups|resolve_memo|footer_memo|term_memo"
                               r"|Segment::resolve|TermSet::open|Postings::parse|Postings.*::parse|Segment::forget"
                               r"|Directory|directory"),
    ("scoring", r"engine::bm25|TermScorer|rank::Sc::|floor_kept|range_bound|Footer::bucket|frontier_of|read_buckets"
                r"|Lengths::(get|header)|length_at|TopRows|score_at|term_scorers|Scorer::new|bound_through"),
    ("decode", r"rank::(Walk<T>::)?(or_)?load$|tinshape::ef::|EfCursor|Ef::|varint|for_each_local|or_into"
               r"|paged_for_each|Postings::container"
               r"|segment::bits|docs::Liveness|liveness|grid"),
    ("walk", r"engine::tinshape::rank::|engine::tinshape::(intersect|Fold|TermSet::find|top_k)|engine::walk::"
             r"|Geometry::group_of_slot|Vec.*retain|NativeSource as engine::walk|stannum::customscan::top_rows"),
    ("executor", r"^Exec[A-Z]|^ExecutorRun|^standard_ExecutorRun|^heap|^heapam|visibilitymap|^tts_|^slot_|printtup"
                 r"|^ExecScan|stannum::customscan|^toast|^detoast|pglz_decompress|^index_|^table_"),
]
AREAS = [(name, re.compile(rx)) for name, rx in AREAS]
GENERIC = re.compile(r"memcpy|memmove|memset|memcmp|malloc|^free$|cfree|_int_|unlink_chunk|^__rust|^alloc::alloc"
                     r"|^alloc::raw_vec|^core::|^<core::|^<alloc::|hashbrown|_dl_tlsdesc|cee_scape|thread_check"
                     r"|run_guarded|sigsetjmp|sigjmp|libc_arm_za|^\[unknown\]|^0x[0-9a-f]+$|tinshape::blob::"
                     r"|^__aarch64_|^try_process|GenericShunt")
# RunSource::pin_page's own samples sit on its first loads of the pinned page (header at +16,
# special space at +8184): the first touch of the page's memory, a cache/TLB miss, and under
# host memory pressure a host-level fault of the VM's memory. Kept apart from pins/bufmgr.
FIRST_TOUCH = re.compile(r"stannum::storage::RunSource::pin_page$")
ORDER = ["walk", "decode", "pins/bufmgr", "page first touch", "footer/directory parse", "positions", "scoring",
         "planner", "executor", "Postgres/OS overhead"]
# Container CPU per query of the v2 campaign's runs (cgroup usage / (QPS x window)).
V2_CPU_MS = {"score-conjunction": 10.2, "full_score-conjunction": 25.0, "score-disjunction": 36.2,
             "full_score-disjunction": 129.6, "score-phrase": 14.8, "full_score-phrase": 28.8}


def area_of(stack):
    if FIRST_TOUCH.search(stack[0]):
        return "page first touch", stack[0]
    if any(PLANNER.search(f) for f in stack):
        return "planner", next((f for f in stack if not GENERIC.search(f)), stack[0])
    for f in stack:
        if GENERIC.search(f):
            continue
        for name, rx in AREAS:
            if rx.search(f):
                return name, f
    return "Postgres/OS overhead", next((f for f in stack if not GENERIC.search(f)), stack[0])


def short(sym):
    sym = re.sub(r"::_\$u7b\$\$u7b\$closure\$u7d\$\$u7d\$::h[0-9a-f]+", "::{closure}", sym)
    sym = sym.replace("$LT$", "<").replace("$GT$", ">").replace("$C$", ",")
    return sym[:90]


def analyse(path):
    total = 0
    areas = collections.Counter()
    funcs = collections.defaultdict(collections.Counter)  # area -> self leaf counter
    for s in stacks(path):
        total += 1
        area, _ = area_of(s)
        areas[area] += 1
        funcs[area][short(s[0])] += 1
    return total, areas, funcs


def table(prefixes):
    out = {}
    for p in prefixes:
        script = p + ".script.txt"
        total, areas, funcs = analyse(script if __import__("os").path.exists(script) else script + ".gz")
        conc = json.load(open(p + ".conc.json"))["summary"]
        st = [v for v in conc["styles"].values() if v["n"]][0]
        out[p] = {"total": total, "cpu_ms": st["cpu_ms_mean"], "qps": conc["qps"],
                  "areas": {a: areas[a] / total for a in ORDER},
                  "top": {a: [(f, c / total) for f, c in funcs[a].most_common(6)] for a in ORDER}}
    return out


def main():
    cmd = sys.argv[1]
    if cmd == "table":
        res = table(sys.argv[2:])
        names = [p.rsplit("/", 1)[-1].replace("v2-", "") for p in res]
        print("| area | " + " | ".join(names) + " |")
        print("| --- |" + " ---: |" * len(names))
        keys = [re.sub(r"^v2b?-", "", n) for n in names]
        v2 = [V2_CPU_MS.get(k, float("nan")) for k in keys]
        print("| backend CPU ms/query, profile run | " + " | ".join(f"{r['cpu_ms']:.1f}" for r in res.values()) + " |")
        print("| container CPU ms/query, v2 campaign | " + " | ".join(f"{c:.1f}" for c in v2) + " |")
        for a in ORDER:
            print(f"| {a} | " + " | ".join(
                f"{100 * r['areas'][a]:.1f}% ({r['areas'][a] * c:.1f})" for r, c in zip(res.values(), v2)) + " |")
        for p, r in res.items():
            print(f"\n### {p.rsplit('/', 1)[-1]} ({r['total']} samples, {r['cpu_ms']:.1f} ms CPU/query)\n")
            for a in ORDER:
                tops = ", ".join(f"{f} {100 * c:.1f}%" for f, c in r["top"][a] if c >= 0.002)
                print(f"- **{a} {100 * r['areas'][a]:.1f}%**: {tops}")
        json.dump(res, open("areas.json", "w"), indent=1)
    elif cmd == "fold":
        c = collections.Counter()
        for s in stacks(sys.argv[2]):
            area, _ = area_of(s)
            frames = [short(f).replace(";", ":") for f in reversed(s)]
            # Root at the backend: drop the postmaster frames above PostgresMain.
            if "PostgresMain" in frames:
                frames = frames[frames.index("PostgresMain"):]
            c[";".join(frames)] += 1
        with open(sys.argv[3], "w") as f:
            for k, v in c.items():
                f.write(f"{k} {v}\n")


if __name__ == "__main__":
    main()
