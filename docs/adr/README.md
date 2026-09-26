# Architecture decision records

Each record states a decision that is in force, why it was made and what
follows from it. The [storage guide](../architecture/segmented-storage.md)
describes the system as a whole. A record whose decision no longer holds is
deleted rather than kept.

| ADR | Decision | Status |
| --- | --- | --- |
| [0003](0003-address-postings-by-document-ordinal.md) | Address a term's documents by ordinal into the segment's document table | Accepted |
| [0004](0004-rank-by-document-ordinal.md) | Rank over the ordinal streams with per-chunk and per-sub-block score bounds | Accepted; extends 0003 |

ADR numbers are identifiers and are never reused; 0001 and 0002 were removed
when their decisions stopped holding. New records take the next number, 0005,
and a front-matter `status` (`proposed` or `accepted`), with `extends` naming
another record by number.
