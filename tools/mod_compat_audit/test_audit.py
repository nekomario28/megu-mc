#!/usr/bin/env python3
"""Check the measured boundary through the audit CLI and real ASM scanner."""

import json
from pathlib import Path
import subprocess
import sys
import unittest

HERE = Path(__file__).resolve().parent


class AuditTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        result = subprocess.run(
            [sys.executable, str(HERE / "audit.py"), "--format", "json"],
            capture_output=True, text=True,
        )
        if result.returncode:
            raise AssertionError(f"audit failed ({result.returncode}): {result.stderr}")
        cls.audit = json.loads(result.stdout)
        cls.fixtures = {fixture["fixture"]: fixture for fixture in cls.audit["fixtures"]}
        surface = HERE.parents[1] / "compat/neoforge-events/shim-surface.json"
        cls.surface = json.loads(surface.read_text())["classes"]

    def assert_method_classified(self, fixture, owner, name, descriptor):
        row = self.fixtures[fixture]
        member = [owner, name, descriptor]
        supported = name + descriptor in self.surface.get(owner, {}).get("methods", [])
        matching = "supported_members" if supported else "unsupported_members"
        other = "unsupported_members" if supported else "supported_members"
        self.assertIn(member, row[matching])
        self.assertNotIn(member, row[other])

    def test_content_passes_with_exact_method_and_field_descriptors(self):
        content = self.fixtures["content"]
        self.assertEqual(content["status"], "COMPATIBLE")
        self.assertEqual(content["unsupported_members"], [])
        self.assertIn(
            ["net/minecraft/world/level/block/state/BlockBehaviour$Properties",
             "of", "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;"],
            content["supported_members"],
        )
        self.assertIn(
            ["net/minecraft/core/registries/Registries", "BLOCK",
             "Lnet/minecraft/resources/ResourceKey;"],
            content["supported_members"],
        )

    def test_gameplay_does_not_conflate_event_coverage_with_jar_admission(self):
        gameplay = self.fixtures["gameplay"]
        self.assertEqual(gameplay["status"], "REJECTED")
        self.assertIn(
            ["net/neoforged/neoforge/event/level/BlockEvent$BreakEvent", "setCanceled", "(Z)V"],
            gameplay["supported_members"],
        )
        self.assertIn(
            ["net/neoforged/neoforge/event/entity/living/LivingDamageEvent$Pre", "setNewDamage", "(F)V"],
            gameplay["unsupported_members"],
        )

    def test_automation_and_network_preserve_erased_jvm_descriptors(self):
        self.assert_method_classified(
            "automation", "net/minecraft/world/level/LevelAccessor", "scheduleTick",
            "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/Block;I)V",
        )
        self.assert_method_classified(
            "automation", "net/neoforged/neoforge/items/IItemHandler", "insertItem",
            "(ILnet/minecraft/world/item/ItemStack;Z)Lnet/minecraft/world/item/ItemStack;",
        )
        self.assert_method_classified(
            "network", "net/minecraft/resources/ResourceKey", "location",
            "()Lnet/minecraft/resources/ResourceLocation;",
        )
        self.assert_method_classified(
            "network", "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent", "registrar",
            "(Ljava/lang/String;)Lnet/neoforged/neoforge/network/registration/PayloadRegistrar;",
        )

    def test_declared_transform_diagnostics_distinguish_config_injections_and_at(self):
        transforms = self.fixtures["transforms"]
        reasons = {(reason["code"], reason["symbol"]) for reason in transforms["diagnostics"]}
        for injection in ("ModifyVariable", "Overwrite", "Redirect"):
            self.assertIn(
                ("UNSUPPORTED_MIXIN_INJECTION", f"auditfixture/transforms/WorldMixin @{injection}"), reasons,
            )
        for point in ("HEAD", "TAIL"):
            self.assertNotIn(("UNSUPPORTED_MIXIN_AT", f"auditfixture/transforms/WorldMixin @At({point})"), reasons)
        self.assertIn(("UNSUPPORTED_MIXIN_CONFIG", "hooks.mixins.json"), reasons)
        self.assertIn(("UNSUPPORTED_ACCESS_TRANSFORMER", "META-INF/accesstransformer.cfg"), reasons)

    def test_fabric_reports_current_metadata_and_side_coverage_boundary(self):
        fabric = self.fixtures["fabric"]
        self.assertIn("fabric.mod.json", fabric["resources"])
        self.assertIn("fabric.mixins.json", fabric["resources"])
        self.assertIn("MISSING_METADATA", {reason["code"] for reason in fabric["diagnostics"]})
        self.assertIn("net/minecraft/class_310", fabric["unsupported_classes"])
        self.assertIn(
            ["net/minecraft/class_1937", "method_27983", "()Lnet/minecraft/class_5321;"],
            fabric["unsupported_members"],
        )

    def test_reports_both_explicit_and_conventional_at_resources(self):
        for name in ("implicit_at", "transforms"):
            self.assertIn("META-INF/accesstransformer.cfg", self.fixtures[name]["resources"])

    def test_member_totals_deduplicate_across_jars_without_losing_each_report(self):
        location = ["net/minecraft/resources/ResourceLocation", "fromNamespaceAndPath",
                    "(Ljava/lang/String;Ljava/lang/String;)Lnet/minecraft/resources/ResourceLocation;"]
        for name in ("content", "network"):
            self.assertIn(location, self.fixtures[name]["supported_members"])
        rows = [tuple(member) for fixture in self.fixtures.values()
                for member in fixture["supported_members"]]
        self.assertEqual(self.audit["counts"]["supported_members"], len(rows))
        self.assertEqual(self.audit["counts"]["distinct_supported_members"], len(set(rows)))


if __name__ == "__main__":
    unittest.main(verbosity=2)
