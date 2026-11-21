#!/usr/bin/env python3
"""Tests for tools/modloader_parity_audit.py (megu-modloader-parity-audit/1).

Mode A runs against the real repository (offline, <5s). Mode B is exercised
with synthetic in-memory/temp JARs built via zipfile -- zero real vendor JARs:

  * client-only Fabric mod            -> CLIENT_ONLY_AUTO_SKIP
  * data-driven NeoForge 1.21.1 mod   -> NATIVE_DATA_DRIVEN + recipe-type and
    neoforge/data_maps gap aggregation
  * mixin-heavy NeoForge mod          -> JVM_BRIDGE_NEEDED with exact mixin
    target count and Jar-in-Jar count
  * native-library mod (.so)          -> UNSUPPORTED_NATIVE_LIB (fail-closed)
  * path-traversal malicious ZIP      -> rejected fail-closed
  * --min-overall-pct 101              -> exit 1 strict gate
"""

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import time
import unittest
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TOOL = REPO / "tools" / "modloader_parity_audit.py"

spec = importlib.util.spec_from_file_location("modloader_parity_audit", TOOL)
mpa = importlib.util.module_from_spec(spec)
sys.modules["modloader_parity_audit"] = mpa
spec.loader.exec_module(mpa)

_MODE_A_CACHE = None


def mode_a_cached():
    global _MODE_A_CACHE
    if _MODE_A_CACHE is None:
        _MODE_A_CACHE = mpa.mode_a(REPO)
    return _MODE_A_CACHE


