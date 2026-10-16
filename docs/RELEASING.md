# Releasing Stannum

Stannum starts at **0.1.0**. Before its first tag, changes accumulate under
`[Unreleased]` in the changelog; the build and SQL version are 0.1.0. The workspace version is the
single source of truth: pgrx substitutes it into `stannum.control`'s
`@CARGO_VERSION@`, and `stannum.version()` reports the compiled value. Earlier
0.0.0 development builds are not public releases and have no supported upgrade
path. Reload their data into a fresh database and rebuild their indexes.

## Release checklist

1. Update the workspace version and Cargo.lock, and rename the changelog's
   `[Unreleased]` section to the version and date, leaving an empty one above.
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
4. Install the release build (`script/install-release`) and run `python3 postgres/tests/extension_upgrade.py`.
   It compares generated installed SQL with the committed current snapshot,
   excluding pgrx source-location comments and independent object ordering, then creates disposable databases
   from every snapshot, upgrades all older versions, and compares extension
   members and full function definitions against a fresh generated install.
   The 0.1.0 bootstrap checks snapshot/fresh equivalence; there is no fictional
   previous public release. The next snapshot automatically activates real
   ALTER EXTENSION upgrade checks. Keep old C wrapper symbols callable until
   their old SQL objects have been replaced by the migration.
5. Run `script/test-all full` (every tier is listed in `docs/testing.md`) and the
   reference oracle. CI checks PostgreSQL 17/18 on x86-64 and AArch64, including
   release schema/upgrade checks. Never ship a binary built with `pg_test`.
6. Review `docs/SECURITY.md`, changelog, SQL changes and the compatibility table.
   Tag the reviewed passing commit `vVERSION` and publish artifacts containing
   the library, control file and upgrade SQL for each supported server major
   (`script/install-release --stage DIR` builds that tree). On x86-64 Linux the
   library is three files, the loader and its two builds, installed together.
   Install matching artifacts on replicas before upgrading primaries. Restart
   existing backends when replacing the shared library; ALTER EXTENSION does
   not unload a library already mapped into a backend.

The upgrade harness writes SQL files in the server's extension directory and
restores them afterward. It requires installation-directory write access.
On shared developer machines, wrap schema/install/test/lifecycle/upgrade commands
in the same machine-wide pgrx lock. A release workflow must also test migrations
against a database and representative indexes created by the previous tagged
*binary*: the SQL bootstrap alone cannot prove binary or data compatibility.

## x86-64 builds and the loader

On x86-64 Linux, `script/install-release` (and so `script/test-all install`)
builds the extension twice and installs three files in `pkglibdir`:

| file | what it is |
| --- | --- |
| `stannum.so` | the loader, `postgres/loader/loader.c`, a few KB |
| `stannum-x86-64.so` | the baseline build, which runs on any x86-64 CPU |
| `stannum-x86-64-v4.so` | the same source, its crates built with `-C target-cpu=x86-64-v4` |

The v4 build runs AVX-512, BMI2, POPCNT and LZCNT wherever the compiler
finds a use, not only in the word kernels. On the 150M replay with both
hyperthreads of one core busy, as a server's backends keep them
([the paired gate](benchmarks.md#the-hyperthread-paired-replay); Xeon
8488C, 2026-10-16, three rounds), it ran 1.7% more queries per gigacycle
than the baseline: conjunctions +0.1%, phrases +0.9%, disjunctions +0.4%;
with `full_score` +2.3%, +2.1% and +4.7%. Every round agreed within 0.1
point. The kernels choose AVX2 or AVX-512 at runtime in both builds, as
before.

PostgreSQL loads `$libdir/stannum` for `CREATE EXTENSION`, the first call of
any Stannum function in a backend, `shared_preload_libraries` and the
maintenance workers, and each time it finds the loader. The loader's
`_PG_init`:

1. reads CPUID and XCR0 (`postgres/loader/cpu_level.h`): x86-64-v4 is every
   feature rustc enables for `-C target-cpu=x86-64-v4` (AVX-512 F, BW, CD,
   DQ and VL, AVX2, BMI1/2, FMA, F16C, LZCNT, MOVBE, POPCNT, SSE4.2, ...)
   and an OS that saves the AVX-512 registers;
2. `dlopen`s the matching build from its own directory with `RTLD_LOCAL`
   (the v4 build if it is there, else the baseline);
3. checks that build's magic block against its own (same server major and
   ABI);
