#!/usr/bin/env python3
"""Portable guard for the native template-initialization output contract (no JAR needed)."""
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from template_init_oracle import FIXTURE, rows_from_stdout, validate_rows


class TemplateInitOutputTest(unittest.TestCase):
    def fixture_text(self):
        return FIXTURE.read_text()

    def test_the_checked_fixture_validates(self):
        self.assertEqual(len(validate_rows(self.fixture_text())), 8)

    def test_a_missing_cell_is_rejected(self):
        rows = self.fixture_text().splitlines()
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows[:-1]) + "\n")

    def test_a_duplicate_cell_is_rejected(self):
        rows = self.fixture_text().splitlines()
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows + [rows[0]]) + "\n")

    def test_a_fixture_without_entities_is_rejected(self):
        rows = [line.split("\t") for line in self.fixture_text().splitlines()]
        rows = [[*row[:4], "absent", "-"] for row in rows]
        with self.assertRaises(ValueError):
            validate_rows("\n".join("\t".join(r) for r in rows) + "\n")

    def test_a_fixture_without_decoded_contents_is_rejected(self):
        rows = [line.split("\t") for line in self.fixture_text().splitlines()]
        rows = [[*row[:5], "-"] for row in rows]
        with self.assertRaises(ValueError):
            validate_rows("\n".join("\t".join(r) for r in rows) + "\n")

    def test_contents_without_an_entity_is_rejected(self):
        rows = [line.split("\t") for line in self.fixture_text().splitlines()]
        entity_row = next(r for r in rows if r[5] != "-")
        entity_row[4] = "absent"
        with self.assertRaises(ValueError):
            validate_rows("\n".join("\t".join(r) for r in rows) + "\n")

    def test_a_malformed_hash_is_rejected(self):
        rows = self.fixture_text().splitlines()
        rows[0] = "\t".join(rows[0].split("\t")[:4] + ["not-a-hash"])
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows) + "\n")

    def test_stdout_with_no_probe_lines_is_empty(self):
        with self.assertRaises(ValueError):
            rows_from_stdout("nothing here\n")

    def test_probe_lines_are_extracted_around_the_log_prefix(self):
        hash = "d8d35ce80a1d3a91d843d7e6ad48b54eae9c14966df803f1cafbbe89a63a9a01"
        lines = []
        cells = [("1,2,3", "minecraft:chest", hash, "-", "-"),
                 ("1,2,4", "minecraft:chest", hash, "0=3 minecraft:redstone", "-"),
                 ("2,2,3", "minecraft:stone", "absent", "-", "-"),
                 ("2,3,3", "minecraft:oak_sign[rotation=0]", hash, "-", "'\"Hi\"'|'\"\"'|'\"\"'|'\"\"'")]
        for case in ("none", "clockwise_90"):
            for cell, state, nbt, contents, text in cells:
                lines.append("[12:00:00] [main/INFO]: [STDOUT]: INIT\t" + case + "\ttrue\t"
                             + cell + "\t" + state + "\t" + nbt + "\t" + contents + "\t" + text)
        rows = rows_from_stdout("\n".join(lines) + "\n")
        self.assertEqual(len(rows), 8)


if __name__ == "__main__":
    unittest.main()
