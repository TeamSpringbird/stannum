# SQL permissions and security review

All functions run as the invoker; no SECURITY DEFINER or LEAKPROOF functions
are installed. Installation requires superuser (`trusted=false`). Administrators
can grant schema USAGE and function EXECUTE to application roles as needed;
PostgreSQL's default PUBLIC EXECUTE remains appropriate for the functions below.

| SQL surface | Permission and execution policy |
| --- | --- |
| `tokenize`, `maybe_quote`, `ql_parse`, `version`, text operator | Public pure inputs; IMMUTABLE, PARALLEL SAFE. Parser/options errors become SQL errors. |
| Text-query highlighting | Public pure inputs; IMMUTABLE, PARALLEL SAFE. An omitted query requires planner binding. |
| Indexed operator and bound highlighting | Public; STABLE, PARALLEL SAFE because tokenizer settings come from catalogs/index metadata. Validate the supplied relation is Stannum before accessing its options. No table contents are returned; supplied text is analyzed. |
| `bind_query`, indexed-query input/output | Public data construction/serialization; IMMUTABLE, PARALLEL SAFE. Explicit InOutFuncs reject malformed JSON, invalid field types, missing fields, out-of-range OIDs and unknown fields. The pgrx default silently returned NULL on decoding failure and is deliberately overridden. Relation validity is checked when evaluated. |
| `segment_info`, `verify_index`, `score_inspect` | VOLATILE, PARALLEL UNSAFE because physical state can change inside a statement. Require heap ownership or table-wide SELECT (including inherited grants and pg_read_all_data); reject readers subject to RLS because physical diagnostics cannot filter index contents by policy. Column-only grants are insufficient. |
| `score`, `full_score`, `max_score` | Immutable planner placeholders, PARALLEL UNSAFE; direct execution errors. Actual bound scoring functions are VOLATILE and PARALLEL UNSAFE. Heap scoring uses invoker SPI permissions and row security. Indexed SQL scoring checks table/index association and the same index-read permission policy on statement-cache population. Direct indexed scoring rejects recovery snapshots, including those retained after promotion. |
| Access-method handler, planner support and selectivity functions | Internal PostgreSQL signatures; cannot be invoked with user-constructed internal pointers. Handler is IMMUTABLE/STRICT/PARALLEL SAFE; support functions are planner-only, and resolve the extension's own type and functions through the catalog cache, so binding a clause needs no USAGE on the `stannum` schema (row-security policies are planned as the querying role). The `get_relation_info` hook rewrites only the planner's private copy of an index predicate, reading settings of indexes the same plan already opened. |
| `corrupt_index_page`, `index_page_kinds` | Compiled only with `pg_test`, absent from release SQL. Both validate relation/heap permissions; byte mutation additionally requires superuser. |

pgrx infers STRICT for non-optional arguments. Optional input functions handle
NULL explicitly: tokenization yields no rows, highlighting yields NULL for NULL
text, and optional scoring parameters retain their defaults. They must not be
blanket-marked STRICT (NULL defaults are meaningful).

Query text is parsed as TINQL, never SQL or a pathname. Tokenizer reloptions are
validated enumerations and bounded lengths. The scoring heap fallback constructs
SQL from catalog-deparsed index expressions/predicates, quoted relation names,
and numeric OIDs; user query strings and options are not SQL-interpolated.

Storage GUCs are USERSET with bounds: write-buffer docs 1..1,000,000, build docs
1..10,000,000, write-buffer bytes 1,024..67,108,864, merge documents
0..2,147,483,647, maximum segments 1..128, tier factor 2..64. Custom-scan selection is
a USERSET boolean. These tune the caller's work without escalating privileges;
large build settings can consume substantial memory, as other PostgreSQL user
query settings can. No GUC accepts a filesystem path.

The storage Buffer guard unlocks/releases buffers on Rust unwind; PostgreSQL
resource owners handle error cleanup for server-managed resources. Corruption
helpers and lifecycle tests exercise errors followed by backend reuse. Remaining
unwrap/expect sites in production are mostly executor invariants, static C strings,
ordered-builder invariants and validated options; this is not a proof against
all corrupt input or PostgreSQL longjmp paths. In particular, raw index opens in
`storage::spec_by_oid` rely on PostgreSQL resource-owner cleanup on an error.
Changes to storage/WAL/scan ownership require a separate focused review.

Physical scoring statistics can reflect the entire indexed corpus, as ordinary
index selectivity statistics can. Applications needing isolation between tenants
should use separate indexes/tables. Do not grant diagnostics to users who should
see only a filtered view of table contents.
