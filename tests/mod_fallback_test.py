"""4-tier mod classifier + strict/quarantine/delegate fallback launcher tests.

Ports the `plan_mod_startup` invariants from `mod-adapter-core/src/fallback.rs`
into unit tests, then drives `tools/mod_launcher.py` end-to-end with fixture
JARs, canned ShimCheck JSON (`--shimcheck-json`), and stub launch commands so
no Java toolchain, network, or real NeoForge runtime is required.
"""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "mod_launcher.py"
TMP_ROOT = ROOT / "target" / "tmp"

_spec = importlib.util.spec_from_file_location("mod_launcher", TOOL)
mod_launcher = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mod_launcher)

MODS_TOML = '''modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"
[[mods]]
modId="{mod_id}"
version="1.0.0"
displayName="{mod_id}"
[[dependencies.{mod_id}]]
modId="neoforge"
mandatory=true
versionRange="[21.1,)"
side="BOTH"
'''

# Stub "mc-spine" / "NeoForge" server: writes its launch context (argv plus the
# MC_SPINE_* env the launcher exports) to the marker file in argv[1], then
# exits with the code in argv[2] (0 when absent).
STUB = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({
    "argv": sys.argv[2:],
    "plan": os.environ.get("MC_SPINE_LAUNCHER_PLAN"),
    "policy": os.environ.get("MC_SPINE_FALLBACK_POLICY"),
    "mods_dir": os.environ.get("MC_SPINE_MODS_DIR"),
    "staging_dir": os.environ.get("MC_SPINE_STAGING_DIR"),
    "needs_bounded_jvm": os.environ.get("MC_SPINE_NEEDS_BOUNDED_JVM"),
    "quarantined": os.environ.get("MC_SPINE_QUARANTINED_MODS"),
    "tier3": os.environ.get("MC_SPINE_TIER3_MODS"),
    "superseded": os.environ.get("MC_SPINE_SUPERSEDED_PERF_MODS"),
}), encoding="utf-8")
sys.exit(int(sys.argv[2]) if len(sys.argv) > 2 else 0)
'''


def _jar(path, mod_id, entries=(), mc_range=None):
    toml = MODS_TOML.format(mod_id=mod_id)
    if mc_range is not None:
        toml += (
            f"[[dependencies.{mod_id}]]\n"
            'modId="minecraft"\n'
            "mandatory=true\n"
            f'versionRange="{mc_range}"\n'
            'side="BOTH"\n')
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as out:
        out.writestr("META-INF/neoforge.mods.toml", toml)
        out.writestr(f"{mod_id}/Main.class", b"\xca\xfe\xba\xbe")
        for name, payload in entries:
            out.writestr(name, payload)


def _fabric_jar(path, mod_id, depends, entries=()):
    """Fabric-loader jar whose fabric.mod.json declares depends.minecraft."""
    manifest = json.dumps({
        "schemaVersion": 1,
        "id": mod_id,
        "version": "1.0.0",
        "environment": "*",
        "depends": {"minecraft": depends},
    })
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as out:
        out.writestr("fabric.mod.json", manifest)
        out.writestr(f"{mod_id}/Main.class", b"\xca\xfe\xba\xbe")
        for name, payload in entries:
            out.writestr(name, payload)


def _data_zip(path, entries):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as out:
        for name, payload in entries:
            out.writestr(name, payload)


def _shim_report(jar_reports):
    """jar_reports: iterable of (path, status, reasons[code,symbol,detail])."""
    jars = []
    for path, status, reasons in jar_reports:
        jars.append({
            "jar": str(path),
            "status": status,
            "modIds": [Path(path).stem],
            "clientSideOnly": False,
            "reasons": [{"code": c, "symbol": s, "detail": d}
                        for c, s, d in reasons],
            "unsupportedMixins": [
                {"code": c, "symbol": s, "detail": d}
                for c, s, d in reasons if "MIXIN" in c],
            "unsupportedAccessTransformers": [
                {"code": c, "symbol": s, "detail": d}
                for c, s, d in reasons if "ACCESS_TRANSFORMER" in c],
        })
    return {
        "status": "COMPATIBLE"
        if all(j["status"] == "COMPATIBLE" for j in jars) else "REJECTED",
        "minecraft": "1.21.1",
        "neoforge": "21.1.172",
        "jars": jars,
    }


def _fake_mod(mod_id, mod_ids=None, has_java_bytecode=True,
              shimcheck_compatible=True, has_unsupported_mixin_or_at=False,
              reasons=(), has_custom_chunk_generator=False,
              mc_version_range=None, pack_format=None,
              pack_supported_min=None, pack_supported_max=None,
              pack_format_tuple=None, pack_min_format=None,
              pack_max_format=None):
    mod = SimpleNamespace(
        mod_id=mod_id,
        mod_ids=mod_ids or [mod_id],
        has_java_bytecode=has_java_bytecode,
        shimcheck_compatible=shimcheck_compatible,
        has_unsupported_mixin_or_at=has_unsupported_mixin_or_at,
        has_custom_chunk_generator=has_custom_chunk_generator,
        mc_version_range=mc_version_range,
        pack_format=pack_format,
        pack_supported_min=pack_supported_min,
        pack_supported_max=pack_supported_max,
        pack_format_tuple=pack_format_tuple,
        pack_min_format=pack_min_format,
        pack_max_format=pack_max_format,
        source=f"mods/{mod_id}.jar",
    )
    mod.reason_codes = lambda: list(reasons)
    return mod


def _versioned_fake(mod_id, mc_version_range):
    """Java mod with a declared Minecraft range and a ShimCheck rejection."""
    return _fake_mod(
        mod_id, shimcheck_compatible=False,
        has_unsupported_mixin_or_at=True,
        reasons=["UNSUPPORTED_MIXIN_INJECTION"],
        mc_version_range=mc_version_range)


def _dim_type_json():
    return json.dumps({
        "min_y": 0, "height": 384, "coordinate_scale": 1.0,
        "has_skylight": True, "has_ceiling": False,
        "ultrawarm": False, "natural": True,
        "bed_works": True, "respawn_anchor_works": False,
        "ambient_light": 0.0, "effects": "minecraft:overworld",
    })


def _dim_json(gen_type="minecraft:noise", type_id=None):
    return json.dumps({
        "type": type_id or "test:dim",
        "generator": {"type": gen_type,
                      "biome_source": {"type": "minecraft:multi_noise"}},
    })


class ClassifierUnitTests(unittest.TestCase):
    """Ports of the #[cfg(test)] cases in mod-adapter-core/src/fallback.rs."""

    def plan(self, mods, flagship=(), profiles=(), saved=(), policy="delegate", **kw):
        return mod_launcher.plan_startup(
            mods, set(flagship), set(profiles), set(saved), policy, **kw)

    def test_zero_unsupported_mods_always_runs_pure_native_under_delegate(self):
        mods = [
            _fake_mod("create", shimcheck_compatible=False,
                      has_unsupported_mixin_or_at=True,
                      reasons=["UNSUPPORTED_MIXIN_INJECTION"]),
            _fake_mod("twilight_datapack", has_java_bytecode=False),
        ]
        plan = self.plan(mods, flagship=["create"], saved=["create"])
        self.assertEqual(plan["plan"], "RunNative")
        self.assertFalse(plan["needs_bounded_jvm"])
        tiers = {a.mod_id: a.tier for a in plan["assignments"]}
        self.assertEqual(tiers, {"create": 0, "twilight_datapack": 1})
        self.assertEqual(plan["assignments"][0].reasons,
                         ["handled by flagship native Rust adapter"])

    def test_delegate_reports_exact_trigger_and_quarantine_protects_save(self):
        mods = [
            _fake_mod("create", shimcheck_compatible=False,
                      has_unsupported_mixin_or_at=True),
            _fake_mod("heavy_mixin_mod", shimcheck_compatible=False,
                      has_unsupported_mixin_or_at=True,
                      reasons=["UNSUPPORTED_MIXIN_INJECTION"]),
        ]
        delegated = self.plan(mods, flagship=["create"],
                              saved=["heavy_mixin_mod"])
        self.assertEqual(delegated["plan"], "DelegateToNeoForge")
        self.assertEqual(len(delegated["triggering_mods"]), 1)
        self.assertEqual(delegated["triggering_mods"][0].mod_id, "heavy_mixin_mod")
        self.assertEqual(delegated["triggering_mods"][0].reasons,
                         ["UNSUPPORTED_MIXIN_INJECTION"])

        protected = self.plan(mods, flagship=["create"],
                              saved=["heavy_mixin_mod"], policy="quarantine",
                              load_safe_data_only=True)
        self.assertEqual(protected["plan"], "RefuseStartup")
        self.assertEqual(protected["blocking_mods"][0].mod_id, "heavy_mixin_mod")

        isolated = self.plan(mods, flagship=["create"], saved=["create"],
                             policy="quarantine", load_safe_data_only=True)
        self.assertEqual(isolated["plan"], "RunNative")
        self.assertEqual(len(isolated["assignments"]), 1)
        self.assertEqual(len(isolated["quarantined_mods"]), 1)
        self.assertTrue(isolated["load_quarantined_data_only"])

    def test_strict_refuses_and_shim_verified_mod_is_tier2(self):
        mods = [
            _fake_mod("shim_ok"),
            _fake_mod("bad", shimcheck_compatible=False,
                      has_unsupported_mixin_or_at=True,
                      reasons=["UNSUPPORTED_MIXIN_AT"]),
        ]
        native = self.plan(mods[:1])
        self.assertEqual(native["plan"], "RunNative")
        self.assertTrue(native["needs_bounded_jvm"])
        self.assertEqual(native["assignments"][0].tier, 2)

        refused = self.plan(mods, policy="strict")
        self.assertEqual(refused["plan"], "RefuseStartup")
        self.assertEqual(refused["reason"],
                         "strict policy rejected unsupported Tier 3 mod(s)")
        self.assertEqual(refused["blocking_mods"][0].reasons,
                         ["UNSUPPORTED_MIXIN_AT"])

    def test_default_reason_when_shimcheck_gave_no_codes(self):
        mods = [_fake_mod("mystery", shimcheck_compatible=False)]
        tier, reasons = mod_launcher.classify(mods[0], set(), set())
        self.assertEqual(tier, 3)
        self.assertEqual(reasons,
                         ["unsupported Mixin/AT or Minecraft API surface"])

    def test_profile_mod_with_bytecode_is_tier1(self):
        mods = [_fake_mod("mekanism")]
        tier, reasons = mod_launcher.classify(mods[0], set(), {"mekanism"})
        self.assertEqual(tier, 1)
        self.assertEqual(reasons,
                         ["handled by zero-JVM universal adapter / offload profile"])

    def test_custom_chunk_generator_is_tier3_even_without_bytecode(self):
        # A modded Java ChunkGenerator can never run on the native Track 2
        # dynamic-dimension path, even when the jar carries no .class files.
        mods = [_fake_mod("deepdim", has_java_bytecode=False,
                          has_custom_chunk_generator=True)]
        tier, reasons = mod_launcher.classify(mods[0], set(), set())
        self.assertEqual(tier, 3)
        self.assertEqual(reasons, ["CUSTOM_CHUNK_GENERATOR"])

        # Profiles and ShimCheck verdicts cannot override it either.
        tier, reasons = mod_launcher.classify(mods[0], set(), {"deepdim"})
        self.assertEqual((tier, reasons), (3, ["CUSTOM_CHUNK_GENERATOR"]))

    def test_pure_data_dimension_pack_stays_tier1(self):
        mods = [_fake_mod("twilight_datapack", has_java_bytecode=False)]
        tier, reasons = mod_launcher.classify(mods[0], set(), set())
        self.assertEqual(tier, 1)
        self.assertEqual(reasons,
                         ["handled by zero-JVM universal adapter / offload profile"])

    def test_builtin_perf_mods_are_superseded_never_fallback(self):
        """Modpack [create, aether_datapack, sodium, lithium, modernfix]: the
        three mixin-heavy perf mods are BuiltInSuperseded under every policy
        and the pack launches 100% pure native with zero Tier 3."""
        heavy = lambda mod_id: _fake_mod(
            mod_id, shimcheck_compatible=False,
            has_unsupported_mixin_or_at=True,
            reasons=["UNSUPPORTED_MIXIN_INJECTION"])
        mods = [
            heavy("create"),
            _fake_mod("aether_datapack", has_java_bytecode=False),
            heavy("sodium"),
            heavy("lithium"),
            heavy("modernfix"),
        ]
        for policy in ("strict", "quarantine", "delegate"):
            plan = self.plan(mods, flagship=["create"], policy=policy)
            self.assertEqual(plan["plan"], "RunNative", policy)
            self.assertFalse(plan["needs_bounded_jvm"], policy)
            self.assertEqual(plan["superseded_perf_mods"],
                             ["lithium", "modernfix", "sodium"], policy)
            tiers = {a.mod_id: a.tier for a in plan["assignments"]}
            self.assertEqual(tiers, {
                "create": 0,
                "aether_datapack": 1,
                "sodium": mod_launcher.TIER_SUPERSEDED,
                "lithium": mod_launcher.TIER_SUPERSEDED,
                "modernfix": mod_launcher.TIER_SUPERSEDED,
            }, policy)
            for a in plan["assignments"]:
                if a.tier == mod_launcher.TIER_SUPERSEDED:
                    self.assertEqual(
                        a.reasons,
                        ["already covered by mc-spine built-in optimizations"])

    def test_superseded_match_is_case_insensitive_multi_id(self):
        # Any declared id (Fabric or NeoForge) hits the catalog, whatever case.
        mod = _fake_mod("perfpack", mod_ids=["perfpack", "EmbeDDium_Extra"])
        tier, reasons = mod_launcher.classify(mod, set(), set())
        self.assertEqual(tier, mod_launcher.TIER_SUPERSEDED)
        self.assertTrue(mod_launcher.is_builtin_superseded(mod))
        self.assertFalse(
            mod_launcher.is_builtin_superseded(_fake_mod("create")))
        # Required catalog coverage (mirrors fallback.rs 1:1).
        for mod_id in ("sodium", "embeddium", "immediatelyfast", "entityculling",
                       "moreculling", "distanthorizons", "voxy", "krypton",
                       "lazydfu", "memoryleakfix", "alternate-current", "clumps",
                       "lithium", "starlight", "ferritecore", "modernfix",
                       "noisium", "c2me"):
            self.assertIn(mod_id, mod_launcher.BUILTIN_SUPERSEDED_PERF_MODS)

    def test_superseded_perf_mod_skips_version_check_entirely(self):
        # `sodium` built only for 1.20.1 on a 1.21.1 server: the superseded
        # catalog wins before the version gate, so it is still a clean
        # BuiltInSuperseded skip — never a version-mismatch Tier 3 report.
        sodium = _versioned_fake("sodium", "[1.20.1,1.20.2)")
        tier, reasons = mod_launcher.classify(sodium, set(), set())
        self.assertEqual(tier, mod_launcher.TIER_SUPERSEDED)
        self.assertEqual(reasons,
                         ["already covered by mc-spine built-in optimizations"])

    def test_incompatible_declared_range_is_tier3_with_plain_language_reason(self):
        # A real content mod declaring a 1.20.1-only Maven range on a 1.21.1
        # server must never silently execute broken native code.
        jei = _versioned_fake("jei", "[1.20.1,1.20.2)")
        tier, reasons = mod_launcher.classify(jei, set(), set())
        self.assertEqual(tier, 3)
        self.assertEqual(reasons, [
            "MC_VERSION_INCOMPATIBLE",
            "built for Minecraft [1.20.1,1.20.2), server is running 1.21.1",
        ])

    def test_compatible_declared_range_runs_on_every_target_in_band(self):
        m = _versioned_fake("somecontent", "[1.21.1,1.21.5)")
        for target in ((1, 21, 1), (1, 21, 3), (1, 21, 4)):
            # Mixin-incompatible with no profile: falls through to Tier 3
            # *only* on technical grounds — the version gate stays silent.
            _, reasons = mod_launcher.classify(m, set(), set(), target)
            self.assertEqual(reasons, ["UNSUPPORTED_MIXIN_INJECTION"], target)
        # A profile-backed mod stays Tier 1 on every in-band target.
        for target in ((1, 21, 1), (1, 21, 4)):
            tier, _ = mod_launcher.classify(m, set(), {"somecontent"}, target)
            self.assertEqual(tier, 1, target)

    def test_invalid_declared_range_fails_closed_as_tier3(self):
        m = _versioned_fake("broken_manifest", "not-a-version-range!!")
        tier, reasons = mod_launcher.classify(m, set(), set())
        self.assertEqual(tier, 3)
        self.assertEqual(reasons[0], "MC_VERSION_RANGE_INVALID")
        self.assertEqual(len(reasons), 2)
        self.assertTrue(reasons[1].startswith(
            "declares an invalid Minecraft version range"))
        self.assertIn("not-a-version-range!!", reasons[1])

    def test_plan_startup_for_version_flags_mismatch_per_policy(self):
        mods = [
            _versioned_fake("jei", "[1.20.1,1.20.2)"),
            _versioned_fake("sodium", "[1.20.1,1.20.2)"),
            _versioned_fake("okmod", "[1.21.1,1.21.5)"),
        ]
        # Under delegate, only `jei` triggers; sodium is still superseded.
        plan = self.plan(mods, profiles=["okmod"], policy="delegate")
        self.assertEqual(plan["plan"], "DelegateToNeoForge")
        triggers = plan["triggering_mods"]
        self.assertEqual(len(triggers), 1)
        self.assertEqual(triggers[0].mod_id, "jei")
        self.assertEqual(triggers[0].reasons, [
            "MC_VERSION_INCOMPATIBLE",
            "built for Minecraft [1.20.1,1.20.2), server is running 1.21.1",
        ])
        tiers = {a.mod_id: a.tier for a in plan["assignments"]}
        self.assertEqual(tiers, {
            "jei": 3,
            "sodium": mod_launcher.TIER_SUPERSEDED,
            "okmod": 1,
        })
        self.assertEqual(plan["mc_version"], "1.21.1")
        # The declared range and verdict are recorded on every assignment.
        by_id = {a.mod_id: a for a in plan["assignments"]}
        self.assertEqual(by_id["jei"].as_dict()["mc_version_range"],
                         "[1.20.1,1.20.2)")
        self.assertEqual(by_id["jei"].as_dict()["mc_version_verdict"],
                         "major_mismatch")
        self.assertEqual(by_id["okmod"].as_dict()["mc_version_verdict"],
                         "compatible")
        # Sodium's range is recorded for audit but never gated on.
        self.assertEqual(by_id["sodium"].as_dict()["mc_version_range"],
                         "[1.20.1,1.20.2)")
        self.assertIsNone(by_id["sodium"].as_dict()["mc_version_verdict"])
        # Drop `jei`: every remaining declared range holds on 1.21.4 too.
        plan = self.plan(mods[1:], profiles=["okmod"], policy="delegate",
                         target_mc_version=(1, 21, 4))
        self.assertEqual(plan["plan"], "RunNative")
        self.assertEqual(plan["mc_version"], "1.21.4")
        # Strict refuses cleanly with the version reason in the audit trail.
        plan = self.plan([_versioned_fake("jei", "[1.20.1,1.20.2)")],
                         policy="strict")
        self.assertEqual(plan["plan"], "RefuseStartup")
        self.assertIn("server is running 1.21.1",
                      plan["blocking_mods"][0].reasons[1])

    def test_pure_data_pack_format_gate(self):
        # A datapack declaring pack_format 48 plus a supported span covering
        # 1.21.0-1.21.5 stays Tier 1 on 1.21.1 and 1.21.4 alike.
        good = _fake_mod("okpack", has_java_bytecode=False,
                         pack_format=48,
                         pack_supported_min=48, pack_supported_max=61)
        for target in ((1, 21, 1), (1, 21, 4)):
            tier, reasons = mod_launcher.classify(good, set(), set(), target)
            self.assertEqual(tier, 1, target)
            self.assertEqual(reasons, [
                "handled by zero-JVM universal adapter / offload profile"])
        # A 1.20.4-era pack (pack_format 26) fails closed on 1.21.1.
        old = _fake_mod("oldpack", has_java_bytecode=False, pack_format=26)
        tier, reasons = mod_launcher.classify(old, set(), set())
        self.assertEqual(tier, 3)
        self.assertEqual(reasons[0], "PACK_FORMAT_INCOMPATIBLE")
        self.assertIn("pack_format 26", reasons[1])
        self.assertIn("1.21.1", reasons[1])
        # Unknown server pack_format fails closed too.
        tier, _ = mod_launcher.classify(old, set(), set(), (1, 19, 4))
        self.assertEqual(tier, 3)

    def test_pure_data_pack_26_x_min_max_format_tuple_gate(self):
        # The 26.x schema: pack_format [121, 0] plus a min_format/max_format
        # span. Exact match and span-coverage both load on 26.3.
        new26 = _fake_mod("new26", has_java_bytecode=False,
                          pack_format_tuple=(121, 0),
                          pack_min_format=(121, 0), pack_max_format=(121, 0))
        tier, reasons = mod_launcher.classify(new26, set(), set(), (26, 3, 0))
        self.assertEqual(tier, 1)
        self.assertEqual(reasons, [
            "handled by zero-JVM universal adapter / offload profile"])
        # A pack spanning both targets is Tier 1 on either release line.
        universal = _fake_mod("universal", has_java_bytecode=False,
                              pack_format_tuple=(48, 0),
                              pack_min_format=(48, 0),
                              pack_max_format=(121, 0))
        for target in ((1, 21, 1), (26, 3, 0)):
            tier, _ = mod_launcher.classify(universal, set(), set(), target)
            self.assertEqual(tier, 1, target)
        # A legacy scalar pack_format 48 is refused on 26.3 (format 121).
        legacy = _fake_mod("legacy", has_java_bytecode=False, pack_format=48)
        tier, reasons = mod_launcher.classify(legacy, set(), set(), (26, 3, 0))
        self.assertEqual(tier, 3)
        self.assertEqual(reasons[0], "PACK_FORMAT_INCOMPATIBLE")
        self.assertIn("pack_format 48", reasons[1])
        self.assertIn("26.3", reasons[1])
        # And the inverse: a 26.3-only tuple pack is refused on 1.21.1.
        tier, reasons = mod_launcher.classify(new26, set(), set(), (1, 21, 1))
        self.assertEqual(tier, 3)
        self.assertEqual(reasons[0], "PACK_FORMAT_INCOMPATIBLE")
        self.assertIn("min_format", reasons[1])
        self.assertIn("1.21.1", reasons[1])

    def test_mc_version_26_3_gates_ranges_but_sodium_stays_superseded(self):
        # NeoForge [26.3,26.4) and Fabric >=26.3 declare the new release line.
        for mod in (_versioned_fake("n26", "[26.3,26.4)"),
                    _versioned_fake("f26", ">=26.3")):
            _, reasons = mod_launcher.classify(mod, set(), set(), (26, 3, 0))
            # Mixin-incompatible with no profile: Tier 3 on technical grounds
            # only — the version gate stays silent on its declared target.
            self.assertEqual(reasons, ["UNSUPPORTED_MIXIN_INJECTION"])
        # A 1.21.1-only mod on 26.3 is a major mismatch, never silent.
        old = _versioned_fake("oldmod", "[1.21.1]")
        tier, reasons = mod_launcher.classify(old, set(), set(), (26, 3, 0))
        self.assertEqual(tier, 3)
        self.assertEqual(reasons, [
            "MC_VERSION_INCOMPATIBLE",
            "built for Minecraft [1.21.1], server is running 26.3.0",
        ])
        # sodium declaring a 1.21.1-only range is still BuiltInSuperseded on
        # 26.3 — the catalog wins before the version gate.
        sodium = _versioned_fake("sodium", "[1.21.1]")
        tier, reasons = mod_launcher.classify(sodium, set(), set(), (26, 3, 0))
        self.assertEqual(tier, mod_launcher.TIER_SUPERSEDED)
        self.assertEqual(reasons,
                         ["already covered by mc-spine built-in optimizations"])


class VersionCompatUnitTests(unittest.TestCase):
    """Ports of the #[cfg(test)] cases in mod-adapter-core/src/version_compat.rs."""

    def vrange(self, s):
        return mod_launcher.parse_version_range(s)

    def test_mc_version_parse_and_ordering(self):
        v = mod_launcher.parse_mc_version
        self.assertEqual(v("1.21.1"), (1, 21, 1))
        self.assertEqual(v("1.21"), (1, 21, 0))
        self.assertEqual(v("1"), (1, 0, 0))
        self.assertIsNone(v(""))
        self.assertIsNone(v("1.21.1.4"))
        self.assertIsNone(v("1.x"))
        self.assertIsNone(v("abc"))
        self.assertIsNone(v("1.65536.0"))
        self.assertLess((1, 20, 6), (1, 21, 0))
        self.assertLess((1, 21, 1), (1, 21, 3))
        self.assertEqual(mod_launcher.format_mc_version((1, 21, 1)), "1.21.1")
        self.assertEqual(mod_launcher.format_mc_version(v("1.21")), "1.21.0")
        # Year-based 26.x versions parse and order past the 1.x line.
        self.assertEqual(v("26.3"), (26, 3, 0))
        self.assertEqual(v("26.3.1"), (26, 3, 1))
        self.assertEqual(mod_launcher.format_mc_version(v("26.3")), "26.3.0")
        self.assertLess((1, 21, 5), (26, 0, 0))
        self.assertLess((26, 3, 0), (26, 4, 0))

    def test_pack_format_and_data_version_table(self):
        pf = mod_launcher.data_pack_format_for
        dv = mod_launcher.data_version_for
        pft = mod_launcher.data_pack_format_tuple_for
        self.assertEqual(pf((1, 20, 4)), 26)
        self.assertEqual(dv((1, 20, 4)), 3700)
        self.assertEqual(pf((1, 20, 6)), 41)
        self.assertEqual(dv((1, 20, 6)), 3839)
        self.assertEqual(pf((1, 21, 0)), 48)
        self.assertEqual(dv((1, 21, 0)), 3953)
        self.assertEqual(pf((1, 21, 1)), 48)
        self.assertEqual(dv((1, 21, 1)), 3955)
        self.assertEqual(pf((1, 21, 3)), 57)
        self.assertEqual(dv((1, 21, 3)), 4082)
        self.assertEqual(pf((1, 21, 4)), 61)
        self.assertEqual(dv((1, 21, 4)), 4189)
        self.assertEqual(pf((1, 21, 5)), 71)
        self.assertEqual(dv((1, 21, 5)), 4325)
        self.assertEqual(pf((26, 3, 0)), 121)
        self.assertEqual(dv((26, 3, 0)), 5023)
        self.assertIsNone(pf((1, 19, 4)))
        self.assertIsNone(dv((2, 0, 0)))
        # The (major, minor) tuple mirrors: legacy versions get a zero minor.
        self.assertEqual(pft((1, 21, 1)), (48, 0))
        self.assertEqual(pft((26, 3, 0)), (121, 0))
        self.assertIsNone(pft((1, 19, 4)))

    def test_check_pack_format_across_supported_targets(self):
        cpf = mod_launcher.check_pack_format
        # Exact-format pack built for 1.21.1 (pack_format 48).
        self.assertTrue(cpf((1, 21, 1), 48))
        self.assertFalse(cpf((1, 21, 4), 48))
        self.assertFalse(cpf((1, 20, 4), 48))
        # Pack declaring a supported span covering 1.21.0-1.21.5.
        self.assertTrue(cpf((1, 21, 1), 48, 48, 71))
        self.assertTrue(cpf((1, 21, 4), 48, 48, 71))
        self.assertFalse(cpf((1, 20, 6), 48, 48, 71))
        # Min-only / max-only bounds.
        self.assertTrue(cpf((1, 21, 5), 48, 48, None))
        self.assertTrue(cpf((1, 20, 4), 48, None, 71))
        # Unknown target format fails closed.
        self.assertFalse(cpf((1, 19, 4), 26, 1, 99))

    def test_check_pack_format_tuple_covers_26_3_min_max_format(self):
        cpf = mod_launcher.check_pack_format_tuple
        # A 26.3 datapack declaring min_format == max_format == [121, 0].
        self.assertTrue(cpf((26, 3, 0), (121, 0), (121, 0), (121, 0)))
        self.assertFalse(cpf((1, 21, 1), (121, 0), (121, 0), (121, 0)))
        # A pack spanning both targets: min (48,0) - max (121,0).
        self.assertTrue(cpf((1, 21, 1), (48, 0), (48, 0), (121, 0)))
        self.assertTrue(cpf((26, 3, 0), (48, 0), (48, 0), (121, 0)))
        # Tuples order lexicographically: (121, 0) < (121, 1).
        self.assertFalse(cpf((26, 3, 0), (121, 1), (121, 1), None))
        self.assertTrue(cpf((26, 3, 0), (121, 0), (120, 0), None))
        # Exact format match without a declared span.
        self.assertTrue(cpf((26, 3, 0), (121, 0)))
        self.assertFalse(cpf((1, 21, 4), (121, 0)))
        # Unknown target format fails closed.
        self.assertFalse(cpf((9, 9, 9), (121, 0), (0, 0), None))

    def test_year_based_26_x_versions_parse_and_range(self):
        # `[26.3]`, `[26.3,26.4)`, `>=26.3`, `26.3.x` behave like any other
        # numeric bound in both supported syntaxes.
        pin = self.vrange("[26.3]")
        self.assertTrue(pin.contains((26, 3, 0)))
        self.assertFalse(pin.contains((26, 3, 1)))
        r = self.vrange("[26.3,26.4)")
        self.assertTrue(r.contains((26, 3, 5)))
        self.assertFalse(r.contains((26, 4, 0)))
        self.assertTrue(self.vrange(">=26.3").contains((26, 9, 0)))
        self.assertTrue(self.vrange("26.3.x").contains((26, 3, 7)))
        self.assertFalse(self.vrange("26.3.x").contains((26, 4, 0)))
        # A 1.21.1-only range never covers 26.3 (different release line).
        verdict = mod_launcher.check_mod_mc_version((26, 3, 0), "[1.21.1]")
        self.assertEqual(verdict.kind, "major_mismatch")

    def test_maven_exact_pins_and_intervals(self):
        pin = self.vrange("[1.21.1]")
        self.assertTrue(pin.contains((1, 21, 1)))
        self.assertFalse(pin.contains((1, 21, 0)))
        self.assertFalse(pin.contains((1, 21, 3)))

        r = self.vrange("[1.21.1,1.21.5)")
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 21, 3)))
        self.assertTrue(r.contains((1, 21, 4)))
        self.assertFalse(r.contains((1, 21, 5)))
        self.assertFalse(r.contains((1, 21, 0)))
        self.assertFalse(r.contains((1, 20, 6)))

        r = self.vrange("(1.21.1,1.21.4]")
        self.assertFalse(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 21, 3)))
        self.assertTrue(r.contains((1, 21, 4)))
        self.assertFalse(r.contains((1, 21, 5)))

        # Unbounded both ways.
        self.assertTrue(self.vrange("[1.21.1,)").contains((1, 21, 5)))
        self.assertFalse(self.vrange("[1.21.1,)").contains((1, 21, 0)))
        self.assertTrue(self.vrange("(,1.21.5)").contains((1, 20, 4)))
        self.assertFalse(self.vrange("(,1.21.5)").contains((1, 21, 5)))
        self.assertTrue(self.vrange("[,1.21.5]").contains((1, 21, 5)))

        # Two-component bounds: "1.21" is 1.21.0.
        r = self.vrange("(1.21,1.22]")
        self.assertFalse(r.contains((1, 21, 0)))
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 22, 0)))
        self.assertFalse(r.contains((1, 22, 1)))

        # Maven union `(,1.20.5],[1.21.1,)`: a gap across 1.20.6-1.21.0.
        r = self.vrange("(,1.20.5],[1.21.1,)")
        self.assertTrue(r.contains((1, 20, 4)))
        self.assertTrue(r.contains((1, 20, 5)))
        self.assertFalse(r.contains((1, 20, 6)))
        self.assertFalse(r.contains((1, 21, 0)))
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 21, 5)))

        # Bare exact version in the Maven ecosystem.
        r = self.vrange("1.21.1")
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertFalse(r.contains((1, 21, 3)))

    def test_semver_comparators_tilde_caret_wildcards(self):
        r = self.vrange(">=1.21.1")
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((2, 0, 0)))
        self.assertFalse(r.contains((1, 21, 0)))

        r = self.vrange(">=1.21.1 <1.21.5")
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 21, 4)))
        self.assertFalse(r.contains((1, 21, 5)))
        self.assertFalse(r.contains((1, 21, 0)))

        # `~1.21.1` == `>=1.21.1 <1.22.0`.
        r = self.vrange("~1.21.1")
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 21, 99)))
        self.assertFalse(r.contains((1, 22, 0)))
        self.assertFalse(r.contains((1, 21, 0)))

        # `^1.21.1` == `>=1.21.1 <2.0.0`; `^0.x` keeps the minor band.
        r = self.vrange("^1.21.1")
        self.assertTrue(r.contains((1, 99, 0)))
        self.assertFalse(r.contains((2, 0, 0)))
        r = self.vrange("^0.2.3")
        self.assertTrue(r.contains((0, 2, 9)))
        self.assertFalse(r.contains((0, 3, 0)))
        self.assertFalse(r.contains((0, 2, 2)))

        # Wildcards: `1.21.x` == `>=1.21.0 <1.22.0`; `1.x` == `>=1.0.0 <2.0.0`.
        r = self.vrange("1.21.x")
        self.assertTrue(r.contains((1, 21, 0)))
        self.assertTrue(r.contains((1, 21, 99)))
        self.assertFalse(r.contains((1, 22, 0)))
        self.assertFalse(r.contains((1, 20, 9)))
        self.assertTrue(self.vrange("1.21.*").contains((1, 21, 4)))
        self.assertTrue(self.vrange("1.x").contains((1, 2, 3)))
        self.assertFalse(self.vrange("1.x").contains((2, 0, 0)))
        self.assertTrue(self.vrange("*").contains((9, 9, 9)))

        # `<`/`>`/`<=` on partial versions use the band edges.
        r = self.vrange("<1.22")
        self.assertTrue(r.contains((1, 21, 9)))
        self.assertFalse(r.contains((1, 22, 0)))
        # npm band semantics: `>1.21` means "after the 1.21.x line" (>=1.22.0).
        r = self.vrange(">1.21")
        self.assertFalse(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 22, 0)))
        self.assertFalse(r.contains((1, 21, 0)))
        self.assertTrue(self.vrange(">1.21.0").contains((1, 21, 1)))
        r = self.vrange("<=1.21.x")
        self.assertTrue(r.contains((1, 21, 99)))
        self.assertFalse(r.contains((1, 22, 0)))

        # `=` pin and OR union.
        self.assertTrue(self.vrange("=1.21.1").contains((1, 21, 1)))
        self.assertFalse(self.vrange("=1.21.1").contains((1, 21, 3)))
        r = self.vrange("1.21.1 || 1.21.4")
        self.assertTrue(r.contains((1, 21, 1)))
        self.assertTrue(r.contains((1, 21, 4)))
        self.assertFalse(r.contains((1, 21, 3)))
        r = self.vrange("1.20.x || >=1.21.1 <1.22")
        self.assertTrue(r.contains((1, 20, 1)))
        self.assertTrue(r.contains((1, 21, 5)))
        self.assertFalse(r.contains((1, 21, 0)))
        self.assertFalse(r.contains((1, 22, 0)))

        # Partial bare `1.21` covers the whole 1.21.x band.
        r = self.vrange("1.21")
        self.assertTrue(r.contains((1, 21, 0)))
        self.assertTrue(r.contains((1, 21, 9)))
        self.assertFalse(r.contains((1, 22, 0)))

    def test_malformed_ranges_fail_closed_never_panic(self):
        for bad in ("", "   ", "[", "(", "]", "[1.21.1", "[1.21.1,",
                    "[1.21.1,1.21.5,1.21.9)", "[]", "(1.21.1)", "[1.21.1] extra",
                    ">=", "~", "1.x.2", "1.2.3.4", "abc", ">=abc",
                    "1.21.1 ||", "|| 1.21.1", "1.21.1 1.21.4 junk",
                    "1.21.1-rc1", "<0.0.0 garbage", ",", "[1.21.1],garbage!"):
            with self.assertRaises(mod_launcher.VersionRangeError, msg=bad):
                self.vrange(bad)
        # `<0.0.0` parses but matches nothing.
        self.assertFalse(self.vrange("<0.0.0").contains((0, 0, 0)))

    def test_mod_version_verdict_distinguishes_minor_and_major(self):
        target = (1, 21, 1)
        check = mod_launcher.check_mod_mc_version
        self.assertEqual(check(target, "[1.21.1,1.22)").kind, "compatible")
        self.assertEqual(check(target, ">=1.20 <1.22").kind, "compatible")
        # Same 1.21.x line, outside the declared window -> minor.
        verdict = check(target, ">=1.21.3")
        self.assertEqual(verdict.kind, "minor_mismatch")
        self.assertEqual(verdict.declared, ">=1.21.3")
        verdict = check(target, "[1.21.3,1.21.5)")
        self.assertEqual(verdict.kind, "minor_mismatch")
        self.assertEqual(verdict.declared, "[1.21.3,1.21.5)")
        # A different release line entirely -> major.
        verdict = check(target, "[1.20.1,1.20.2)")
        self.assertEqual(verdict.kind, "major_mismatch")
        self.assertEqual(verdict.declared, "[1.20.1,1.20.2)")
        verdict = check(target, "1.20.x")
        self.assertEqual(verdict.kind, "major_mismatch")
        self.assertEqual(verdict.declared, "1.20.x")
        # A union spanning both lines still admits a 1.21.x version: minor.
        verdict = check(target, "1.20.x || >=1.21.3 <1.22")
        self.assertEqual(verdict.kind, "minor_mismatch")
        self.assertEqual(verdict.declared, "1.20.x || >=1.21.3 <1.22")
        # Malformed input reports invalid_range with the reason preserved.
        verdict = check(target, "bogus!!")
        self.assertEqual(verdict.kind, "invalid_range")
        self.assertEqual(verdict.declared, "bogus!!")
        self.assertTrue(verdict.reason)
        self.assertEqual(check(target, "  ").kind, "invalid_range")


class LauncherIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        TMP_ROOT.mkdir(parents=True, exist_ok=True)

    def setUp(self):
        self.ws = tempfile.TemporaryDirectory(prefix="mod-fallback-", dir=TMP_ROOT)
        self.tmp = Path(self.ws.name)
        self.mods = self.tmp / "mods"
        self.mods.mkdir()
        self.profiles = self.tmp / "profiles"
        self.profiles.mkdir()
        self.staging = self.tmp / "staging"
        self.stub = self.tmp / "stub_server.py"
        self.stub.write_text(STUB, encoding="utf-8")
        self.env = dict(os.environ, TMPDIR=str(TMP_ROOT))

    def tearDown(self):
        self.ws.cleanup()

    def cmd(self, marker, exit_code=0):
        return f"{sys.executable} {self.stub} {marker} {exit_code}"

    def write_shim(self, *jar_reports):
        path = self.tmp / "shimcheck.json"
        path.write_text(json.dumps(_shim_report(jar_reports)), encoding="utf-8")
        return path

    def invoke(self, *argv):
        command = [sys.executable, str(TOOL),
                   "--mods-dir", str(self.mods),
                   "--staging-dir", str(self.staging)] + list(argv)
        proc = subprocess.run(command, cwd=ROOT, env=self.env,
                              capture_output=True, text=True)
        report = None
        if "--json" in argv and proc.stdout.strip():
            report = json.loads(proc.stdout)
        return proc, report

    def test_native_fast_path_never_starts_neoforge(self):
        """create flagship + pure-data pack + mekanism profile => all Tier 0/1.

        Under --fallback delegate with a NeoForge fallback command wired up,
        0 Tier 3 mods must launch mc-spine directly and never touch the
        NeoForge runtime (zero additional RSS / startup time invariant).
        """
        create = self.mods / "create.jar"
        _jar(create, "create")
        twilight = self.mods / "twilight_dim.zip"
        _data_zip(twilight, [("data/twilight/dimension/twilight.json", "{}"),
                             ("pack.mcmeta", '{"pack":{"pack_format":48}}')])
        mek = self.mods / "mekanism.jar"
        _jar(mek, "mekanism")
        (self.profiles / "mekanism.json").write_text(json.dumps({
            "mod_id": "mekanism",
            "blocks": [{"id": "mekanism:crusher", "role": "processor"}],
        }), encoding="utf-8")
        shim = self.write_shim(
            (create, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "c.Mix @Redirect", "Unsupported S0 injection")]),
            (mek, "REJECTED",
             [("UNSUPPORTED_SYMBOL", "net.minecraft.Foo#bar", "Referenced from M")]),
        )
        mc_marker = self.tmp / "mc.json"
        neo_marker = self.tmp / "neo.json"
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--profiles-dir", str(self.profiles),
            "--shimcheck-json", str(shim),
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--neoforge-fallback-cmd", self.cmd(neo_marker),
            "--world-namespaces", "create,minecraft",
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertFalse(report["needs_bounded_jvm"])
        tiers = {a["mod_id"]: a["tier_name"] for a in report["assignments"]}
        self.assertEqual(tiers, {
            "create": "NativeFlagship",
            "mekanism": "UniversalNative",
            "twilight": "UniversalNative",
        })
        self.assertEqual(report["launch"]["kind"], "mc_spine")
        self.assertTrue(report["launch"]["executed"])
        self.assertTrue(mc_marker.is_file(), "mc-spine stub never ran")
        marker = json.loads(mc_marker.read_text())
        self.assertEqual(marker["plan"], "RunNative")
        self.assertEqual(marker["needs_bounded_jvm"], "false")
        self.assertFalse(neo_marker.exists(),
                         "NeoForge fallback ran despite 0 Tier 3 mods")

    def test_shim_compatible_java_mod_runs_tier2_bounded_jvm(self):
        good = self.mods / "goodjava.jar"
        _jar(good, "goodjava")
        shim = self.write_shim((good, "COMPATIBLE", []))
        mc_marker = self.tmp / "mc.json"
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertTrue(report["needs_bounded_jvm"])
        assignment = report["assignments"][0]
        self.assertEqual(assignment["tier_name"], "BoundedJvmShim")
        self.assertEqual(assignment["reasons"],
                         ["verified compatible with bounded S0-S4 JVM shim"])
        marker = json.loads(mc_marker.read_text())
        self.assertEqual(marker["needs_bounded_jvm"], "true")

    def test_strict_refuses_startup_with_exact_reason_codes(self):
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect",
               "Unsupported S0 injection"),
              ("UNSUPPORTED_ACCESS_TRANSFORMER", "META-INF/accesstransformer.cfg",
               "Access transformers are unsupported in S0")]))
        mc_marker = self.tmp / "mc.json"
        proc, report = self.invoke(
            "--fallback", "strict",
            "--shimcheck-json", str(shim),
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--json")
        self.assertEqual(proc.returncode, 2, (proc.returncode, proc.stderr))
        self.assertEqual(report["plan"], "RefuseStartup")
        self.assertEqual(report["refusal_reason"],
                         "strict policy rejected unsupported Tier 3 mod(s)")
        blocking = report["blocking_mods"]
        self.assertEqual([a["mod_id"] for a in blocking], ["badmixin"])
        self.assertIn("UNSUPPORTED_MIXIN_INJECTION", blocking[0]["reasons"])
        self.assertIn("UNSUPPORTED_ACCESS_TRANSFORMER", blocking[0]["reasons"])
        self.assertFalse(report["launch"]["executed"])
        self.assertFalse(mc_marker.exists())

    def test_quarantine_refuses_when_namespace_is_in_saved_world(self):
        create = self.mods / "create.jar"
        _jar(create, "create")
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (create, "COMPATIBLE", []),
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect",
               "Unsupported S0 injection")]))
        proc, report = self.invoke(
            "--fallback", "quarantine",
            "--shimcheck-json", str(shim),
            "--world-namespaces", "create,minecraft,badmixin",
            "--mc-spine-cmd", self.cmd(self.tmp / "mc.json"),
            "--json")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(report["plan"], "RefuseStartup")
        self.assertIn("saved world", report["refusal_reason"])
        self.assertEqual([a["mod_id"] for a in report["blocking_mods"]],
                         ["badmixin"])

    def test_quarantine_stages_safe_mods_and_launches_mc_spine(self):
        create = self.mods / "create.jar"
        _jar(create, "create")
        twilight = self.mods / "twilight_dim.zip"
        _data_zip(twilight, [("data/twilight/dimension/twilight.json", "{}")])
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin",
             entries=[("data/badmixin/recipes/thing.json",
                       '{"type":"minecraft:crafting_shaped"}')])
        shim = self.write_shim(
            (create, "COMPATIBLE", []),
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect",
               "Unsupported S0 injection")]))
        mc_marker = self.tmp / "mc.json"
        proc, report = self.invoke(
            "--fallback", "quarantine",
            "--shimcheck-json", str(shim),
            "--world-namespaces", "create,minecraft",
            "--quarantine-load-data",
            "--mc-spine-cmd", self.cmd(mc_marker) + " {mods_dir}",
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertEqual([a["mod_id"] for a in report["quarantined_mods"]],
                         ["badmixin"])
        staged = {p.name for p in (self.staging / "mods").iterdir()}
        self.assertEqual(staged, {"create.jar", "twilight_dim.zip"})
        self.assertFalse((self.staging / "mods" / "badmixin.jar").exists())
        extract = self.staging / "quarantined_data" / "badmixin" \
            / "data" / "badmixin" / "recipes" / "thing.json"
        self.assertTrue(extract.is_file(), "quarantined data-only extract missing")
        quarantine_report = json.loads(
            (self.staging / "quarantine_report.json").read_text())
        self.assertEqual(quarantine_report["quarantined_mods"][0]["mod_id"],
                         "badmixin")
        marker = json.loads(mc_marker.read_text())
        self.assertEqual(marker["plan"], "RunNative")
        self.assertEqual(marker["mods_dir"], str(self.staging / "mods"))
        self.assertEqual(marker["quarantined"], "badmixin")
        self.assertEqual(marker["argv"][-1], str(self.staging / "mods"))

    def test_quarantine_force_drop_saved_blocks_allows_startup(self):
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect", "x")]))
        mc_marker = self.tmp / "mc.json"
        proc, report = self.invoke(
            "--fallback", "quarantine",
            "--shimcheck-json", str(shim),
            "--world-namespaces", "badmixin",
            "--force-drop-saved-blocks",
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertTrue(mc_marker.is_file())

    def test_save_file_namespaces_feed_quarantine_guard(self):
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect", "x")]))
        save_dir = self.tmp / "world"
        (save_dir / "region").mkdir(parents=True)
        (save_dir / "region" / "r.0.0.mca").write_bytes(
            b"\x00\x01palette\x00badmixin:cool_block[facing=north]"
            b"minecraft:stone create:shaft")
        proc, report = self.invoke(
            "--fallback", "quarantine",
            "--shimcheck-json", str(shim),
            "--save-file", str(save_dir),
            "--mc-spine-cmd", self.cmd(self.tmp / "mc.json"),
            "--json")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(report["plan"], "RefuseStartup")
        self.assertIn("badmixin", report["saved_world_namespaces"])

    def test_delegate_executes_neoforge_and_propagates_exit_code(self):
        create = self.mods / "create.jar"
        _jar(create, "create")
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (create, "COMPATIBLE", []),
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect",
               "Unsupported S0 injection")]))
        mc_marker = self.tmp / "mc.json"
        neo_marker = self.tmp / "neo.json"
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--world-namespaces", "badmixin",
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--neoforge-fallback-cmd", self.cmd(neo_marker, 7),
            "--json")
        self.assertEqual(proc.returncode, 7, (proc.returncode, proc.stderr))
        self.assertEqual(report["plan"], "DelegateToNeoForge")
        self.assertEqual(report["launch"]["exit_code"], 7)
        self.assertFalse(mc_marker.exists(), "mc-spine ran during delegation")
        self.assertTrue(neo_marker.is_file(), "NeoForge fallback never ran")
        marker = json.loads(neo_marker.read_text())
        self.assertEqual(marker["plan"], "DelegateToNeoForge")
        self.assertEqual(marker["tier3"], "badmixin")
        triggers = report["triggering_mods"]
        self.assertEqual([a["mod_id"] for a in triggers], ["badmixin"])
        self.assertIn("UNSUPPORTED_MIXIN_INJECTION", triggers[0]["reasons"])
        self.assertIn("badmixin -> UNSUPPORTED_MIXIN_INJECTION", proc.stderr)

    def test_delegate_without_neoforge_cmd_refuses_with_instructions(self):
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect", "x")]))
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--mc-spine-cmd", self.cmd(self.tmp / "mc.json"),
            "--json")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(report["plan"], "DelegateToNeoForge")
        self.assertIn("--neoforge-fallback-cmd", proc.stderr)
        self.assertFalse(report["launch"]["executed"])

    def test_delegate_dry_run_reports_plan_without_launching(self):
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin")
        shim = self.write_shim(
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect", "x")]))
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "DelegateToNeoForge")
        self.assertTrue(report["dry_run"])
        self.assertFalse(report["launch"]["executed"])

    def test_pure_data_dimension_mod_jar_is_tier1_zero_jvm(self):
        """Aether-style data-driven dimension mod: dimension_type + dimension
        + worldgen + ore tags, vanilla noise generator, zero .class files =>
        Tier 1, mc-spine launches natively and NeoForge never starts."""
        aether = self.mods / "aetherlike.jar"
        _data_zip(aether, [
            ("data/aetherlike/dimension_type/the_aether.json", _dim_type_json()),
            ("data/aetherlike/dimension/the_aether.json",
             _dim_json("minecraft:noise", "aetherlike:the_aether")),
            ("data/aetherlike/worldgen/noise_settings/aether_noise.json", "{}"),
            ("data/aetherlike/tags/block/ores.json",
             '{"values": ["aetherlike:ambrosium_ore", "#forge:ores"]}'),
            ("data/c/tags/blocks/chests.json",
             '{"values": [{"id": "aetherlike:skyroot_chest", "required": false}]}'),
            ("pack.mcmeta", '{"pack":{"pack_format":48}}'),
        ])
        mc_marker = self.tmp / "mc.json"
        neo_marker = self.tmp / "neo.json"
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--neoforge-fallback-cmd", self.cmd(neo_marker),
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertFalse(report["needs_bounded_jvm"])
        assignment = report["assignments"][0]
        self.assertEqual(assignment["tier"], 1)
        self.assertEqual(assignment["tier_name"], "UniversalNative")
        dims = report["dimension_mods"]
        self.assertEqual(len(dims), 1)
        self.assertEqual(dims[0]["dimensions"], ["aetherlike:the_aether"])
        self.assertEqual(dims[0]["dimension_types"],
                         ["aetherlike:the_aether"])
        self.assertEqual(dims[0]["custom_chunk_generators"], [])
        self.assertEqual(dims[0]["ore_tag_blocks"],
                         ["aetherlike:ambrosium_ore"])
        self.assertEqual(dims[0]["chest_tag_blocks"],
                         ["aetherlike:skyroot_chest"])
        self.assertTrue(mc_marker.is_file(), "mc-spine stub never ran")
        self.assertFalse(neo_marker.exists(),
                         "NeoForge ran for a pure data-driven dimension mod")

    def test_custom_generator_dimension_mod_jar_delegates_tier3(self):
        """A dimension mod whose `dimension/` JSON names a modded Java
        ChunkGenerator is Tier 3 with CUSTOM_CHUNK_GENERATOR even when
        ShimCheck reports the jar COMPATIBLE — the bounded shim cannot host a
        custom ChunkGenerator."""
        deep = self.mods / "deepdim.jar"
        _jar(deep, "deepdim", entries=[
            ("data/deepdim/dimension_type/otherside.json", _dim_type_json()),
            ("data/deepdim/dimension/otherside.json",
             _dim_json("deepdim:echoing_void_gen", "deepdim:otherside")),
        ])
        shim = self.write_shim((deep, "COMPATIBLE", []))
        mc_marker = self.tmp / "mc.json"
        neo_marker = self.tmp / "neo.json"
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--neoforge-fallback-cmd", self.cmd(neo_marker),
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "DelegateToNeoForge")
        triggers = report["triggering_mods"]
        self.assertEqual([a["mod_id"] for a in triggers], ["deepdim"])
        self.assertEqual(triggers[0]["reasons"], ["CUSTOM_CHUNK_GENERATOR"])
        self.assertIn("deepdim -> CUSTOM_CHUNK_GENERATOR", proc.stderr)
        dims = report["dimension_mods"]
        self.assertEqual(len(dims), 1)
        self.assertEqual(dims[0]["custom_chunk_generators"],
                         ["deepdim:echoing_void_gen"])
        self.assertFalse(mc_marker.exists(), "mc-spine ran during delegation")
        self.assertTrue(neo_marker.is_file(), "NeoForge fallback never ran")

    def test_builtin_perf_modpack_launches_pure_native_zero_fallback(self):
        """Real modpack shape: content mods + sodium/lithium/modernfix whose
        ShimCheck-visible Mixins must be superseded — never Tier 3.

        Every policy launches pure native mc-spine; the NeoForge fallback
        command is never executed and the operator line names the 3 skipped
        mods.
        """
        create = self.mods / "create.jar"
        _jar(create, "create")
        aether = self.mods / "aether_datapack.zip"
        _data_zip(aether, [("data/aether/dimension/the_aether.json", "{}"),
                           ("pack.mcmeta", '{"pack":{"pack_format":48}}')])
        for mod_id in ("sodium", "lithium", "modernfix"):
            _jar(self.mods / f"{mod_id}.jar", mod_id)
        # Perf jars are skipped before ShimCheck, so the canned report only
        # covers create; its own (ignored) Mixin verdict proves the flagship
        # adapter path is unaffected by superseding.
        shim = self.write_shim(
            (create, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "c.Mix @Redirect",
               "Unsupported S0 injection")]))
        for policy in ("strict", "quarantine", "delegate"):
            mc_marker = self.tmp / f"mc-{policy}.json"
            neo_marker = self.tmp / f"neo-{policy}.json"
            proc, report = self.invoke(
                "--fallback", policy,
                "--shimcheck-json", str(shim),
                "--mc-spine-cmd", self.cmd(mc_marker),
                "--neoforge-fallback-cmd", self.cmd(neo_marker),
                "--world-namespaces", "create,minecraft",
                "--json")
            self.assertEqual(proc.returncode, 0, (policy, proc.stderr))
            self.assertEqual(report["plan"], "RunNative", policy)
            self.assertFalse(report["needs_bounded_jvm"], policy)
            self.assertEqual(report["superseded_perf_mods"],
                             ["lithium", "modernfix", "sodium"], policy)
            tiers = {a["mod_id"]: a["tier_name"]
                     for a in report["assignments"]}
            self.assertEqual(tiers, {
                "create": "NativeFlagship",
                "aether": "UniversalNative",
                "sodium": "BuiltInSuperseded",
                "lithium": "BuiltInSuperseded",
                "modernfix": "BuiltInSuperseded",
            }, policy)
            self.assertEqual(report["launch"]["kind"], "mc_spine", policy)
            self.assertTrue(mc_marker.is_file(),
                            f"mc-spine stub never ran under {policy}")
            marker = json.loads(mc_marker.read_text())
            self.assertEqual(marker["plan"], "RunNative")
            self.assertEqual(marker["superseded"],
                             "lithium,modernfix,sodium")
            self.assertEqual(marker["tier3"], "")
            self.assertFalse(neo_marker.exists(),
                             f"NeoForge ran under {policy} despite 0 Tier 3")
            self.assertIn(
                "Built-in optimizations replace 3 mod(s) "
                "(skipped automatically): lithium, modernfix, sodium",
                proc.stderr)
            # Perf jars carry no ShimCheck verdict and no rejection reasons:
            # the launcher never scanned them.
            for a in report["assignments"]:
                if a["tier_name"] == "BuiltInSuperseded":
                    self.assertEqual(
                        a["reasons"],
                        ["already covered by mc-spine built-in optimizations"])

    def test_quarantine_never_stages_superseded_perf_mods(self):
        """Quarantine staging copies active + tier3-adjacent data only —
        superseded perf jars are never copied into staging/mods."""
        create = self.mods / "create.jar"
        _jar(create, "create")
        sodium = self.mods / "sodium.jar"
        _jar(sodium, "sodium")
        bad = self.mods / "badmixin.jar"
        _jar(bad, "badmixin",
             entries=[("data/badmixin/recipes/thing.json",
                       '{"type":"minecraft:crafting_shaped"}')])
        shim = self.write_shim(
            (create, "COMPATIBLE", []),
            (bad, "REJECTED",
             [("UNSUPPORTED_MIXIN_INJECTION", "badmixin.Mix @Redirect",
               "Unsupported S0 injection")]))
        mc_marker = self.tmp / "mc.json"
        proc, report = self.invoke(
            "--fallback", "quarantine",
            "--shimcheck-json", str(shim),
            "--world-namespaces", "create,minecraft",
            "--quarantine-load-data",
            "--mc-spine-cmd", self.cmd(mc_marker),
            "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertEqual(report["superseded_perf_mods"], ["sodium"])
        self.assertEqual([a["mod_id"] for a in report["quarantined_mods"]],
                         ["badmixin"])
        staged = {p.name for p in (self.staging / "mods").iterdir()}
        self.assertEqual(staged, {"create.jar"})
        self.assertFalse((self.staging / "mods" / "sodium.jar").exists(),
                         "superseded perf jar was staged")
        marker = json.loads(mc_marker.read_text())
        self.assertEqual(marker["superseded"], "sodium")
        self.assertEqual(marker["quarantined"], "badmixin")
        self.assertIn(
            "Built-in optimizations replace 1 mod(s) "
            "(skipped automatically): sodium", proc.stderr)

    def test_neoforge_minecraft_dependency_range_gates_version(self):
        """Real neoforge.mods.toml extraction: the [[dependencies.<mod>]]
        entry with modId="minecraft" feeds the --mc-version gate.

        `jei` declares [1.20.1,1.20.2) -> Tier 3 MC_VERSION_INCOMPATIBLE with
        the plain-language message on both 1.21.1 and 1.21.4. `okmod` declares
        [1.21.1,1.21.5) -> accepted on BOTH targets and still Tier 2."""
        jei = self.mods / "jei.jar"
        _jar(jei, "jei", mc_range="[1.20.1,1.20.2)")
        okmod = self.mods / "okmod.jar"
        _jar(okmod, "okmod", mc_range="[1.21.1,1.21.5)")
        shim = self.write_shim(
            (jei, "COMPATIBLE", []),
            (okmod, "COMPATIBLE", []))
        for target in ("1.21.1", "1.21.4"):
            proc, report = self.invoke(
                "--mc-version", target,
                "--fallback", "delegate",
                "--shimcheck-json", str(shim),
                "--dry-run", "--json")
            self.assertEqual(proc.returncode, 0, (target, proc.stderr))
            self.assertEqual(report["mc_version"], target)
            self.assertEqual(report["plan"], "DelegateToNeoForge")
            triggers = report["triggering_mods"]
            self.assertEqual([a["mod_id"] for a in triggers], ["jei"], target)
            self.assertEqual(triggers[0]["reasons"], [
                "MC_VERSION_INCOMPATIBLE",
                f"built for Minecraft [1.20.1,1.20.2), "
                f"server is running {target}",
            ])
            by_id = {a["mod_id"]: a for a in report["assignments"]}
            self.assertEqual(by_id["jei"]["mc_version_range"],
                             "[1.20.1,1.20.2)")
            self.assertEqual(by_id["jei"]["mc_version_verdict"],
                             "major_mismatch")
            self.assertEqual(by_id["okmod"]["tier_name"], "BoundedJvmShim")
            self.assertEqual(by_id["okmod"]["mc_version_range"],
                             "[1.21.1,1.21.5)")
            self.assertEqual(by_id["okmod"]["mc_version_verdict"],
                             "compatible")

    def test_fabric_depends_minecraft_ranges(self):
        """fabric.mod.json `depends.minecraft` feeds the same gate: comparators,
        tilde, wildcards, and list-OR all parse; a malformed range fails
        closed as MC_VERSION_RANGE_INVALID instead of crashing."""
        jars = {}
        specs = {
            "fab_ok": ">=1.21.1 <1.21.5",
            "fab_tilde": "~1.21.1",
            "fab_wild": "1.21.x",
            "fab_list": ["1.20.x", "1.21.x"],
            "fab_bad": "bogus!!",
        }
        for mod_id, depends in specs.items():
            jar = self.mods / f"{mod_id}.jar"
            _fabric_jar(jar, mod_id, depends)
            jars[mod_id] = jar
        shim = self.write_shim(
            *[(jar, "COMPATIBLE", []) for jar in jars.values()])
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["mc_version"], "1.21.1")
        by_id = {a["mod_id"]: a for a in report["assignments"]}
        for mod_id in ("fab_ok", "fab_tilde", "fab_wild", "fab_list"):
            self.assertEqual(by_id[mod_id]["tier_name"], "BoundedJvmShim",
                             mod_id)
            self.assertEqual(by_id[mod_id]["mc_version_verdict"],
                             "compatible", mod_id)
        # Fabric list depends join as an OR union for the SemVer parser.
        self.assertEqual(by_id["fab_list"]["mc_version_range"],
                         "1.20.x || 1.21.x")
        # The malformed range fails closed as Tier 3, never a crash.
        self.assertEqual(by_id["fab_bad"]["tier_name"], "RequiresFallback")
        self.assertEqual(by_id["fab_bad"]["mc_version_verdict"],
                         "invalid_range")
        self.assertEqual(by_id["fab_bad"]["reasons"][0],
                         "MC_VERSION_RANGE_INVALID")
        self.assertIn("bogus!!", by_id["fab_bad"]["reasons"][1])
        self.assertEqual([a["mod_id"] for a in report["triggering_mods"]],
                         ["fab_bad"])

    def test_fabric_sodium_declaring_old_version_stays_superseded(self):
        """A sodium JAR whose fabric.mod.json declares only 1.20.x is STILL
        BuiltInSuperseded: the catalog check wins before the version gate,
        so no Tier-3 noise is produced for it."""
        sodium = self.mods / "sodium.jar"
        _fabric_jar(sodium, "sodium", "1.20.x")
        proc, report = self.invoke(
            "--fallback", "delegate",
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["plan"], "RunNative")
        self.assertEqual(report["superseded_perf_mods"], ["sodium"])
        self.assertEqual(report["triggering_mods"], [])
        assignment = report["assignments"][0]
        self.assertEqual(assignment["tier_name"], "BuiltInSuperseded")
        self.assertEqual(assignment["reasons"],
                         ["already covered by mc-spine built-in optimizations"])
        # The declared range is still recorded for audit, verdict uncomputed.
        self.assertEqual(assignment["mc_version_range"], "1.20.x")
        self.assertIsNone(assignment["mc_version_verdict"])

    def test_datapack_pack_format_gates_version(self):
        """pack.mcmeta pack_format/supported_formats are gated for pure-data
        packs: a 48+[48,61] pack loads on 1.21.1 and 1.21.4; a pack_format 26
        datapack directory reports PACK_FORMAT_INCOMPATIBLE on 1.21.1."""
        okpack = self.mods / "okpack.zip"
        _data_zip(okpack, [
            ("pack.mcmeta",
             '{"pack":{"pack_format":48,"supported_formats":[48,61]}}'),
            ("data/okpack/recipes/thing.json",
             '{"type":"minecraft:crafting_shaped"}'),
        ])
        oldpack = self.mods / "oldpack"
        (oldpack / "data" / "oldpack" / "recipes").mkdir(parents=True)
        (oldpack / "pack.mcmeta").write_text(
            '{"pack":{"pack_format":26}}', encoding="utf-8")
        (oldpack / "data" / "oldpack" / "recipes" / "thing.json").write_text(
            '{"type":"minecraft:crafting_shaped"}', encoding="utf-8")
        for target in ("1.21.1", "1.21.4"):
            proc, report = self.invoke(
                "--mc-version", target,
                "--fallback", "delegate",
                "--dry-run", "--json")
            self.assertEqual(proc.returncode, 0, (target, proc.stderr))
            self.assertEqual(report["plan"], "DelegateToNeoForge")
            by_id = {a["mod_id"]: a for a in report["assignments"]}
            # 48 in [48,61] covers both 1.21.1 (pf 48) and 1.21.4 (pf 61).
            self.assertEqual(by_id["okpack"]["tier_name"],
                             "UniversalNative", target)
            self.assertEqual(by_id["okpack"]["pack_format"], 48)
            # pack_format 26 never covers a 1.21.x target.
            self.assertEqual(by_id["oldpack"]["tier_name"],
                             "RequiresFallback", target)
            self.assertEqual(by_id["oldpack"]["reasons"][0],
                             "PACK_FORMAT_INCOMPATIBLE", target)
            self.assertIn("pack_format 26", by_id["oldpack"]["reasons"][1])
            self.assertIn(target, by_id["oldpack"]["reasons"][1])
            self.assertEqual([a["mod_id"] for a in report["triggering_mods"]],
                             ["oldpack"], target)

    def test_mc_version_26_3_gates_real_jar_ranges(self):
        """--mc-version 26.3: NeoForge [26.3,26.4) and Fabric >=26.3 jars
        classify cleanly, while a 1.21.1-only mod is refused as a different
        release line — never silently compatible."""
        oldmod = self.mods / "oldmod.jar"
        _jar(oldmod, "oldmod", mc_range="[1.21.1]")
        n26 = self.mods / "n26.jar"
        _jar(n26, "n26", mc_range="[26.3,26.4)")
        f26 = self.mods / "f26.jar"
        _fabric_jar(f26, "f26", ">=26.3")
        shim = self.write_shim(
            (oldmod, "COMPATIBLE", []),
            (n26, "COMPATIBLE", []),
            (f26, "COMPATIBLE", []))
        proc, report = self.invoke(
            "--mc-version", "26.3",
            "--fallback", "delegate",
            "--shimcheck-json", str(shim),
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["mc_version"], "26.3.0")
        self.assertEqual(report["plan"], "DelegateToNeoForge")
        triggers = report["triggering_mods"]
        self.assertEqual([a["mod_id"] for a in triggers], ["oldmod"])
        self.assertEqual(triggers[0]["reasons"], [
            "MC_VERSION_INCOMPATIBLE",
            "built for Minecraft [1.21.1], server is running 26.3.0",
        ])
        by_id = {a["mod_id"]: a for a in report["assignments"]}
        for mod_id in ("n26", "f26"):
            self.assertEqual(by_id[mod_id]["tier_name"], "BoundedJvmShim",
                             mod_id)
            self.assertEqual(by_id[mod_id]["mc_version_verdict"],
                             "compatible", mod_id)
        self.assertEqual(by_id["oldmod"]["mc_version_verdict"],
                         "major_mismatch")

    def test_mc_version_26_3_sodium_stays_builtin_superseded(self):
        """Even a sodium jar declaring a 1.21.1-only range is still
        BuiltInSuperseded under --mc-version 26.3: the catalog check wins
        before the version gate, so no Tier-3 fallback is triggered."""
        sodium = self.mods / "sodium.jar"
        _jar(sodium, "sodium", mc_range="[1.21.1]")
        shim = self.write_shim((sodium, "REJECTED",
            [("UNSUPPORTED_MIXIN_INJECTION", "sodium.Mix @Redirect",
              "Unsupported S0 injection")]))
        proc, report = self.invoke(
            "--mc-version", "26.3",
            "--fallback", "strict",
            "--shimcheck-json", str(shim),
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["mc_version"], "26.3.0")
        self.assertEqual(report["plan"], "RunNative")
        self.assertEqual(report["superseded_perf_mods"], ["sodium"])
        self.assertEqual(report["triggering_mods"], [])
        assignment = report["assignments"][0]
        self.assertEqual(assignment["tier_name"], "BuiltInSuperseded")
        self.assertEqual(assignment["reasons"],
                         ["already covered by mc-spine built-in optimizations"])
        self.assertEqual(assignment["mc_version_range"], "[1.21.1]")
        self.assertIsNone(assignment["mc_version_verdict"])

    def test_datapack_26_x_min_max_format_tuples_gate_version(self):
        """The 26.x pack.mcmeta schema — pack_format [major, minor] plus
        min_format/max_format tuples ([121, 0] or scalar 121) — is gated like
        the legacy scalar pack_format/supported_formats."""
        new26 = self.mods / "new26.zip"
        _data_zip(new26, [
            ("pack.mcmeta",
             '{"pack":{"pack_format":[121,0],'
             '"min_format":[121,0],"max_format":[125,0]}}'),
            ("data/new26/recipes/thing.json",
             '{"type":"minecraft:crafting_shaped"}'),
        ])
        # Scalar min_format/max_format spellings work too.
        scalar26 = self.mods / "scalar26.zip"
        _data_zip(scalar26, [
            ("pack.mcmeta",
             '{"pack":{"pack_format":[121,0],'
             '"min_format":121,"max_format":121}}'),
            ("data/scalar26/recipes/thing.json",
             '{"type":"minecraft:crafting_shaped"}'),
        ])
        # A legacy 1.21.1-only pack declares scalar pack_format 48.
        legacy = self.mods / "legacy.zip"
        _data_zip(legacy, [
            ("pack.mcmeta", '{"pack":{"pack_format":48}}'),
            ("data/legacy/recipes/thing.json",
             '{"type":"minecraft:crafting_shaped"}'),
        ])
        # On 26.3: the tuple packs load, the legacy pack is refused.
        proc, report = self.invoke(
            "--mc-version", "26.3",
            "--fallback", "delegate",
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        by_id = {a["mod_id"]: a for a in report["assignments"]}
        for mod_id in ("new26", "scalar26"):
            self.assertEqual(by_id[mod_id]["tier_name"], "UniversalNative",
                             mod_id)
            self.assertIsNone(by_id[mod_id]["pack_format"], mod_id)
            self.assertEqual(by_id[mod_id]["pack_min_format"], [121, 0],
                             mod_id)
        legacy_a = by_id["legacy"]
        self.assertEqual(legacy_a["tier_name"], "RequiresFallback")
        self.assertEqual(legacy_a["reasons"][0], "PACK_FORMAT_INCOMPATIBLE")
        self.assertIn("pack_format 48", legacy_a["reasons"][1])
        self.assertIn("26.3.0", legacy_a["reasons"][1])
        self.assertEqual([a["mod_id"] for a in report["triggering_mods"]],
                         ["legacy"])
        # On 1.21.1 the roles invert: 26.x-only tuple packs are refused.
        proc, report = self.invoke(
            "--mc-version", "1.21.1",
            "--fallback", "delegate",
            "--dry-run", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        by_id = {a["mod_id"]: a for a in report["assignments"]}
        self.assertEqual(by_id["legacy"]["tier_name"], "UniversalNative")
        for mod_id in ("new26", "scalar26"):
            a = by_id[mod_id]
            self.assertEqual(a["tier_name"], "RequiresFallback", mod_id)
            self.assertEqual(a["reasons"][0], "PACK_FORMAT_INCOMPATIBLE",
                             mod_id)
            self.assertIn("min_format", a["reasons"][1], mod_id)
        self.assertEqual({a["mod_id"] for a in report["triggering_mods"]},
                         {"new26", "scalar26"})

    def test_invalid_mc_version_flag_fails_closed(self):
        proc, _ = self.invoke("--mc-version", "bogus",
                              "--dry-run", "--json")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("invalid --mc-version", proc.stderr)

    def test_human_report_prints_tier_table(self):
        create = self.mods / "create.jar"
        _jar(create, "create")
        shim = self.write_shim((create, "COMPATIBLE", []))
        proc, _ = self.invoke(
            "--shimcheck-json", str(shim),
            "--mc-spine-cmd", self.cmd(self.tmp / "mc.json"))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Tier 0 NativeFlagship", proc.stdout)
        self.assertIn("create", proc.stdout)


# ---------------------------------------------------------------------------
# Modpack pre-flight (`--modpack`): ports of the #[cfg(test)] cases in
# mod-adapter-core/src/modpack.rs plus real .mrpack / CurseForge .zip CLI
# integration.
# ---------------------------------------------------------------------------

# Real-shaped 1.21.1 NeoForge Modrinth index: Create + Aether content mods,
# five redundant perf mods, one client-only mod, one datapack, and one
# unknown JAR (MODRINTH_1211 in modpack.rs).
MODRINTH_1211 = {
    "formatVersion": 1,
    "game": "minecraft",
    "versionId": "1.4.0",
    "name": "Create NeoForge Adventure",
    "summary": "Create, Aether, and a performance stack",
    "files": [
        {"path": "mods/create-1.21.1-6.0.6.jar",
         "hashes": {"sha1": "a" * 40, "sha512": "b"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/abc/create.jar"],
         "fileSize": 16000000},
        {"path": "mods/aether-1.21.1-1.5.9-neoforge.jar",
         "hashes": {"sha512": "c"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/def/aether.jar"],
         "fileSize": 42000000},
        {"path": "mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
         "hashes": {"sha512": "d"},
         "env": {"client": "required", "server": "optional"},
         "downloads": ["https://cdn.modrinth.com/data/ghi/sodium.jar"],
         "fileSize": 2000000},
        {"path": "mods/lithium-neoforge-0.15.0+mc1.21.1.jar",
         "hashes": {"sha512": "e"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/jkl/lithium.jar"],
         "fileSize": 1000000},
        {"path": "mods/modernfix-neoforge-5.20.2+mc1.21.1.jar",
         "hashes": {"sha512": "f"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/mno/modernfix.jar"],
         "fileSize": 3000000},
        {"path": "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
         "hashes": {"sha512": "g"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/pqr/dh.jar"],
         "fileSize": 25000000},
        {"path": "mods/ferritecore-7.0.2-neoforge.jar",
         "hashes": {"sha512": "h"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/stu/ferritecore.jar"],
         "fileSize": 300000},
        {"path": "mods/iris-neoforge-1.8.0+mc1.21.1.jar",
         "hashes": {"sha512": "i"},
         "env": {"client": "required", "server": "unsupported"},
         "downloads": ["https://cdn.modrinth.com/data/vwx/iris.jar"],
         "fileSize": 2500000},
        {"path": "datapacks/terralith-v2.5.8.zip",
         "hashes": {"sha512": "j"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/yz1/terralith.zip"],
         "fileSize": 800000},
        {"path": "mods/mysterious-factory-3.2.1.jar",
         "hashes": {"sha512": "k"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/234/mystery.jar"],
         "fileSize": 900000},
    ],
    "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.93"},
}

CURSEFORGE_1211 = {
    "minecraft": {
        "version": "1.21.1",
        "modLoaders": [
            {"id": "neoforge-21.1.93", "primary": True},
            {"id": "forge-52.0.0", "primary": False},
        ],
    },
    "manifestType": "minecraftModpack",
    "manifestVersion": 1,
    "name": "CF Adventure",
    "version": "7.1",
    "author": "someone",
    "files": [
        {"projectID": 123, "fileID": 456, "required": True,
         "slug": "create", "fileName": "create-1.21.1-6.0.6.jar"},
        {"projectID": 234, "fileID": 567, "required": True,
         "slug": "sodium",
         "fileName": "sodium-neoforge-0.6.13+mc1.21.1.jar"},
        {"projectID": 345, "fileID": 678, "required": True,
         "slug": "lithium"},
        {"projectID": 456, "fileID": 789, "required": False},
        {"projectID": 567, "fileID": 890, "required": True,
         "fileName": "mekanism-1.21.1-10.7.7.jar"},
    ],
    "overrides": "overrides",
}


def _modrinth_doc(**overrides):
    doc = json.loads(json.dumps(MODRINTH_1211))
    doc.update(overrides)
    return doc


def _mrpack(path, index, extra_members=()):
    """A real .mrpack zip: modrinth.index.json plus optional overrides."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as out:
        out.writestr("modrinth.index.json",
                     index if isinstance(index, str) else json.dumps(index))
        for name, payload in extra_members:
            out.writestr(name, payload)


def _curseforge_zip(path, manifest, extra_members=()):
    """A CurseForge pack zip: manifest.json plus optional overrides."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as out:
        out.writestr("manifest.json",
                     manifest if isinstance(manifest, str)
                     else json.dumps(manifest))
        for name, payload in extra_members:
            out.writestr(name, payload)


class ModpackPreflightUnitTests(unittest.TestCase):
    """Ports of the #[cfg(test)] cases in mod-adapter-core/src/modpack.rs."""

    def modrinth(self, doc=None):
        return mod_launcher.parse_modrinth_manifest(
            json.dumps(doc if doc is not None else MODRINTH_1211))

    def analyze(self, manifest, target=(1, 21, 1), flagship=(), profiles=()):
        return mod_launcher.analyze_modpack(
            manifest, target, set(flagship), set(profiles))

    def test_candidate_id_strips_loader_and_version_suffixes(self):
        cases = [
            ("mods/create-1.21.1-6.0.6.jar", "create"),
            ("mods/sodium-fabric-0.6.13+mc1.21.1.jar", "sodium"),
            ("mods/lithium-neoforge-0.15.0+mc1.21.1.jar", "lithium"),
            ("mods/modernfix-neoforge-5.20.2+mc1.21.1.jar", "modernfix"),
            ("mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
             "distanthorizons"),
            ("mods/ferritecore-7.0.2-neoforge.jar", "ferritecore"),
            ("mods/fabric-api-0.102.0+1.21.1.jar", "fabric-api"),
            ("mods/aether-1.21.1-1.5.9-neoforge.jar", "aether"),
            ("mods/jei-1.21.1-neoforge-19.21.0.247.jar", "jei"),
            ("mods/embeddium_extra-0.5.7+mc1.21.1.jar", "embeddium-extra"),
            ("mods/mekanism-1.21.1-10.7.7.jar", "mekanism"),
        ]
        infer = mod_launcher.infer_candidate_mod_id
        for path, expected in cases:
            self.assertEqual(infer(path), expected, path)
        self.assertIsNone(infer("mods/1.21.1.jar"))
        self.assertIsNone(infer("datapacks/x.zip"))

    def test_modrinth_manifest_parses_real_shape(self):
        m = self.modrinth()
        self.assertEqual(m.format, "modrinth")
        self.assertEqual(m.name, "Create NeoForge Adventure")
        self.assertEqual(m.version, "1.4.0")
        self.assertEqual(m.mc_version, "1.21.1")
        self.assertEqual(m.loaders, [
            {"id": "neoforge", "version": "21.1.93", "primary": True}])
        self.assertEqual(len(m.files), 10)
        self.assertEqual(m.files[0].candidate_mod_id, "create")
        self.assertEqual(m.files[2].candidate_mod_id, "sodium")
        self.assertEqual(m.files[5].candidate_mod_id, "distanthorizons")
        self.assertEqual(m.files[7].server_support, "unsupported")
        self.assertEqual(m.files[0].file_size, 16000000)
        self.assertEqual(m.files[0].sha1, "a" * 40)
        self.assertEqual(m.files[0].sha512, "b")

    def test_modrinth_preflight_classifies_every_entry(self):
        report = self.analyze(self.modrinth(), flagship=["create", "aether"])
        self.assertEqual(report.version_verdict.kind, "compatible")
        self.assertEqual(report.readiness, "needs_jar_inspection")
        self.assertEqual(len(report.entries), 10)
        class_of = {e.path: e.entry_class for e in report.entries}
        self.assertEqual(class_of["mods/create-1.21.1-6.0.6.jar"],
                         "native_flagship")
        self.assertEqual(class_of["mods/aether-1.21.1-1.5.9-neoforge.jar"],
                         "native_flagship")
        for perf in ("mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
                     "mods/lithium-neoforge-0.15.0+mc1.21.1.jar",
                     "mods/modernfix-neoforge-5.20.2+mc1.21.1.jar",
                     "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
                     "mods/ferritecore-7.0.2-neoforge.jar"):
            self.assertEqual(class_of[perf], "superseded_perf_mod", perf)
        self.assertEqual(class_of["mods/iris-neoforge-1.8.0+mc1.21.1.jar"],
                         "client_only_skipped")
        self.assertEqual(class_of["datapacks/terralith-v2.5.8.zip"],
                         "universal_native")
        self.assertEqual(class_of["mods/mysterious-factory-3.2.1.jar"],
                         "needs_inspection")
        self.assertEqual(report.skipped_perf_mod_names(), [
            "distanthorizons", "ferritecore", "lithium", "modernfix",
            "sodium"])
        self.assertEqual(report.client_only_names(), ["iris"])
        self.assertEqual(report.needs_inspection_names(),
                         ["mysterious-factory"])

    def test_modrinth_native_pack_reports_ready_with_auto_skips(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/create-1.21.1-6.0.6.jar",
             "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
            {"path": "mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
             "hashes": {"sha512": "y"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
        ])
        report = self.analyze(self.modrinth(doc), flagship=["create"])
        self.assertEqual(report.readiness, "ready_with_auto_skips")

    def test_modrinth_all_native_pack_reports_ready_native(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/create-1.21.1-6.0.6.jar",
             "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
            {"path": "datapacks/custom-recipes.zip",
             "hashes": {"sha512": "y"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
        ])
        report = self.analyze(self.modrinth(doc), flagship=["create"])
        self.assertEqual(report.readiness, "ready_native")

    def test_modrinth_26_3_pack_matches_26_3_target(self):
        doc = _modrinth_doc(
            dependencies={"minecraft": "26.3", "neoforge": "21.1.93"})
        doc["files"][0]["path"] = "mods/create-26.3-6.0.6.jar"
        m = self.modrinth(doc)
        self.assertEqual(m.mc_version, "26.3")
        report = self.analyze(
            m, target=(26, 3, 0), flagship=["create", "aether"])
        self.assertEqual(report.version_verdict.kind, "compatible")
        self.assertEqual(report.readiness, "needs_jar_inspection")

    def test_modrinth_1211_pack_mismatches_26_3_target(self):
        report = self.analyze(
            self.modrinth(), target=(26, 3, 0),
            flagship=["create", "aether"])
        self.assertEqual(report.version_verdict.kind, "major_mismatch")
        self.assertEqual(report.readiness, "version_mismatch")

    def test_profile_and_datapack_entries_classify_universal_native(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/jei-1.21.1-neoforge-19.21.0.247.jar",
             "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
            {"path": "datapacks/dungeon-pack.zip",
             "hashes": {"sha512": "y"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
        ])
        report = self.analyze(self.modrinth(doc), profiles=["jei"])
        self.assertEqual(report.entries[0].entry_class, "universal_native")
        self.assertEqual(report.entries[1].entry_class, "universal_native")
        self.assertEqual(report.readiness, "ready_native")

    def test_curseforge_manifest_parses_real_shape(self):
        m = mod_launcher.parse_curseforge_manifest(
            json.dumps(CURSEFORGE_1211))
        self.assertEqual(m.format, "curseforge")
        self.assertEqual(m.name, "CF Adventure")
        self.assertEqual(m.version, "7.1")
        self.assertEqual(m.mc_version, "1.21.1")
        self.assertEqual(len(m.loaders), 2)
        self.assertEqual(m.loaders[0]["id"], "neoforge")
        self.assertEqual(m.loaders[0]["version"], "21.1.93")
        self.assertTrue(m.loaders[0]["primary"])
        self.assertFalse(m.loaders[1]["primary"])
        self.assertEqual(len(m.files), 5)
        self.assertEqual(m.files[0].project_id, 123)
        self.assertEqual(m.files[0].candidate_mod_id, "create")
        self.assertFalse(m.files[3].required)
        self.assertIsNone(m.files[3].candidate_mod_id)
        self.assertEqual(m.files[4].candidate_mod_id, "mekanism")

    def test_curseforge_preflight_report(self):
        m = mod_launcher.parse_curseforge_manifest(
            json.dumps(CURSEFORGE_1211))
        report = self.analyze(m, flagship=["create"])
        self.assertEqual(report.version_verdict.kind, "compatible")
        self.assertEqual(report.skipped_perf_mod_names(),
                         ["lithium", "sodium"])
        self.assertEqual(report.native_names(), ["create"])
        self.assertEqual(report.needs_inspection_names(), [
            "curseforge-project-456-file-789.jar", "mekanism"])
        self.assertEqual(report.readiness, "needs_jar_inspection")

    def test_modrinth_rejects_path_traversal_fail_closed(self):
        for bad in ("../../etc/passwd", "mods/../../etc/passwd",
                    "/abs/path.jar", "//server/share.jar",
                    "mods\\..\\evil.jar", "mods\\evil.jar",
                    "C:\\evil.jar", "C:/evil.jar", "mods//dup.jar",
                    "mods/./weird.jar"):
            doc = _modrinth_doc(files=[
                {"path": bad, "hashes": {"sha512": "a"},
                 "downloads": [], "fileSize": 1}])
            with self.assertRaises(mod_launcher.ModpackError, msg=bad):
                self.modrinth(doc)

    def test_modrinth_rejects_nul_byte_in_path(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/evil\x00.jar", "hashes": {"sha512": "a"},
             "downloads": [], "fileSize": 1}])
        with self.assertRaises(mod_launcher.ModpackError):
            self.modrinth(doc)

    def test_modrinth_rejects_wrong_game_and_format(self):
        for bad in (
                {"formatVersion": 2, "game": "minecraft", "versionId": "1",
                 "name": "x", "files": [],
                 "dependencies": {"minecraft": "1.21.1"}},
                {"formatVersion": 1, "game": "terraria", "versionId": "1",
                 "name": "x", "files": [],
                 "dependencies": {"minecraft": "1.21.1"}},
                {"formatVersion": 1, "game": "minecraft", "versionId": "1",
                 "name": "x", "files": [], "dependencies": {}}):
            with self.assertRaises(mod_launcher.ModpackError, msg=bad):
                mod_launcher.parse_modrinth_manifest(json.dumps(bad))

    def test_malformed_json_fails_closed(self):
        for bad in ("", "not json", "{",
                    '{"formatVersion": 1, "game": "minecraft", '
                    '"versionId": "1", "name": "x", "files": [}',
                    "[]", "42"):
            with self.assertRaises(mod_launcher.ModpackError, msg=bad):
                mod_launcher.parse_modrinth_manifest(bad)
            with self.assertRaises(mod_launcher.ModpackError, msg=bad):
                mod_launcher.parse_curseforge_manifest(bad)

    def test_curseforge_rejects_wrong_manifest_type(self):
        base = {"manifestType": "minecraftModpack", "manifestVersion": 1,
                "name": "x", "version": "1",
                "minecraft": {"version": "1.21.1", "modLoaders": []},
                "files": []}
        for mutate in (
                lambda d: d.update(manifestVersion=2),
                lambda d: d.update(manifestType="other"),
                lambda d: d.update(files=[{
                    "projectID": "a", "fileID": 1, "required": True}])):
            doc = json.loads(json.dumps(base))
            mutate(doc)
            with self.assertRaises(mod_launcher.ModpackError, msg=doc):
                mod_launcher.parse_curseforge_manifest(json.dumps(doc))

    def test_curseforge_rejects_unsafe_slug_and_filename(self):
        base = {"manifestType": "minecraftModpack", "manifestVersion": 1,
                "name": "x", "version": "1",
                "minecraft": {"version": "1.21.1", "modLoaders": []}}
        for extra in ({"slug": "../evil"}, {"slug": "a/b"},
                      {"fileName": "..\\evil.jar"},
                      {"fileName": "sub/evil.jar"}):
            entry = {"projectID": 1, "fileID": 2, "required": True}
            entry.update(extra)
            doc = dict(base, files=[entry])
            with self.assertRaises(mod_launcher.ModpackError, msg=extra):
                mod_launcher.parse_curseforge_manifest(json.dumps(doc))

    def test_uppercase_jar_extension_still_needs_inspection(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/MYSTERY-1.0.JAR", "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1}])
        report = self.analyze(self.modrinth(doc))
        self.assertEqual(report.entries[0].entry_class, "needs_inspection")
        self.assertEqual(report.readiness, "needs_jar_inspection")

    def test_human_summary_is_plain_language_and_names_skipped_mods(self):
        report = self.analyze(self.modrinth(),
                              flagship=["create", "aether"])
        summary = report.format_human_summary()
        self.assertIn("Create NeoForge Adventure", summary)
        self.assertIn("Modrinth pack", summary)
        self.assertIn("matches this server", summary)
        for name in ("distanthorizons", "ferritecore", "lithium",
                     "modernfix", "sodium"):
            self.assertIn(name, summary)
        self.assertIn("built in", summary)
        self.assertIn("iris", summary)
        self.assertIn("aether", summary)
        self.assertIn("create", summary)
        self.assertIn("mysterious-factory", summary)
        self.assertIn("almost ready", summary)

    def test_human_summary_reports_version_mismatch_plainly(self):
        report = self.analyze(self.modrinth(), target=(26, 3, 0))
        summary = report.format_human_summary()
        self.assertIn("versions do not match", summary)
        self.assertIn("different Minecraft version", summary)


class ModpackPreflightCliTests(unittest.TestCase):
    """`--modpack` CLI end-to-end: real .mrpack / CurseForge archives."""

    @classmethod
    def setUpClass(cls):
        TMP_ROOT.mkdir(parents=True, exist_ok=True)

    def setUp(self):
        self.ws = tempfile.TemporaryDirectory(prefix="modpack-", dir=TMP_ROOT)
        self.tmp = Path(self.ws.name)
        self.env = dict(os.environ, TMPDIR=str(TMP_ROOT))

    def tearDown(self):
        self.ws.cleanup()

    def invoke(self, *argv):
        command = [sys.executable, str(TOOL), "--modpack"] + [
            str(a) for a in argv]
        proc = subprocess.run(command, cwd=ROOT, env=self.env,
                              capture_output=True, text=True)
        report = None
        if proc.stdout.strip() and proc.stdout.lstrip().startswith("{"):
            report = json.loads(proc.stdout)
        return proc, report

    def test_mrpack_zip_preflight_json_and_human(self):
        """A real .mrpack zip (1.21.1 NeoForge Create + perf mods + a
        client-only mod) reports JSON and human pre-flight, exit 0."""
        pack = self.tmp / "adventure.mrpack"
        _mrpack(pack, MODRINTH_1211,
                extra_members=[("overrides/options.txt", "fov:1")])
        proc, report = self.invoke(
            pack, "--format", "json", "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["schema"], "mc-spine-modpack-preflight/1")
        self.assertEqual(report["format"], "modrinth")
        self.assertEqual(report["pack_name"], "Create NeoForge Adventure")
        self.assertEqual(report["mc_version_declared"], "1.21.1")
        self.assertEqual(report["target"], "1.21.1")
        self.assertEqual(report["version_verdict"]["kind"], "compatible")
        self.assertEqual(report["readiness"], "needs_jar_inspection")
        class_of = {e["path"]: e["class"] for e in report["entries"]}
        for perf in ("mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
                     "mods/lithium-neoforge-0.15.0+mc1.21.1.jar",
                     "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar"):
            self.assertEqual(class_of[perf], "superseded_perf_mod", perf)
        self.assertEqual(class_of["mods/create-1.21.1-6.0.6.jar"],
                         "native_flagship")
        self.assertEqual(class_of["mods/iris-neoforge-1.8.0+mc1.21.1.jar"],
                         "client_only_skipped")
        self.assertEqual(class_of["datapacks/terralith-v2.5.8.zip"],
                         "universal_native")
        self.assertEqual(class_of["mods/mysterious-factory-3.2.1.jar"],
                         "needs_inspection")
        self.assertEqual(report["skipped_perf_mods"], [
            "distanthorizons", "ferritecore", "lithium", "modernfix",
            "sodium"])
        self.assertEqual(report["client_only_mods"], ["iris"])

        proc, _ = self.invoke(
            pack, "--format", "human", "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Create NeoForge Adventure", proc.stdout)
        self.assertIn("Modrinth pack", proc.stdout)
        self.assertIn("matches this server", proc.stdout)
        self.assertIn("built in", proc.stdout)
        for name in ("distanthorizons", "ferritecore", "lithium",
                     "modernfix", "sodium", "iris", "mysterious-factory"):
            self.assertIn(name, proc.stdout)
        self.assertIn("almost ready", proc.stdout)

    def test_standalone_modrinth_index_json_preflight(self):
        index = self.tmp / "modrinth.index.json"
        index.write_text(json.dumps(MODRINTH_1211), encoding="utf-8")
        proc, report = self.invoke(
            index, "--format", "json", "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["format"], "modrinth")
        self.assertEqual(report["readiness"], "needs_jar_inspection")

    def test_26_3_pack_ok_on_26_3_but_1211_pack_mismatches(self):
        pack26 = self.tmp / "pack26.mrpack"
        doc = _modrinth_doc(
            dependencies={"minecraft": "26.3", "neoforge": "21.1.93"})
        _mrpack(pack26, doc)
        proc, report = self.invoke(
            pack26, "--mc-version", "26.3", "--format", "json",
            "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["target"], "26.3.0")
        self.assertEqual(report["version_verdict"]["kind"], "compatible")
        self.assertEqual(report["readiness"], "needs_jar_inspection")

        pack1211 = self.tmp / "pack1211.mrpack"
        _mrpack(pack1211, MODRINTH_1211)
        proc, report = self.invoke(
            pack1211, "--mc-version", "26.3", "--format", "json",
            "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(report["version_verdict"]["kind"],
                         "major_mismatch")
        self.assertEqual(report["readiness"], "version_mismatch")
        proc, _ = self.invoke(pack1211, "--mc-version", "26.3")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("versions do not match", proc.stdout)
        self.assertIn("not compatible", proc.stdout)

    def test_curseforge_manifest_json_and_zip_preflight(self):
        manifest = self.tmp / "manifest.json"
        manifest.write_text(json.dumps(CURSEFORGE_1211), encoding="utf-8")
        for source in (manifest,):
            proc, report = self.invoke(
                source, "--format", "json", "--flagship-mods", "create")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertEqual(report["format"], "curseforge")
            self.assertEqual(report["readiness"], "needs_jar_inspection")
            self.assertEqual(report["skipped_perf_mods"],
                             ["lithium", "sodium"])
            self.assertEqual(report["needs_inspection_mods"], [
                "curseforge-project-456-file-789.jar", "mekanism"])
        pack = self.tmp / "cfpack.zip"
        _curseforge_zip(pack, CURSEFORGE_1211,
                        extra_members=[("overrides/config.cfg", "x=1")])
        proc, report = self.invoke(
            pack, "--format", "json", "--flagship-mods", "create")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["format"], "curseforge")
        self.assertEqual(report["pack_name"], "CF Adventure")
        self.assertEqual(report["loaders"][0],
                         {"id": "neoforge", "version": "21.1.93",
                          "primary": True})
        self.assertEqual(report["skipped_perf_mods"],
                         ["lithium", "sodium"])

    def test_fail_closed_traversal_nul_and_malformed_exit_1(self):
        for bad_path in ("../../etc/passwd", "C:\\evil.jar",
                         "mods/evil\x00.jar", "mods//dup.jar",
                         "mods/./weird.jar"):
            doc = _modrinth_doc(files=[
                {"path": bad_path, "hashes": {"sha512": "a"},
                 "downloads": [], "fileSize": 1}])
            pack = self.tmp / "evil.mrpack"
            _mrpack(pack, doc)
            proc, report = self.invoke(pack, "--format", "json")
            self.assertEqual(proc.returncode, 1, bad_path)
            self.assertIsNone(report)
            self.assertIn("modrinth.index.json", proc.stderr)
        # Malformed standalone JSON is a hard error, never a silent skip.
        broken = self.tmp / "modrinth.index.json"
        broken.write_text("{not json", encoding="utf-8")
        proc, _ = self.invoke(broken, "--format", "json")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("invalid JSON", proc.stderr)
        # A zip carrying neither manifest is equally rejected.
        empty_zip = self.tmp / "empty.zip"
        _data_zip(empty_zip, [("random.txt", "hi")])
        proc, _ = self.invoke(empty_zip, "--format", "json")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("neither", proc.stderr)
        # Missing pack path.
        proc, _ = self.invoke(self.tmp / "nope.mrpack", "--format", "json")
        self.assertEqual(proc.returncode, 1)

    def test_uppercase_jar_classifies_needs_inspection(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/MYSTERY-1.0.JAR", "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1}])
        pack = self.tmp / "upper.mrpack"
        _mrpack(pack, doc)
        proc, report = self.invoke(pack, "--format", "json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["entries"][0]["class"],
                         "needs_inspection")
        self.assertEqual(report["readiness"], "needs_jar_inspection")

    def test_profiles_dir_marks_profile_mods_universal(self):
        profiles = self.tmp / "profiles"
        profiles.mkdir()
        (profiles / "jei.json").write_text(json.dumps({
            "mod_id": "jei",
            "blocks": [{"id": "jei:gui", "role": "ui"}],
        }), encoding="utf-8")
        doc = _modrinth_doc(files=[
            {"path": "mods/jei-1.21.1-neoforge-19.21.0.247.jar",
             "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
            {"path": "mods/mystery-9.9.jar", "hashes": {"sha512": "y"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
        ])
        pack = self.tmp / "pack.mrpack"
        _mrpack(pack, doc)
        proc, report = self.invoke(
            pack, "--format", "json", "--profiles-dir", str(profiles))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        class_of = {e["path"]: e["class"] for e in report["entries"]}
        self.assertEqual(
            class_of["mods/jei-1.21.1-neoforge-19.21.0.247.jar"],
            "universal_native")
        self.assertEqual(class_of["mods/mystery-9.9.jar"],
                         "needs_inspection")

    def test_modpack_flagship_default_and_custom_ids(self):
        """--flagship-mods drives native_flagship; default is create."""
        pack = self.tmp / "pack.mrpack"
        _mrpack(pack, MODRINTH_1211)
        # Default --flagship-mods is just "create": aether lands in
        # needs_inspection until named.
        proc, report = self.invoke(pack, "--format", "json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        class_of = {e["path"]: e["class"] for e in report["entries"]}
        self.assertEqual(class_of["mods/create-1.21.1-6.0.6.jar"],
                         "native_flagship")
        self.assertEqual(class_of["mods/aether-1.21.1-1.5.9-neoforge.jar"],
                         "needs_inspection")
        proc, report = self.invoke(
            pack, "--format", "json", "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        class_of = {e["path"]: e["class"] for e in report["entries"]}
        self.assertEqual(class_of["mods/aether-1.21.1-1.5.9-neoforge.jar"],
                         "native_flagship")

    def test_modpack_json_flag_alias_and_mutual_exclusion(self):
        pack = self.tmp / "pack.mrpack"
        _mrpack(pack, MODRINTH_1211)
        proc, report = self.invoke(
            pack, "--json", "--flagship-mods", "create,aether")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["schema"], "mc-spine-modpack-preflight/1")
        # --mods-dir and --modpack are mutually exclusive.
        command = [sys.executable, str(TOOL),
                   "--mods-dir", str(self.tmp), "--modpack", str(pack)]
        proc = subprocess.run(command, cwd=ROOT, env=self.env,
                              capture_output=True, text=True)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("not allowed with argument", proc.stderr)

    def test_modpack_safe_pack_reports_ready_with_auto_skips(self):
        doc = _modrinth_doc(files=[
            {"path": "mods/create-1.21.1-6.0.6.jar",
             "hashes": {"sha512": "x"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
            {"path": "mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
             "hashes": {"sha512": "y"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
            {"path": "datapacks/custom-recipes.zip",
             "hashes": {"sha512": "z"},
             "env": {"client": "required", "server": "required"},
             "downloads": [], "fileSize": 1},
        ])
        pack = self.tmp / "safe.mrpack"
        _mrpack(pack, doc)
        proc, report = self.invoke(pack, "--format", "json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report["readiness"], "ready_with_auto_skips")
        proc, _ = self.invoke(pack)
        self.assertIn("ready — redundant and client-only files are "
                      "skipped automatically", proc.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
