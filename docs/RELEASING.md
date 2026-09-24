# Releasing Stannum

Stannum starts at **0.1.0**. Before its first tag this is 0.1.0-dev in the
changelog; the build and SQL version are 0.1.0. The workspace version is the
single source of truth: pgrx substitutes it into `stannum.control`'s
`@CARGO_VERSION@`, and `stannum.version()` reports the compiled value. Earlier
0.0.0 development builds are not public releases and have no supported upgrade
path. Reload their data into a fresh database and rebuild their indexes.

## Release checklist

1. Update the workspace version and Cargo.lock, and add a dated changelog entry.
   Before 1.0, minor releases can change APIs; patch releases preserve them.
   SQL and on-disk format versions are independent.
2. Generate the release schema without the `pg_test` feature:
   `cargo pgrx schema pg18 --package stannum --no-default-features --features pg18 --out postgres/sql/stannum--VERSION.sql`.
   Add the generated snapshot to `source-provenance.json`, preserving the mixed
   attribution of its Rust inputs, and stage the new path with `git add`. Run
   `python3 script/source_headers.py --write` to restore its source notice after
   each generation (the command operates on tracked/staged paths).
   Commit the full snapshot. Retain every previously tagged snapshot unchanged.
3. For every supported predecessor, add a SQL migration such as
   `postgres/sql/stannum--0.1.0--0.2.0.sql`, or a chain of migrations leading to
   the new version. Use ALTER/CREATE OR REPLACE as appropriate; preserve user
   data, grants, extension membership and dependency identities. pgrx installs
   upgrade files from `postgres/sql/`; it generates the fresh installation SQL
   from Rust, rather than copying the snapshot. PostgreSQL executes migration
   scripts through `ALTER EXTENSION stannum UPDATE`.
4. Install the release build and run `python3 postgres/tests/extension_upgrade.py`.
   It compares generated installed SQL with the committed current snapshot,
   excluding pgrx source-location comments and independent object ordering, then creates disposable databases
   from every snapshot, upgrades all older versions, and compares extension
   members and full function definitions against a fresh generated install.
   The 0.1.0 bootstrap checks snapshot/fresh equivalence; there is no fictional
   previous public release. The next snapshot automatically activates real
   ALTER EXTENSION upgrade checks. Keep old C wrapper symbols callable until
   their old SQL objects have been replaced by the migration.
5. Run formatting, clippy, Rust and PostgreSQL tests, lifecycle checks and the
   reference oracle. CI checks PostgreSQL 17/18 on x86-64 and AArch64, including
   release schema/upgrade checks. Never ship a binary built with `pg_test`.
6. Review `docs/SECURITY.md`, changelog, SQL changes and the compatibility table.
   Tag the reviewed passing commit `vVERSION` and publish artifacts containing
   the library, control file and upgrade SQL for each supported server major.
   Install matching artifacts on replicas before upgrading primaries. Restart
   existing backends when replacing the shared library; ALTER EXTENSION does
   not unload a library already mapped into a backend.

The upgrade harness writes SQL files in the server's extension directory and
restores them afterward. It requires installation-directory write access.
On shared developer machines, wrap schema/install/test/lifecycle/upgrade commands
in the same machine-wide pgrx lock. A release workflow must also test migrations
against a database and representative indexes created by the previous tagged
*binary*: the SQL bootstrap alone cannot prove binary or data compatibility.

## On-disk compatibility

| Extension | Page signature/version read | Segment signatures read | Formats written |
| --- | --- | --- | --- |
| 0.1.0 | LDP2, VERSION 2 only | STN3 | LDP2 VERSION 2; STN3 |

`VERSION` is the special-area byte on every page, including the meta page.
The write-buffer's `version` counter is a cache invalidation generation, not a
format version. The `LSG1` to `LSG5`, `STN1` and `STN2` segment formats of
earlier development builds are not read; REINDEX writes the current page and segment formats.

Readers validate page versions before decoding and reject unknown segment
signatures. A future writer must bump the page version or segment signature
before changing its layout, so an older reader errors instead of misreading
bytes. Unknown page versions report `unsupported Stannum page version`; an
unknown segment signature reports `corrupt segment data: segment magic` with
segment generation and REINDEX guidance. Do not REINDEX with an older binary
as a downgrade procedure: restore the supported binary or rebuild from the
heap in a separately validated migration. Never overwrite a released signature
with a different encoding. pg_tests exercise future-version rejection and
REINDEX to the current format.
