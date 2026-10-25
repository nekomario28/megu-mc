#!/usr/bin/env python3
"""Offline corpus identity, coverage and fail-closed weird-sampler guards."""
import copy
import hashlib
import json
import math
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import weird_sampler_oracle as oracle  # noqa: E402


def f64(bits):
    return struct.unpack(">d", bytes.fromhex(bits))[0]


# Actual 1.21.1 enum functions return these scales, not ordinal bucket numbers.
RARITIES = {"type_1": (0.75, 1.0, 1.5, 2.0),
            "type_2": (0.5, 0.75, 1.0, 2.0, 3.0)}


class WeirdSamplerReferenceTests(unittest.TestCase):
    def setUp(self):
        self.data = json.loads(oracle.FIXTURE.read_text())
        self.rows = self.data["cases"]
        self.requested = oracle.cases()

    def test_complete_cases_and_provenance(self):
        self.assertEqual(set(self.data), {"minecraft", "server_sha1", "oracle_sha256",
                                          "noise_sha256", "cases"})
        self.assertEqual(self.data["minecraft"], "1.21.1")
        self.assertEqual(self.data["server_sha1"], oracle.JAR_SHA1)
        self.assertEqual(self.data["oracle_sha256"],
                         hashlib.sha256(oracle.SOURCE.read_bytes()).hexdigest())
        self.assertEqual(set(self.data["noise_sha256"]), set(oracle.NOISE_NAMES))
        for name, digest in self.data["noise_sha256"].items():
            local = oracle.NOISE_DIR / (name.split(":", 1)[1] + ".json")
            self.assertEqual(digest, hashlib.sha256(local.read_bytes()).hexdigest(), name)
        oracle.validate(self.rows, self.requested)
        self.assertEqual([{key: value for key, value in row.items() if key != "expected"}
                          for row in self.rows], self.requested)
        self.assertEqual(self.requested, oracle.cases())
        self.assertEqual(len({row["id"] for row in self.rows}), len(self.rows))
        self.assertTrue(all(row["id"] == f"weird-{i:03d}"
                            for i, row in enumerate(self.rows)))
        json.dumps(self.requested, allow_nan=False)

        mapper_rows = [row for row in self.rows if row["op"] == "mapper"]
        self.assertTrue(mapper_rows)
        for mapper, thresholds in oracle.THRESHOLDS.items():
            bits = {row["input_bits"] for row in mapper_rows if row["mapper"] == mapper}
            for threshold in thresholds:
                for value in (math.nextafter(threshold, -math.inf), threshold,
                              math.nextafter(threshold, math.inf)):
                    self.assertIn(oracle.f64_bits(value), bits, (mapper, threshold, value))
            for value in (0.0, -0.0, math.inf, -math.inf):
                self.assertIn(oracle.f64_bits(value), bits, (mapper, value))
            self.assertIn(oracle.NAN_BITS, bits)
            values = {row["input_bits"]: f64(row["expected"]["rarity_bits"])
                      for row in mapper_rows if row["mapper"] == mapper}
            scales = RARITIES[mapper]
            for index, threshold in enumerate(thresholds):
                self.assertEqual(values[oracle.f64_bits(math.nextafter(threshold, -math.inf))],
                                 scales[index])
                self.assertEqual(values[oracle.f64_bits(threshold)], scales[index + 1])
                self.assertEqual(values[oracle.f64_bits(math.nextafter(threshold, math.inf))],
                                 scales[index + 1])
            self.assertEqual(values[oracle.f64_bits(-0.0)], values[oracle.f64_bits(0.0)])
            self.assertEqual(values[oracle.f64_bits(-math.inf)], scales[0])
            self.assertEqual(values[oracle.f64_bits(math.inf)], scales[-1])
            self.assertEqual(values[oracle.NAN_BITS], scales[-1])

        samples = [row for row in self.rows if row["op"] == "sample"]
        self.assertTrue(samples)
        self.assertEqual({row["mapper"] for row in samples}, set(oracle.THRESHOLDS))
        self.assertEqual({row["seed"] for row in samples}, {0, 42})
        self.assertEqual({row["noise"] for row in samples}, set(oracle.NOISE_NAMES))
        for mapper in oracle.THRESHOLDS:
            expected_buckets = set(RARITIES[mapper])
            for noise in oracle.NOISE_NAMES:
                for seed in (0, 42):
                    selected = [row for row in samples if row["mapper"] == mapper
                                and row["noise"] == noise and row["seed"] == seed]
                    self.assertTrue(selected, (mapper, noise, seed))
                    buckets = {f64(row["expected"]["rarity_bits"]) for row in selected}
                    self.assertEqual(buckets, expected_buckets, (mapper, noise, seed))
        for row in samples:
            self.assertTrue(math.isfinite(f64(row["input_bits"])))
            bucket = sum(f64(row["input_bits"]) >= threshold
                         for threshold in oracle.THRESHOLDS[row["mapper"]])
            self.assertEqual(f64(row["expected"]["rarity_bits"]),
                             RARITIES[row["mapper"]][bucket], row["id"])
            self.assertEqual(set(row["expected"]),
                             {"rarity_bits", "noise_bits", "value_bits", "scaled_bits"})
            self.assertEqual(len(row["expected"]["scaled_bits"]), 3)
            for bits in row["expected"]["scaled_bits"]:
                self.assertTrue(math.isfinite(f64(bits)))
        noise_values = [f64(row["expected"]["noise_bits"]) for row in samples]
        value_values = [f64(row["expected"]["value_bits"]) for row in samples]
        self.assertTrue(any(value > 0 for value in noise_values))
        self.assertTrue(any(value < 0 for value in noise_values))
        self.assertTrue(any(value > 0 for value in value_values))
        self.assertTrue(all(value >= 0 for value in value_values))
        positions = {tuple(row["pos"]) for row in samples}
        self.assertTrue(any(any(value < 0 for value in pos) for pos in positions))
        self.assertTrue(any(pos == (0, 0, 0) for pos in positions))
        self.assertTrue(any(abs(value) == 30000000 for pos in positions for value in pos))
        self.assertTrue(any(abs(pos[1]) > 64 for pos in positions))

    def test_comparison_rejects_empty_truncated_extra_changed_and_stale(self):
        oracle.require_equal(self.data, copy.deepcopy(self.data))
        mutations = [
            lambda value: value.update(cases=[]),
            lambda value: value["cases"].pop(),
            lambda value: value["cases"].append(copy.deepcopy(value["cases"][0])),
            lambda value: value["cases"].reverse(),
            lambda value: value["cases"][0].update(input_bits="0" * 16),
            lambda value: value["cases"][0]["expected"].pop("rarity_bits"),
            lambda value: value["cases"][0]["expected"].update(extra="0" * 16),
            lambda value: value["cases"][0]["expected"].update(rarity_bits="0" * 16),
            lambda value: value.update(oracle_sha256="0" * 64),
            lambda value: value.update(server_sha1="0" * 40),
            lambda value: value.update(minecraft="1.20.1"),
            lambda value: value["noise_sha256"].update(
                {"minecraft:spaghetti_2d": "0" * 64}),
            lambda value: value["noise_sha256"].pop("minecraft:spaghetti_2d"),
            lambda value: value["noise_sha256"].update(extra="0" * 64),
        ]
        for mutate in mutations:
            changed = copy.deepcopy(self.data)
            mutate(changed)
            with self.assertRaises(ValueError):
                oracle.require_equal(self.data, changed)

    def test_comparison_rejects_bool_integer_equality_traps(self):
        changed = copy.deepcopy(self.data)
        sample = next(row for row in changed["cases"] if row["op"] == "sample")
        self.assertEqual(sample["seed"], 0)
        sample["seed"] = False
        with self.assertRaises(ValueError):
            oracle.require_equal(self.data, changed)

    def test_validation_rejects_missing_results_changed_requests_and_types(self):
        rows = copy.deepcopy(self.rows)
        requested = oracle.cases()
        mapper_index = next(i for i, row in enumerate(rows) if row["op"] == "mapper")
        sample_index = next(i for i, row in enumerate(rows) if row["op"] == "sample")

        def rejected(changed):
            with self.assertRaises(ValueError):
                oracle.validate(changed, requested)

        rejected([])
        rejected(rows[:-1])
        rejected(rows + [copy.deepcopy(rows[0])])
        changed = copy.deepcopy(rows)
        changed[0], changed[1] = changed[1], changed[0]
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[0]["extra"] = 0
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[mapper_index]["input_bits"] = "0" * 16
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[mapper_index]["input_bits"] = "0" * 15
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[mapper_index]["input_bits"] = "A" * 16
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[mapper_index]["mapper"] = ("type_2" if changed[mapper_index]["mapper"] == "type_1"
                                           else "type_1")
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["seed"] = False
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["pos"] = [False, 0, 0]
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["noise"] = "minecraft:spaghetti_2d_extra"
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["op"] = "temperature"
        rejected(changed)
        for field in ("rarity_bits", "noise_bits", "value_bits"):
            changed = copy.deepcopy(rows)
            changed[sample_index]["expected"].pop(field)
            rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["expected"]["extra"] = "0" * 16
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["expected"]["rarity_bits"] = "0" * 15
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["expected"]["value_bits"] = False
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["expected"]["scaled_bits"] = (
            changed[sample_index]["expected"]["scaled_bits"][:2])
        rejected(changed)
        changed = copy.deepcopy(rows)
        changed[sample_index]["expected"]["scaled_bits"][0] = 0
        rejected(changed)

    def test_wrong_jar_preserves_output_before_java(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar, output = Path(tmp) / "wrong.jar", Path(tmp) / "output.json"
            jar.write_bytes(b"not the pinned jar")
            output.write_text("keep this reference\n")
            result = subprocess.run([sys.executable, str(ROOT / "tools/weird_sampler_oracle.py"),
                                     "--jar", str(jar), "--output", str(output)],
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the pinned 1.21.1 server jar", result.stderr)
            self.assertEqual(output.read_text(), "keep this reference\n")


if __name__ == "__main__":
    unittest.main()
