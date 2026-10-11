#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""dev/spec.py --write-sizes on a throwaway tree (T-3664)."""

import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import spec  # noqa: E402

DOC = """\
# Map

Prose with `antares-api` | 7 | inside a sentence stays.

| crate | lines | what |
|---|---|---|
| `antares-api` | 9 | the binding |
| `antares-store` | 1 000 | stale |
| `not-a-crate` | 5 | no such crate |

| module | lines | what |
|---|---|---|
| `lib.rs` | 1 210 | the router, within tolerance |
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
    (root / "ARCHITECTURE.md").write_text(DOC)


class WriteSizes(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        tree(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def text(self):
        return (self.root / "ARCHITECTURE.md").read_text()

    def test_a_stale_row_is_rewritten_and_then_passes(self):
        self.assertIn(
            "ARCHITECTURE.md: sizes antares-store at 1000 lines, it holds 150",
            spec.architecture_size_violations(self.root),
        )
        self.assertEqual(spec.write_sizes(self.root), ["antares-api", "antares-store"])
        self.assertIn("| `antares-api` | 1 200 |", self.text())
        self.assertIn("| `antares-store` | 150 |", self.text())
        left = spec.architecture_size_violations(self.root)
        self.assertFalse([e for e in left if "sizes " in e], left)

    def test_prose_unknown_names_and_rows_in_tolerance_stay_byte_for_byte(self):
        spec.write_sizes(self.root)
        before, after = DOC.splitlines(), self.text().splitlines()
        self.assertEqual(len(before), len(after))
        changed = [b for b, a in zip(before, after) if b != a]
        self.assertEqual(
            changed,
            ["| `antares-api` | 9 | the binding |", "| `antares-store` | 1 000 | stale |"],
        )

    def test_a_missing_module_row_is_reported_not_added(self):
        spec.write_sizes(self.root)
        self.assertNotIn("views.rs", self.text())
        self.assertIn(
            "ARCHITECTURE.md: the module table has no row for views.rs",
            spec.architecture_size_violations(self.root),
        )

    def test_running_it_twice_changes_nothing(self):
        spec.write_sizes(self.root)
        once = self.text()
        self.assertEqual(spec.write_sizes(self.root), [])
        self.assertEqual(self.text(), once)

    def test_every_rounding_step_stays_inside_the_tolerance(self):
        for n in [0, 1, 15, 99, 100, 104, 105, 995, 999, 1000, 1024, 9975, 9999, 10049, 55870]:
            stated = int(spec.rounded(n).replace(" ", ""))
            self.assertFalse(spec.size_is_stale(stated, n), (n, stated))


if __name__ == "__main__":
    unittest.main()
