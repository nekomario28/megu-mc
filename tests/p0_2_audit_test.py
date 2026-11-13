#!/usr/bin/env python3
"""Fail-closed contract tests for tools/p0_2_audit.py."""
import copy
import importlib.util
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("p0_2_audit", ROOT / "tools/p0_2_audit.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class P02AuditTests(unittest.TestCase):
    def test_pinned_datapack_and_rust_parsers_pass_audit(self):
        report = audit.run_audit()
        self.assertEqual(report["failures"], [])
        ow = report["density_and_surface"]["overworld"]
        self.assertEqual(
            ow["df_zero_folded_reachable"],
            {"blend_alpha": "DF::C(1.0)", "blend_offset": "DF::Zero"},
        )
        self.assertEqual(ow["surface_biome_is_missing_from_bleaf"], [])
        self.assertLess(ow["bleaf_count"], 64)
        self.assertEqual(
            report["features_and_placements"]["end"]["configured_feature_unimplemented_reachable"],
            ["end_spike"],
        )
        self.assertEqual(report["biomes_and_structures"]["structures_total"], 34)

    def test_unsupported_density_surface_or_bleaf_overflow_fails_closed(self):
        parsers = audit.parse_rust_parsers()
        noise_settings = audit.load_json_dir("noise_settings")
        df_files = audit.load_json_dir("density_function")
        noise_files = audit.load_json_dir("noise")
        dim_biomes = audit.tree_biomes()

        ns_bad = copy.deepcopy(noise_settings)
        ns_bad["minecraft:overworld"]["noise_router"]["final_density"] = {"type": "minecraft:unparsed_df_node"}
        ns_bad["minecraft:overworld"]["surface_rule"] = {
            "type": "minecraft:condition",
            "if_true": {"type": "minecraft:biome", "biome_is": ["minecraft:nonexistent_biome"]},
            "then_run": {"type": "minecraft:unparsed_rule"},
        }
        dims_bad = {k: set(v) for k, v in dim_biomes.items()}
        dims_bad["nether"] = {f"minecraft:fake_biome_{i}" for i in range(64)}

        _, failures = audit.audit_density_and_surface(ns_bad, df_files, noise_files, dims_bad, parsers)
        joined = "\n".join(failures)
        self.assertIn("unparsed_df_node", joined)
        self.assertIn("unparsed_rule", joined)
        self.assertIn("minecraft:nonexistent_biome", joined)
        self.assertIn("overflows Cond::Biome(u64)", joined)

    def test_unsupported_placement_modifier_fails_closed(self):
        parsers = audit.parse_rust_parsers()
        biomes = audit.load_json_dir("biome")
        pfs = audit.load_json_dir("placed_feature")
        cfs = audit.load_json_dir("configured_feature")
        dim_biomes = audit.tree_biomes()

        pfs_bad = copy.deepcopy(pfs)
        first_pf = next(iter(pfs_bad))
        # Ensure it's reachable from an overworld biome
        first_ow_biome = sorted(dim_biomes["overworld"])[0]
        biomes_bad = copy.deepcopy(biomes)
        biomes_bad[first_ow_biome]["features"] = [[first_pf]]
        pfs_bad[first_pf] = {
            "feature": next(iter(cfs)),
            "placement": [{"type": "minecraft:unknown_pm_modifier"}],
        }
        _, failures = audit.audit_features_and_placements(biomes_bad, pfs_bad, cfs, dim_biomes, parsers)
        self.assertTrue(any("unknown_pm_modifier" in f for f in failures), failures)


if __name__ == "__main__":
    unittest.main()
