#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""dev/spec.py's size check and --write-sizes on a throwaway tree (T-3664, T-3766)."""

import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import spec  # noqa: E402

DOC = """\
# Map

| crate | what |
|---|---|
| `antares-api` | the binding |

| module | what |
|---|---|
| `lib.rs` | the router |
"""


def tree(root):
    """A workspace of two crates: antares-api (lib.rs 1 200 lines, views.rs
    3 lines) and antares-store (150 lines)."""
    api = root / "crates/antares-api/src"
    store = root / "crates/antares-store/src"
    api.mkdir(parents=True)
    store.mkdir(parents=True)
    (api / "lib.rs").write_text("x\n" * 1200)
    (api / "views.rs").write_text("x\n" * 3)
    (store / "lib.rs").write_text("x\n" * 150)
    (root / "ARCHITECTURE.md").write_text(DOC + "| `views.rs` | the views |\n")


class Sizes(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        tree(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def sizes(self):
        return (self.root / spec.SIZES).read_text()

    def violations(self):
        return spec.architecture_size_violations(self.root)

    def test_written_sizes_pass_and_a_grown_module_turns_the_check_red(self):
        self.assertIn("ci/sizes.txt: missing; run dev/spec.py --write-sizes", self.violations())
        self.assertEqual(
            spec.write_sizes(self.root), ["antares-api", "antares-store", "lib.rs", "views.rs"]
        )
        self.assertEqual(self.violations(), [])
        self.assertIn("\nantares-api 1200\n", self.sizes())
        (self.root / "crates/antares-api/src/lib.rs").write_text("x\n" * 2000)
        self.assertIn("ci/sizes.txt: sizes lib.rs at 1200 lines, it holds 2000", self.violations())
        self.assertEqual(spec.write_sizes(self.root), ["antares-api", "lib.rs"])
        self.assertEqual(self.violations(), [])

    def test_a_number_in_tolerance_keeps_its_rounding(self):
        spec.write_sizes(self.root)
        text = self.sizes().replace("antares-api 1200", "antares-api 1250")
        (self.root / spec.SIZES).write_text(text)
        self.assertEqual(spec.write_sizes(self.root), [])
        self.assertIn("\nantares-api 1250\n", self.sizes())

    def test_a_missing_line_and_an_unknown_name_are_refused_then_repaired(self):
        spec.write_sizes(self.root)
        text = self.sizes().replace("antares-store 150\n", "not-a-crate 5\n")
        (self.root / spec.SIZES).write_text(text)
        self.assertIn("ci/sizes.txt: no line for antares-store", self.violations())
        self.assertIn("ci/sizes.txt: not-a-crate is no crate or antares-api module", self.violations())
        self.assertEqual(spec.write_sizes(self.root), ["antares-store", "not-a-crate"])
        self.assertEqual(self.violations(), [])

    def test_a_size_column_back_in_the_tables_is_refused(self):
        spec.write_sizes(self.root)
        (self.root / "ARCHITECTURE.md").write_text(DOC.replace("| `lib.rs` |", "| `lib.rs` | 1 200 |")
                                                   + "| `views.rs` | the views |\n")
        self.assertIn(
            "ARCHITECTURE.md: sizes lib.rs in a table; sizes live in ci/sizes.txt", self.violations()
        )

    def test_a_module_without_a_row_is_reported(self):
        (self.root / "ARCHITECTURE.md").write_text(DOC)
        spec.write_sizes(self.root)
        self.assertIn("ARCHITECTURE.md: the module table has no row for views.rs", self.violations())

    def test_running_it_twice_changes_nothing(self):
        spec.write_sizes(self.root)
        once = self.sizes()
        self.assertEqual(spec.write_sizes(self.root), [])
        self.assertEqual(self.sizes(), once)

    def test_every_rounding_step_stays_inside_the_tolerance(self):
        for n in [0, 1, 15, 99, 100, 104, 105, 995, 999, 1000, 1024, 9975, 9999, 10049, 55870]:
            self.assertFalse(spec.size_is_stale(spec.rounded(n), n), (n, spec.rounded(n)))


if __name__ == "__main__":
    unittest.main()
