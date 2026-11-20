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
            [sys.executable, str(HERE / "audit.py"), "--java-home", "/usr/lib/jvm/java-21-openjdk", "--format", "json"],
            capture_output=True, text=True,
        )
        if result.returncode:
            raise AssertionError(f"audit failed ({result.returncode}): {result.stderr}")
        cls.audit = json.loads(result.stdout)
        cls.fixtures = {fixture["fixture"]: fixture for fixture in cls.audit["fixtures"]}

    def assert_method_classified(self, fixture, owner, name, descriptor, *, supported):
        row = self.fixtures[fixture]
        member = [owner, name, descriptor]
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

    def test_gameplay_accepts_coordinates_and_owner_driven_damage_surface(self):
        gameplay = self.fixtures["gameplay"]
        self.assertEqual(gameplay["status"], "COMPATIBLE")
        self.assert_method_classified("gameplay", "net/minecraft/core/BlockPos", "getY", "()I", supported=True)
        event = "net/neoforged/neoforge/event/entity/living/LivingDamageEvent$Pre"
        self.assert_method_classified("gameplay", event, "setNewDamage", "(F)V", supported=True)
        self.assert_method_classified("gameplay", event, "getNewDamage", "()F", supported=True)

    def test_automation_and_network_preserve_erased_jvm_descriptors(self):
        self.assert_method_classified(
            "automation", "net/minecraft/world/level/LevelAccessor", "scheduleTick",
            "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/Block;I)V", supported=True,
        )
        self.assert_method_classified(
            "automation", "net/neoforged/neoforge/items/IItemHandler", "insertItem",
            "(ILnet/minecraft/world/item/ItemStack;Z)Lnet/minecraft/world/item/ItemStack;", supported=True,
        )
        self.assert_method_classified(
            "network", "net/minecraft/resources/ResourceKey", "location",
            "()Lnet/minecraft/resources/ResourceLocation;", supported=True,
        )
        self.assert_method_classified(
            "network", "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent", "registrar",
            "(Ljava/lang/String;)Lnet/neoforged/neoforge/network/registration/PayloadRegistrar;", supported=True,
        )
        self.assert_method_classified("network", "net/minecraft/world/level/Level", "dimension",
                                      "()Lnet/minecraft/resources/ResourceKey;", supported=True)
        self.assert_method_classified("network", "net/minecraft/network/FriendlyByteBuf", "readResourceKey",
                                      "(Lnet/minecraft/resources/ResourceKey;)Lnet/minecraft/resources/ResourceKey;",
                                      supported=True)

    def test_automation_and_network_keep_unbacked_entry_points_closed(self):
        self.assertEqual(self.fixtures["automation"]["status"], "REJECTED")
        self.assertEqual({member[1] for member in self.fixtures["automation"]["unsupported_members"]},
                         {"setChanged", "tick", "registerBlockEntity"})
        self.assertEqual(self.fixtures["network"]["status"], "REJECTED")
        self.assertEqual([member[1] for member in self.fixtures["network"]["unsupported_members"]],
                         ["configurationToClient"])

    def test_declared_transform_diagnostics_distinguish_config_injections_and_at(self):
        transforms = self.fixtures["transforms"]
        reasons = {(reason["code"], reason["symbol"]) for reason in transforms["diagnostics"]}
        for injection in ("ModifyVariable", "Overwrite", "Redirect"):
            self.assertIn(
                ("UNSUPPORTED_MIXIN_INJECTION", f"auditfixture/transforms/WorldMixin @{injection}"), reasons,
            )
        self.assertEqual({symbol for code, symbol in reasons if code == "UNSUPPORTED_MIXIN_AT"},
                         {f"auditfixture/transforms/WorldMixin @At({point})" for point in ("INVOKE", "STORE")})
        self.assertIn(("UNSUPPORTED_MIXIN_CONFIG", "hooks.mixins.json"), reasons)

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
            row = self.fixtures[name]
            self.assertEqual(row["status"], "REJECTED")
            self.assertIn("META-INF/accesstransformer.cfg", row["resources"])
            self.assertEqual([reason["symbol"] for reason in row["diagnostics"]
                              if reason["code"] == "UNSUPPORTED_ACCESS_TRANSFORMER"],
                             ["META-INF/accesstransformer.cfg"])

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
