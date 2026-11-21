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


def _jar(path, mod_id, entries=()):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as out:
        out.writestr("META-INF/neoforge.mods.toml", MODS_TOML.format(mod_id=mod_id))
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
              reasons=(), has_custom_chunk_generator=False):
    mod = SimpleNamespace(
        mod_id=mod_id,
        mod_ids=mod_ids or [mod_id],
        has_java_bytecode=has_java_bytecode,
        shimcheck_compatible=shimcheck_compatible,
        has_unsupported_mixin_or_at=has_unsupported_mixin_or_at,
        has_custom_chunk_generator=has_custom_chunk_generator,
        source=f"mods/{mod_id}.jar",
    )
    mod.reason_codes = lambda: list(reasons)
    return mod


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


if __name__ == "__main__":
    unittest.main(verbosity=2)