4. points its trampolines at the build's symbols and calls the build's
   `_PG_init`.

PostgreSQL resolves each SQL function's C symbol in the library it opened,
the loader, so the loader exports one trampoline for every function symbol
the builds export (the `#[pg_extern]` wrappers, their `pg_finfo_` records,
the background workers' entry points): an `endbr64` and a jump through a
table. A jump leaves every argument register as it was, so one form serves
any signature, and nothing returns through the loader. What the build hands
PostgreSQL (the access method's routine table, hooks, custom scan methods)
points into the build. The list comes from the baseline build's `nm -D`;
`install-release` fails if the v4 build exports another list, or if the
loader does not export exactly that list with `_PG_init` and
`Pg_magic_func`. It links the loader with `-Bsymbolic`, so that its own
references cannot bind to another extension's `_PG_init` or
`Pg_magic_func`.

Settings, in the server's environment:

- `STANNUM_LOADER_LEVEL`: `auto` (unset), `x86-64` (the baseline on any
  CPU) or `x86-64-v4`. Asking for v4 on a CPU without it is an error at
  load (a server preloading Stannum does not start) rather than SIGILL in a
  backend.
- `STANNUM_LOADER_HIDE`: feature names to take as missing, for tests
  (`avx512f` makes a v4 CPU load the baseline).
- `SHOW stannum.cpu_level` reports the level the running build was
  compiled for: `x86-64`, `x86-64-v4`, or `aarch64`.

The v4 build sets the level through `script/rustc-cpu-level`, a
`RUSTC_WRAPPER` that adds `-C target-cpu=x86-64-v4` to the workspace's
crates only (by their `CARGO_MANIFEST_DIR`). A per-target `RUSTFLAGS` would
also compile build scripts and proc macros for v4, which then fault on a
build host without it (CI's runners). `--target` would keep them out, and
so would `RUSTC_WORKSPACE_WRAPPER`, but cargo hashes either into each
workspace crate's `-C metadata`, which renames every function, and the
baseline's PGO profile then names none of the v4 build's (0 of 40 hot
functions, measured). `install-release` fails if the two builds share no
mangled engine or segment function name. Dependencies outside the
workspace keep the baseline, except what they instantiate in the
workspace's crates.

`STANNUM_LEVELS=x86-64` (or `--levels x86-64`) builds and installs the
baseline alone as `stannum.so`, as before the loader; tools that swap
`stannum.so` between two builds (`benchmarks/paired_libraries.py`) need
that, since a loader runs the builds installed beside it. aarch64 and macOS
build once, as before. Profile-guided builds take
`benchmarks/pgo/x86_64-v4.profdata` for the v4 build when there is one
(`script/pgo-build image --target-cpu x86-64-v4`, trained on a CPU with
AVX-512), else the baseline's profile; `script/pgo-check` reports the
coverage of each build. The baseline's `x86_64.profdata` covers 35 of its
40 hot functions in the v4 build (2026-10-16; the other five are the
weighing and Elias-Fano kernels, whose control flow `cfg(target_feature)`
changes), so `--pgo strict` fails on the loader's builds until an
`x86_64-v4` profile is trained.

`postgres/tests/test_loader.py` checks the choice on hand-built CPUs (each
feature missing in turn, XCR0 without the AVX-512 state, short CPUID
leaves, forced and hidden settings), against `rustc --print cfg`, and on
x86-64 Linux against `/proc/cpuinfo`. `postgres/tests/loader_shim.py` starts
a server under each setting and checks the running build, the maintenance
launcher and identical answers. `benchmarks/aws/devbox-loader.sh` runs both,
the conformance suite under each build and the kernels of a v4 build at
each level on a devbox with AVX-512.

## On-disk compatibility

| Extension | Page signature/version read | Segment signatures read | Formats written |
| --- | --- | --- | --- |
| 0.1.0 | LDP2, VERSION 2 only | STN3 | LDP2 VERSION 2; STN3 |

`VERSION` is the special-area byte on every page, including the meta page.
The write-buffer's `version` counter is a cache invalidation generation, not a
format version. Segments with any signature other than those listed are not
read; REINDEX writes the current page and segment formats.

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
