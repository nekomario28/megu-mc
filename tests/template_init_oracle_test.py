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
        self.assertEqual(len(validate_rows(self.fixture_text())), 4)

    def test_a_missing_cell_is_rejected(self):
        rows = self.fixture_text().splitlines()
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows[:-1]) + "\n")

    def test_a_duplicate_cell_is_rejected(self):
        rows = self.fixture_text().splitlines()
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows + [rows[0]]) + "\n")

    def test_a_missing_entity_or_a_missing_control_is_rejected(self):
        rows = [line.split("\t") for line in self.fixture_text().splitlines()]
        for index, row in enumerate(rows):
            row = list(row)
            row[4] = "absent"
            rows[index] = row
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
        rows = rows_from_stdout("[12:00:00] [main/INFO]: [STDOUT]: INIT\tnone\ttrue\t1,2,3\t"
                                "minecraft:chest\td8d35ce80a1d3a91d843d7e6ad48b54eae9c14966df803f1cafbbe89a63a9a01\n"
                                "[12:00:00] [main/INFO]: [STDOUT]: INIT\tnone\ttrue\t2,2,3\t"
                                "minecraft:stone\tabsent\n"
                                "[12:00:00] [main/INFO]: [STDOUT]: INIT\tclockwise_90\ttrue\t1,2,3\t"
                                "minecraft:chest\td8d35ce80a1d3a91d843d7e6ad48b54eae9c14966df803f1cafbbe89a63a9a01\n"
                                "[12:00:00] [main/INFO]: [STDOUT]: INIT\tclockwise_90\ttrue\t2,2,3\t"
                                "minecraft:stone\tabsent\n")
        self.assertEqual(len(rows), 4)


if __name__ == "__main__":
    unittest.main()
