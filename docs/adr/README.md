# Architecture decision records

Each record states a decision that is in force, why it was made and what
follows from it. The [storage guide](../architecture/segmented-storage.md)
describes the system as a whole. A record whose decision no longer holds is
deleted rather than kept, except that a record superseded by one still being
implemented stays, marked `superseded`, for as long as the code follows it.

| ADR | Decision | Status |
| --- | --- | --- |
| [0003](0003-address-postings-by-document-ordinal.md) | Address a term's documents by ordinal into the segment's document table | Superseded by 0005; in force until it is integrated |
| [0004](0004-rank-by-document-ordinal.md) | Rank over the ordinal streams with per-chunk and per-sub-block score bounds | Superseded by 0005; extends 0003 |
| [0005](0005-address-postings-by-ctid.md) | Address postings by heap ctid, in TIN's shape (footer, payload, TF tail, DL sidecar, liveness) | Accepted; supersedes 0003 and 0004 |

ADR numbers are identifiers and are never reused; 0001 and 0002 were removed
when their decisions stopped holding. New records take the next number, 0006,
and a front-matter `status` (`proposed`, `accepted` or `superseded`), with
`extends`, `supersedes` or `superseded-by` naming other records by number.
