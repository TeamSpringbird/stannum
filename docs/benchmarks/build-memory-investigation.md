# Construction memory: hypotheses and next probes

The 500,000-document published Wikipedia pilot was OOM-killed during CREATE
INDEX with a 512 MiB container limit, while construction at 2 GiB completed.
The same index subsequently answered the full query trace under 512 MiB.
See [the retained pilot](published-memory.md). Construction and query memory
must be measured separately.

## Ranked hypotheses

1. **Accumulated segment postings dominate.** The build flushes after 32,768
   nonempty documents, irrespective of bytes or token count. `SegmentBuilder`
   retains term strings, occurrence vectors and position vectors. Prediction:
   lowering `stannum.build_segment_docs` reduces the peak during accumulation,
   especially for long documents.
2. **Merging dominates later peaks.** Build flushes call maintenance with an
   unlimited document budget. Direct merges retain encoded input blobs while
   constructing output; admission checks are format bounds, not memory bounds.
   Prediction: peaks align with larger tier merges and can persist even after
   reducing initial batch size. Instrument accumulation, encoding and merging
   separately before attributing a container peak to any one of them.
3. **Encoding or allocator retention amplifies the peak.** Encoding holds output
   areas and creates the final blob; released allocations need not immediately
   leave resident memory. Prediction: live allocated bytes fall after a flush
   while backend RSS remains elevated, or output assembly sets the peak.
4. **Shared buffers and page cache consume the remaining headroom.** Prediction:
   file/shared-memory counters explain most of the cgroup pressure, while live
   Rust allocations stay modest. Cgroup anonymous memory alone cannot identify
   an allocation site or distinguish live allocations from retained arenas.

The four repeated 500,000-document builds sampled 1,155–1,203 MiB of
container anonymous memory. This weakens the page-cache-only explanation, but
does not identify an allocation site. The one-million-document build also passes
at 2 GiB, with a 1,234 MiB sampled anonymous peak; that single step does not
show linear peak-memory growth with corpus size.

These are source-derived hypotheses, not completed allocation profiles.
`maintenance_work_mem=64MB` does not constitute a 64 MiB limit for this Rust
builder: its flush condition is the document-count setting.

## Controlled follow-up

Keep the image, exact input prefix, CPU limit and PostgreSQL settings fixed.
First compare build batches of 32,768 and 8,192 documents under the same 2 GiB
cap, retaining construction time, sampled anonymous/file memory, final index
size, segment layout and selected correctness results. Capture phase-specific
allocation evidence in an untimed diagnostic build. Only then retry the lower
cap, retaining failures and ensuring temporary resources are cleaned up.

Do not ship a smaller default solely because it uses less construction memory.
Different segment counts can change ranked query cost and merge work; rerun
query controls and sustained writes/VACUUM before adopting a change. A byte
budget or streaming merge may ultimately be needed, but neither is proved by
the current measurements.