def _make_jar(path, entries):
    """entries: list of (arcname, bytes|str)."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in entries:
            if isinstance(data, str):
                data = data.encode()
            zf.writestr(name, data)
    return path


NEOFORGE_MANIFEST = """modLoader="javafml"
loaderVersion="[1,*)"
license="MIT"
[[mods]]
modId="probe"
version="1.0.0"
"""

FABRIC_CLIENT_MANIFEST = json.dumps({
    "schemaVersion": 1,
    "id": "probeclient",
    "version": "1.0.0",
    "environment": "client",
    "entrypoints": {"client": ["com.probe.Client"]},
})

FABRIC_SERVER_MANIFEST = json.dumps({
    "schemaVersion": 1,
    "id": "probeserver",
    "version": "1.0.0",
    "environment": "*",
})

MIXIN_CONFIG = json.dumps({
    "required": True,
    "package": "com.probe.mixin",
    "compatibilityLevel": "JAVA_21",
    "mixins": ["AlphaMixin", "BetaMixin"],
    "client": ["GammaClientMixin"],
    "injectors": {"defaultRequire": 1},
})

JARJAR_METADATA = json.dumps({
    "jars": [{
        "path": "META-INF/jarjar/inner-1.0.jar",
        "identifier": {"group": "com.probe", "artifact": "inner",
                       "version": "1.0"},
        "isObfuscated": False,
    }]
})

ACCESS_TRANSFORMER = """# probe access transformer
public net.minecraft.world.level.Level f_46443_
public-f net.minecraft.world.level.Level f_46443_
"""


class ModeATest(unittest.TestCase):
    """Mode A against the real repository (offline, fast)."""

    @classmethod
    def setUpClass(cls):
        t0 = time.time()
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            cls.rc = mpa.main(["--repo", str(REPO), "--format", "json"])
        cls.elapsed = time.time() - t0
        cls.report = json.loads(buf.getvalue())

    def test_runs_under_5s_and_schema(self):
        self.assertEqual(self.rc, 0)
        self.assertLess(self.elapsed, 5.0)
        self.assertEqual(self.report["schema"],
                         "megu-modloader-parity-audit/1")

    def test_all_six_dimensions_nonzero_coverage(self):
        ma = self.report["mode_a"]
        self.assertEqual([d["id"] for d in ma["dimensions"]],
                         ["A1", "A2", "A3", "A4", "A5", "A6"])
        for dim in ma["dimensions"]:
            self.assertEqual(dim["total"], len(dim["items"]))
            self.assertGreater(dim["covered"], 0,
                               f"dimension {dim['id']} has zero coverage")
            for item in dim["items"]:
                self.assertIn(item["status"], ("COVERED", "GAP"))
                if item["status"] == "GAP":
                    self.assertEqual(item["evidence"], [])

    def test_parity_breakdown_and_gaps(self):
        parity = self.report["mode_a"]["parity"]
        for key in ("neoforge", "fabric", "overall"):
            self.assertIn(key, parity)
            self.assertGreater(parity[key]["covered"], 0)
            self.assertGreater(parity[key]["total"], 0)
            self.assertGreater(parity[key]["pct"], 0.0)
            self.assertLessEqual(parity[key]["pct"], 100.0)
        # gap entries must carry dimension + id + loaders
        for g in parity["gaps"]:
            self.assertIn("dimension", g)
            self.assertIn("id", g)
            self.assertIn("loaders", g)
        # a known-real gap: no c:ingots evidence in the Rust tree
        gap_ids = {g["id"] for g in parity["gaps"]}
        self.assertIn("c:ingots", gap_ids)

    def test_test_code_is_masked(self):
        # test_support/_tests files must not be scanned
        ma = self.report["mode_a"]
        for dim in ma["dimensions"]:
            for item in dim["items"]:
                for ev in item["evidence"]:
                    self.assertFalse(ev.endswith("_tests.rs"), ev)
                    self.assertNotIn("/tests/", ev)


class ModeBJarTest(unittest.TestCase):
    """Synthetic JARs: each loader-parity class must be classified."""

    def _audit(self, jar_path):
        return mpa.audit_jar(jar_path)

    def test_client_only_fabric_mod_auto_skips(self):
        with tempfile.TemporaryDirectory() as td:
            jar = _make_jar(Path(td) / "clientonly.jar", [
                ("fabric.mod.json", FABRIC_CLIENT_MANIFEST),
                ("com/probe/Client.class", b"\xca\xfe\xba\xbe"),
                ("data/probe/recipe/x.json",
                 json.dumps({"type": "minecraft:crafting_shaped"})),
            ])
            rep = self._audit(jar)
        self.assertEqual(rep["loader"], "fabric")
        self.assertEqual(rep["environment"], "client")
        self.assertTrue(rep["client_only"])
        self.assertEqual(rep["classification"], "CLIENT_ONLY_AUTO_SKIP")

    def test_data_driven_neoforge_mod_and_gap_aggregation(self):
        with tempfile.TemporaryDirectory() as td:
            jar = _make_jar(Path(td) / "datadriven.jar", [
                ("META-INF/neoforge.mods.toml", NEOFORGE_MANIFEST),
                ("pack.mcmeta", json.dumps(
                    {"pack": {"pack_format": 48, "description": "probe"}})),
                ("data/ex/recipe/hammer.json", json.dumps({
                    "type": "minecraft:crafting_shaped",
                    "pattern": ["##", "##"], "key": {},
                    "result": {"id": "ex:hammer"}})),
                ("data/ex/recipes/mix.json", json.dumps({
                    "type": "create:mixing", "ingredients": []})),
                ("data/ex/neoforge/data_maps/ex/map.json",
                 json.dumps({"values": {}})),
                ("data/ex/neoforge/biome_modifier/b.json",
                 json.dumps({"type": "neoforge:add_features"})),
                ("data/ex/tags/block/ores.json", json.dumps({"values": []})),
                ("data/ex/loot_table/blocks/x.json", json.dumps({})),
            ])
            rep = self._audit(jar)
        self.assertEqual(rep["loader"], "neoforge")
        self.assertEqual(rep["classification"], "NATIVE_DATA_DRIVEN")
        self.assertEqual(rep["counts"]["class_files"], 0)
        self.assertEqual(rep["counts"]["recipes"], 2)
        self.assertEqual(rep["recipe_types"].get("crafting_shaped"), 1)
        self.assertEqual(rep["recipe_types"].get("mixing"), 1)
        self.assertEqual(rep["counts"]["neoforge_data_maps"], 1)
        self.assertEqual(rep["counts"]["neoforge_biome_modifiers"], 1)
        self.assertEqual(rep["counts"]["tags"], 1)
        self.assertEqual(rep["counts"]["loot_tables"], 1)
        # unknown recipe type + neoforge-only dirs become missing primitives
        self.assertIn("recipe_type:mixing", rep["missing_primitives"])
        self.assertIn("neoforge/data_maps", rep["missing_primitives"])
        self.assertIn("neoforge/biome_modifier", rep["missing_primitives"])

    def test_mixin_heavy_neoforge_mod_needs_jvm_bridge(self):
        with tempfile.TemporaryDirectory() as td:
            jar = _make_jar(Path(td) / "mixindemo.jar", [
                ("META-INF/neoforge.mods.toml", NEOFORGE_MANIFEST),
                ("fabric.mod.json", FABRIC_SERVER_MANIFEST),  # -> dual
                ("com/probe/Main.class", b"\xca\xfe\xba\xbe"),
                ("com/probe/Other.class", b"\xca\xfe\xba\xbe"),
                ("probe.mixins.json", MIXIN_CONFIG),
                ("META-INF/accesstransformer.cfg", ACCESS_TRANSFORMER),
                ("META-INF/jarjar/metadata.json", JARJAR_METADATA),
                ("META-INF/jarjar/inner-1.0.jar", b"PK\x03\x04"),
                ("pack.mcmeta", "{}"),
            ])
            rep = self._audit(jar)
        self.assertEqual(rep["loader"], "dual")
        self.assertEqual(rep["classification"], "JVM_BRIDGE_NEEDED")
        # 2 mixins + 1 client = 3 exact targets
        self.assertEqual(rep["counts"]["mixin_configs"], 1)
        self.assertEqual(rep["counts"]["mixin_targets"], 3)
        self.assertEqual(len(rep["mixin_target_sample"]), 3)
        self.assertTrue(all("com.probe.mixin." in t
                            for t in rep["mixin_target_sample"]))
        # 2 real AT lines (comment stripped)
        self.assertEqual(rep["counts"]["accesstransformer_lines"], 2)
        # jarjar metadata lists 1 + nested jar entry = max(1,1) -> >=1
        self.assertGreaterEqual(rep["counts"]["jarjar_archives"], 1)
        self.assertEqual(rep["counts"]["class_files"], 2)
        for p in ("mixin_targets", "accesstransformer", "jar_in_jar"):
            self.assertIn(p, rep["missing_primitives"])

    def test_native_library_mod_fails_closed(self):
        with tempfile.TemporaryDirectory() as td:
            jar = _make_jar(Path(td) / "native.jar", [
                ("fabric.mod.json", FABRIC_SERVER_MANIFEST),
                ("com/probe/Native.class", b"\xca\xfe\xba\xbe"),
                ("lib/x86_64/libprobe.so", b"\x7fELF"),
            ])
            rep = self._audit(jar)
        self.assertEqual(rep["classification"], "UNSUPPORTED_NATIVE_LIB")
        self.assertEqual(rep["counts"]["native_libs"], 1)
        self.assertIn("native_lib", rep["missing_primitives"])

    def test_plain_class_mod_is_vm_or_primitive(self):
        with tempfile.TemporaryDirectory() as td:
            jar = _make_jar(Path(td) / "pureclass.jar", [
                ("META-INF/neoforge.mods.toml", NEOFORGE_MANIFEST),
                ("com/probe/Pure.class", b"\xca\xfe\xba\xbe"),
            ])
            rep = self._audit(jar)
        self.assertEqual(rep["loader"], "neoforge")
        self.assertEqual(rep["classification"], "NATIVE_PRIMITIVE_OR_VM")

    def test_path_traversal_zip_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            evil = Path(td) / "evil.jar"
            with zipfile.ZipFile(evil, "w") as zf:
                zf.writestr("fabric.mod.json", FABRIC_SERVER_MANIFEST)
                zf.writestr(zipfile.ZipInfo("../evil.class"),
                            b"\xca\xfe\xba\xbe")
            with self.assertRaises(mpa.JarIntegrityError):
                mpa.audit_jar(evil)
            # through main(): strict -> exit 1, non-strict -> exit 2
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                rc_strict = mpa.main(
                    ["--repo", str(REPO), "--jar", str(evil),
                     "--strict", "--format", "json"])
            self.assertEqual(rc_strict, 1)
            report = json.loads(buf.getvalue())
            self.assertEqual(report["mode_b"]["jars"][0]["classification"],
                             "REJECTED")
            self.assertTrue(report["mode_b"]["integrity_errors"])

            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc_plain = mpa.main(
                    ["--repo", str(REPO), "--jar", str(evil),
                     "--format", "json"])
            self.assertEqual(rc_plain, 2)

    def test_corrupt_zip_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            bad = Path(td) / "corrupt.jar"
            bad.write_bytes(b"this is not a zip file at all")
            with self.assertRaises(mpa.JarIntegrityError):
                mpa.audit_jar(bad)

    def test_mods_dir_aggregation(self):
        with tempfile.TemporaryDirectory() as td:
            mdir = Path(td) / "mods"
            mdir.mkdir()
            _make_jar(mdir / "a.jar", [
                ("fabric.mod.json", FABRIC_CLIENT_MANIFEST),
                ("com/a/A.class", b"\xca\xfe\xba\xbe")])
            _make_jar(mdir / "b.jar", [
                ("META-INF/neoforge.mods.toml", NEOFORGE_MANIFEST),
                ("data/ns/recipe/r.json",
                 json.dumps({"type": "thermal:centrifuge"}))])
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                rc = mpa.main(
                    ["--repo", str(REPO), "--mods-dir", str(mdir),
                     "--format", "json"])
        self.assertEqual(rc, 0)
        report = json.loads(buf.getvalue())
        mb = report["mode_b"]
        self.assertEqual(mb["jars_total"], 2)
        self.assertEqual(mb["by_classification"]["CLIENT_ONLY_AUTO_SKIP"], 1)
        self.assertEqual(mb["by_classification"]["NATIVE_DATA_DRIVEN"], 1)
        top = {t["primitive"]: t["jars"]
               for t in mb["top_missing_primitives"]}
        self.assertEqual(top.get("recipe_type:centrifuge"), 1)


class StrictGateTest(unittest.TestCase):
    def test_min_overall_pct_101_exits_1(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = mpa.main(["--repo", str(REPO), "--format", "json",
                           "--min-overall-pct", "101"])
        self.assertEqual(rc, 1)
        report = json.loads(buf.getvalue())
        self.assertLess(report["mode_a"]["parity"]["overall"]["pct"], 101)

    def test_min_overall_pct_0_exits_0(self):
        with tempfile.TemporaryDirectory() as td:
            # run from an empty-ish repo? mode_a needs repo; use real repo.
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                rc = mpa.main(["--repo", str(REPO), "--format", "json",
                               "--min-overall-pct", "0"])
        self.assertEqual(rc, 0)

    def test_missing_mods_dir_fails_closed(self):
        with tempfile.TemporaryDirectory() as td:
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = mpa.main(["--repo", str(REPO), "--format", "json",
                               "--mods-dir", str(Path(td) / "nope")])
        self.assertEqual(rc, 2)
        self.assertIn("modloader_parity_audit", err.getvalue())


if __name__ == "__main__":
    unittest.main(verbosity=2)
