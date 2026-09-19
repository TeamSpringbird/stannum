# Source attribution

Stannum is derived from [PlanetScale Lead](https://github.com/planetscale/lead).
Preserve PlanetScale's copyright on inherited code, including moved files and
code extracted into new modules. Use `Ben Weis <ben@springbird.app>` for
Stannum contributions. Preserve any other contributor's existing notices.

`source-provenance.json` records the reviewed attribution for each source file.
It includes upstream paths and evidence for moved/copied files, rather than
inferring ownership from the most recent editor or the current filename.
For example, `segment/src/tf_bucket.rs` originated as Lead's
`postgres/src/tf_bucket.rs`. The generated release SQL inherits attribution
from the Rust modules that produce it.

The initial audit compared content at the last shared Lead ancestor
`300ad3afbcaae42a90ef963cb3eb445784c733b3`, inspected rename/copy history and
substantial matching blocks in newly added files, and reviewed the moved
query-language documentation and generated SQL. The imported Boldi–Vigna
README comes from Lead commit `251df396f0a13636c67331d08550b419209bb66a`.
These are provenance judgments, not proof of authorship by an automated tool.

## Notices

Use PlanetScale's notice alone on unchanged inherited files:

```text
Copyright (C) 2026 PlanetScale
```

For inherited files with Stannum contributions:

```text
Copyright (C) 2026 Ben Weis <ben@springbird.app>
Based on Lead, copyright (C) 2026 PlanetScale
```

For independently authored Stannum source:

```text
Copyright (C) 2026 Ben Weis <ben@springbird.app>
```

Each notice ends with `See LICENSE in the repository root for license terms.`
Use the file format's comment syntax; interpreter and Python encoding
declarations remain first. The current templates use 2026, the year of these
contributions. Extend the templates deliberately when later years or other
copyright holders apply; do not erase existing notices to satisfy the check.

License terms are unchanged. Workspace metadata still says `AGPL-3.0-only`.
Lead's newer Rust boilerplate says “or any later version”; clarification is
pending, so this migration copies the copyright credit and references the
existing LICENSE rather than adopting that conflicting wording.

## Maintenance

1. Review each new file's provenance, including copied or extracted fragments.
   Add its entry to `source-provenance.json`. When substantively modifying an
   inherited file, change `planetscale` to `mixed` and record the contribution.
   Do not add a copyright holder solely for a mechanical edit.
2. Stage new source paths so `git ls-files` includes them, then run
   `python3 script/source_headers.py --write` to apply the reviewed notices.
   The command preserves existing code bytes, refuses unfamiliar notices, and
   will not remove an existing copyright holder when reclassifying a file.
3. Run `python3 script/source_headers.py` and
   `python3 -m unittest discover -s script -p 'test_*.py'`. CI runs both.

The check covers tracked Rust, Python, shell, SQL, Pest grammars, Cargo and
other TOML configuration, YAML workflows, extension control files, Dockerfiles,
and files with interpreter shebangs. The inventory also explicitly covers the
copied documentation. Original prose, lockfiles, ignore patterns, JSON
metadata, regression seeds, and binary fixtures are outside text-header
coverage. Add support when introducing another source format; a file being
outside the check does not remove its attribution obligations.

The check catches missing entries and incorrect headers. It cannot determine
whether a newly edited file contains an upstream contribution: that remains
part of code review. For generated release SQL, reapply notices after schema
generation as described in [the release checklist](RELEASING.md).

Keep mechanical header insertion in its own commit. Add its final SHA to
`.git-blame-ignore-revs` in a subsequent commit, and use
`git blame --ignore-revs-file .git-blame-ignore-revs` to follow code history.
If the migration is rebased or squash-merged, update that entry to the final
header-only commit; never ignore a squash commit containing behavior changes.
