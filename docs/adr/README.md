# Architecture decision records

Each record states a decision, the evidence behind it and its consequences at
the time it was made. A record is not rewritten when the code moves on; its
front matter says whether it is still in force. The
[storage guide](../architecture/segmented-storage.md) describes the current
system.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-preserve-posting-order-before-changing-encoding.md) | Merge segments by preserving sorted posting order, before changing the encoding | Accepted; its encoding decision superseded by 0003 |
| [0002](0002-batched-posting-execution-before-format-migration.md) | Batch Boolean execution over physical heap-page bitmaps before migrating the format | Superseded by 0003 |
| [0003](0003-address-postings-by-document-ordinal.md) | Address a term's documents by ordinal into the segment's document table | Accepted |
| [0004](0004-rank-by-document-ordinal.md) | Rank over the ordinal streams with per-chunk score bounds | Accepted; extends 0003 |

New records take the next number and a front-matter `status` (`proposed`,
`accepted` or `superseded`), with `supersedes`, `superseded-by` or `extends`
naming other records by number.
