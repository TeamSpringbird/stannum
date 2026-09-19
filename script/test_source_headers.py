# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import tempfile
from pathlib import Path
import unittest

from source_headers import check, notice, preamble, rewrite


class SourceHeadersTest(unittest.TestCase):
    def test_preserves_code_and_interpreter_declarations(self):
        examples = {
            "lib.rs": b"//! Crate docs\n#![allow(dead_code)]\nfn main() {}\n",
            "script.py": b"#!/usr/bin/env python3\n# coding: utf-8\n\"\"\"Module docs\"\"\"\n",
            "plain.py": b"# coding: latin-1\ntext = 'caf\xe9'\n",
            "script/run": b"#!/bin/sh\nset -eu\n",
            "grammar.pest": b"term = { ASCII_ALPHA+ }\n",
            "snapshot.sql": b"/* generated */\nSELECT 1;\n",
            "README.md": b"# Title\n",
        }
        for path, content in examples.items():
            with self.subTest(path=path):
                updated = rewrite(path, content, "mixed")
                prefix, body = preamble(path, updated)
                self.assertEqual(prefix + body.removeprefix(notice(path, "mixed")), content)
                self.assertEqual(rewrite(path, updated, "mixed"), updated)

    def test_reclassification_replaces_only_our_header(self):
        content = b"fn main() {}\n"
        inherited = rewrite("main.rs", content, "planetscale")
        self.assertEqual(rewrite("main.rs", inherited, "mixed"),
                         notice("main.rs", "mixed") + content)

    def test_does_not_overwrite_unrecognized_notice(self):
        with self.assertRaisesRegex(ValueError, "requires review"):
            rewrite("lib.rs", b"// Copyright 2025 Another contributor\nfn main() {}\n", "ben")

    def test_inventory_requires_review_for_new_files_and_detects_stale_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "new.rs").write_text("fn main() {}\n")
            errors = check(root, ["new.rs"], {"old.rs": {"owner": "ben", "evidence": "new"}})
            self.assertEqual(len(errors), 2)
            self.assertTrue(any("no reviewed provenance" in error for error in errors))
            self.assertTrue(any("no tracked file" in error for error in errors))

    def test_write_then_check(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "new.rs").write_text("fn main() {}\n")
            entries = {"new.rs": {"owner": "ben", "evidence": "original implementation"}}
            self.assertEqual(len(check(root, ["new.rs"], entries)), 1)
            self.assertEqual(check(root, ["new.rs"], entries, write=True), [])
            self.assertEqual(check(root, ["new.rs"], entries), [])


if __name__ == "__main__":
    unittest.main()
