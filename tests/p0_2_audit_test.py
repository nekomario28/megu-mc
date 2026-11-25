#!/usr/bin/env python3
"""Fail-closed contract tests for tools/p0_2_audit.py."""
import copy
import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("p0_2_audit", ROOT / "tools/p0_2_audit.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class P02AuditTests(unittest.TestCase):
    def test_pinned_datapack_and_rust_parsers_are_fully_audited(self):
        report = audit.run_audit()
        # A future runtime fix can close the gaps without changing this test.
        # Unexpected new failures still fail the CLI and remain in the report.
        for dim in ("overworld", "nether", "end"):
            ds = report["density_and_surface"][dim]
            fp = report["features_and_placements"][dim]
            for field in ("df_missing_refs", "noise_missing_refs", "df_unsupported",
                          "surface_rule_unsupported", "surface_cond_unsupported",
                          "surface_biome_is_missing_from_bleaf"):
                self.assertFalse(ds[field], (dim, field, ds[field]))
            for field in ("placement_modifier_unsupported", "missing_pf", "missing_cf"):
                self.assertFalse(fp[field], (dim, field, fp[field]))
        self.assertEqual(report["cond_biome_anchor"], "u128")
        ow = report["density_and_surface"]["overworld"]
        self.assertEqual(
            ow["df_zero_folded_reachable"],
            {"blend_alpha": "DF::C(1.0)", "blend_offset": "DF::Zero"},
        )
        self.assertEqual(ow["surface_biome_is_missing_from_bleaf"], [])
        self.assertLess(ow["bleaf_count"], 128)
        self.assertEqual(
            report["features_and_placements"]["end"]["configured_feature_unimplemented_reachable"],
            [],
        )
        self.assertEqual(report["biomes_and_structures"]["structures_total"], 34)
        spawn = report["spawn_reachability"]
        self.assertEqual(spawn["registry_entities"], 130)
        self.assertEqual(spawn["spawner_entries"], 746)
        self.assertEqual(spawn["valid_spawner_entries"], 746)
        self.assertEqual(spawn["reachable_spawner_uses"], 746)
        self.assertEqual(spawn["unreachable_spawner_uses"], 0)
        self.assertEqual(spawn["spawn_cost_entries"], 5)
        self.assertEqual(spawn["reachable_spawn_cost_uses"], 5)
        self.assertEqual(spawn["invalid_spawn_costs"], [])
        self.assertEqual(spawn["reachable_spawner_uses"] + spawn["unreachable_spawner_uses"],
                         spawn["spawner_entries"])
        self.assertEqual(sum(spawn["categories"].values()), spawn["spawner_entries"])

    @staticmethod
    def spawn_sources():
        return {
            name: (ROOT / f"src/{name}.rs").read_text()
            for name in ("entity_registry", "feature", "spawn_costs", "natural_spawner",
                         "nether_spawn_rules", "spawner", "main")
        }

    def test_real_nether_spawn_entries_and_costs_have_live_dispatch(self):
        all_biomes = audit.load_json_dir("biome")
        nether = audit.tree_biomes()["nether"]
        biomes = {name: all_biomes[name] for name in nether}
        runtime = audit.parse_spawn_runtime()
        result, failures = audit.audit_spawn_reachability(biomes, {"nether": nether}, runtime)
        self.assertEqual(failures, [])
        self.assertEqual(result["spawn_cost_entries"], 5)
        self.assertEqual(result["reachable_spawn_cost_uses"], 5)
        self.assertEqual(result["unreachable_spawner_uses"], 0)
        self.assertGreater(result["reachable_spawner_uses"], 0)

    def test_unknown_spawn_cost_entity_fails_closed(self):
        all_biomes = audit.load_json_dir("biome")
        nether = audit.tree_biomes()["nether"]
        biomes = {name: copy.deepcopy(all_biomes[name]) for name in nether}
        biomes["minecraft:warped_forest"]["spawn_costs"]["minecraft:phantom_unregistered"] = {
            "energy_budget": 0.5, "charge": 1.0,
        }
        result, failures = audit.audit_spawn_reachability(
            biomes, {"nether": nether}, audit.parse_spawn_runtime()
        )
        self.assertIn("unmapped spawn_costs entity 'minecraft:phantom_unregistered'", "\n".join(failures))
        self.assertEqual(result["spawn_cost_entries"], 6)
        self.assertEqual(result["reachable_spawn_cost_uses"], 5)

    def test_unreferenced_biome_entries_fail_closed(self):
        all_biomes = audit.load_json_dir("biome")
        name = "minecraft:the_void"
        biomes = {name: copy.deepcopy(all_biomes[name])}
        biomes[name]["spawners"]["monster"] = [{
            "type": "minecraft:zombie", "weight": 1, "minCount": 1, "maxCount": 1,
        }]
        biomes[name]["spawn_costs"]["minecraft:zombie"] = {
            "energy_budget": 1.0, "charge": 1.0,
        }
        result, failures = audit.audit_spawn_reachability(
            biomes, audit.tree_biomes(), audit.parse_spawn_runtime()
        )
        self.assertEqual(result["spawner_entries"], 1)
        self.assertEqual(result["unreachable_spawner_uses"], 1)
        self.assertEqual(result["spawn_cost_entries"], 1)
        self.assertIn("unreferenced: unreachable weighted monster minecraft:zombie", "\n".join(failures))
        self.assertIn("spawn_costs biome absent from dimension trees", "\n".join(failures))

    def test_registered_but_unreachable_nether_mob_fails_closed(self):
        all_biomes = audit.load_json_dir("biome")
        nether = audit.tree_biomes()["nether"]
        biomes = {name: copy.deepcopy(all_biomes[name]) for name in nether}
        biomes["minecraft:warped_forest"]["spawners"]["monster"].append({
            "type": "minecraft:bogged", "weight": 1, "minCount": 1, "maxCount": 1,
        })
        result, failures = audit.audit_spawn_reachability(
            biomes, {"nether": nether}, audit.parse_spawn_runtime()
        )
        self.assertEqual(result["unreachable_spawner_uses"], 1)
        self.assertIn("nether: unreachable weighted monster minecraft:bogged", "\n".join(failures))

    def test_malformed_cost_and_spawner_entries_fail_closed(self):
        all_biomes = audit.load_json_dir("biome")
        nether = audit.tree_biomes()["nether"]
        biomes = {name: copy.deepcopy(all_biomes[name]) for name in nether}
        b = biomes["minecraft:warped_forest"]
        b["spawn_costs"]["minecraft:enderman"] = {"energy_budget": float("nan"), "charge": 0.7}
        b["spawners"]["monster"].append({
            "type": "minecraft:missing_mob", "weight": 2, "minCount": 1, "maxCount": 4,
        })
        b["spawners"]["monster"].append({
            "type": "minecraft:enderman", "weight": 1, "minCount": 5, "maxCount": 2,
        })
        b["spawners"]["unsupported_category"] = [{
            "type": "minecraft:enderman", "weight": 1, "minCount": 1, "maxCount": 1,
        }]
        _, failures = audit.audit_spawn_reachability(biomes, {"nether": nether}, audit.parse_spawn_runtime())
        rendered = "\n".join(failures)
        for expected in ("invalid energy_budget/charge", "unmapped spawner entity",
                         "invalid weight or group bounds", "unsupported_category"):
            self.assertIn(expected, rendered)

    def test_live_tick_call_site_and_cost_consumer_fail_closed(self):
        sources = self.spawn_sources()
        self.assertIn("spawn_nether_natural", audit.parse_spawn_runtime(sources=sources)["tick_calls"])
        changed_main = dict(sources)
        changed_main["main"] = sources["main"].replace(
            "spawn_nether_natural(t);", "spawn_nether_natural_disabled(t);"
        )
        with self.assertRaisesRegex(audit.AuditError, "main tick -> spawn_nether_natural"):
            audit.parse_spawn_runtime(sources=changed_main)
        changed_natural = dict(sources)
        changed_natural["natural_spawner"] = sources["natural_spawner"].replace(
            "costs.can_spawn(&entry.kind, pos, chunk)", "true"
        )
        with self.assertRaisesRegex(audit.AuditError, "Nether spawn cost"):
            audit.parse_spawn_runtime(sources=changed_natural)

    def test_new_live_spawn_dispatch_requires_gated_tick_and_weighted_rules(self):
        sources = self.spawn_sources()
        baseline = audit.parse_spawn_runtime(sources=sources)
        self.assertEqual(baseline["end_categories"], {"Monster"})
        self.assertTrue({"WaterCreature", "WaterAmbient", "Ambient", "Axolotls",
                         "UndergroundWaterCreature"}.issubset(baseline["overworld_categories"]))
        for name in ("spawn_aquatic_natural", "spawn_end_natural",
                     "spawn_underground_natural"):
            changed = dict(sources)
            old = f"{name}(t);"
            assert old in sources["main"]
            changed["main"] = sources["main"].replace(old, f"disabled_{name}(t);", 1)
            with self.subTest(name=name), self.assertRaises(audit.AuditError):
                audit.parse_spawn_runtime(sources=changed)

        for snippet in ("choose_nightly_spawner(entries)",
                        "rules.allows(mob, pos, queries, draws)"):
            changed = dict(sources)
            assert snippet in sources["main"]
            changed["main"] = sources["main"].replace(snippet, "disabled_dispatch()", 1)
            with self.subTest(snippet=snippet), self.assertRaises(audit.AuditError):
                audit.parse_spawn_runtime(sources=changed)

        changed = dict(sources)
        loop_member = "        MobCategory::UndergroundWaterCreature,\n"
        assert loop_member in sources["main"]
        changed["main"] = sources["main"].replace(loop_member, "", 1)
        runtime = audit.parse_spawn_runtime(sources=changed)
        self.assertNotIn("UndergroundWaterCreature", runtime["overworld_categories"])
        all_biomes = audit.load_json_dir("biome")
        _, failures = audit.audit_spawn_reachability(all_biomes, audit.tree_biomes(), runtime)
        self.assertIn("unreachable weighted underground_water_creature", "\n".join(failures))

    def test_jungle_ocelot_quirk_only_allows_the_two_pinned_monster_rows(self):
        biomes = audit.load_json_dir("biome")
        names = ("minecraft:jungle", "minecraft:bamboo_jungle")
        subset = {name: copy.deepcopy(biomes[name]) for name in names}
        runtime = audit.parse_spawn_runtime()
        for name in names:
            self.assertEqual(subset[name]["spawners"]["monster"][8]["type"], "minecraft:ocelot")
        _, failures = audit.audit_spawn_reachability(subset, {"overworld": names}, runtime)
        self.assertEqual(failures, [])

        duplicate = copy.deepcopy(subset)
        duplicate["minecraft:jungle"]["spawners"]["monster"].append(
            copy.deepcopy(subset["minecraft:jungle"]["spawners"]["monster"][8]))
        _, failures = audit.audit_spawn_reachability(duplicate, {"overworld": names}, runtime)
        self.assertIn("registry category Creature differs from biome Monster", "\n".join(failures))

        swapped = copy.deepcopy(subset)
        swapped["minecraft:jungle"]["spawners"]["monster"][8]["type"] = "minecraft:cow"
        _, failures = audit.audit_spawn_reachability(swapped, {"overworld": names}, runtime)
        self.assertIn("minecraft:cow registry category Creature differs from biome Monster", "\n".join(failures))

    def test_cond_biome_anchor_fails_closed_on_revert_or_mismatch(self):
        wg_src = (ROOT / "src/worldgen.rs").read_text()
        self.assertEqual(audit.parse_cond_biome_anchor(wg_src), "u128")
        with self.assertRaises(audit.AuditError):
            audit.parse_cond_biome_anchor(wg_src.replace("Biome(u128)", "Biome(u64)"))
        with self.assertRaises(audit.AuditError):
            audit.parse_cond_biome_anchor(wg_src.replace("enum Cond", "enum CondRenamed"))
        with self.assertRaises(audit.AuditError):
            audit.parse_cond_biome_anchor("no Cond here")

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
        dims_bad["nether"] = {f"minecraft:fake_biome_{i}" for i in range(128)}

        _, failures = audit.audit_density_and_surface(ns_bad, df_files, noise_files, dims_bad, parsers)
        joined = "\n".join(failures)
        self.assertIn("unparsed_df_node", joined)
        self.assertIn("unparsed_rule", joined)
        self.assertIn("minecraft:nonexistent_biome", joined)
        self.assertIn("overflows Cond::Biome(u128)", joined)

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
