#!/usr/bin/env python3
"""Offline structural and determinism tests for mod_compat_matrix.py."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "mod_compat_matrix.py"


def archive(path, entries):
    with zipfile.ZipFile(path, "w") as result:
        for name, payload in entries.items():
            result.writestr(name, payload)


class MatrixTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mod-matrix-test-")
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def run_tool(self, *args):
        return subprocess.run([sys.executable, str(TOOL), *map(str, args)], cwd=ROOT,
                              text=True, capture_output=True, check=False)

    def test_synthetic_loader_and_mixin_classification(self):
        archive(self.root / "fabric.jar", {
            "fabric.mod.json": b'{"id":"sample_fabric","version":"1.2"}',
            "data/sample/recipes/test.json": b"{}",
        })
        archive(self.root / "neo.jar", {
            "META-INF/neoforge.mods.toml": b'[[mods]]\nmodId="sample_neo"\nversion="2.0"\n',
        })
        archive(self.root / "mixin.jar", {
            "fabric.mod.json": b'{"id":"mixin_sample","version":"1"}',
            "sample.mixins.json": b"{}", "mod/Entry.class": b"Lorg/spongepowered/asm/mixin/Mixin;",
        })
        result = self.run_tool("--input-dir", self.root, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = {row["file"]: row for row in json.loads(result.stdout)["mods"]}
        self.assertEqual(rows["fabric.jar"]["loader"], "fabric")
        self.assertEqual(rows["neo.jar"]["loader"], "neoforge")
        self.assertEqual(rows["fabric.jar"]["structural_signals"]["datapack"]["recipes"], ["data/sample/recipes/test.json"])
        self.assertTrue(rows["mixin.jar"]["compatibility"].startswith("Blocked"))
        self.assertTrue(rows["mixin.jar"]["structural_signals"]["mixin_annotations"])

    def test_mmod_and_nested_jar_depth_and_perf_overlap(self):
        archive(self.root / "sodium.jar", {
            "fabric.mod.json": b'{"id":"sodium","version":"1"}',
            "culling/Foo.class": b"",
        })
        inner_bytes = self.root / "inner.jar"
        archive(inner_bytes, {"inner.class": b""})
        archive(self.root / "outer.jar", {"META-INF/jars/inner.jar": inner_bytes.read_bytes()})
        archive(self.root / "native.mmod", {
            "megu-mod.toml": b'[mod]\nid="native_mod"\nversion="1.0.0"\n',
            "megu-mod-bundle.json": b'{}',
        })
        result = self.run_tool("--input-dir", self.root)
        payload = json.loads(result.stdout)
        rows = {row["file"]: row for row in payload["mods"]}
        self.assertEqual(rows["native.mmod"]["compatibility"], "Full")
        self.assertEqual(rows["outer.jar"]["jar_in_jar_depth"], 1)
        self.assertEqual(rows["sodium.jar"]["structural_signals"]["built_in_perf_overlap"][0]["mod"], "sodium")

    def test_mrpack_preflight_and_formats_are_repeatable(self):
        archive(self.root / "perf.jar", {"fabric.mod.json": b'{"id":"lithium","version":"1"}'})
        archive(self.root / "dependent.jar", {
            "fabric.mod.json": b'{"id":"dependent","version":"1","depends":{"missing_lib":"*"}}',
            "code.class": b"",
        })
        pack = self.root / "pack.mrpack"
        archive(pack, {"modrinth.index.json": json.dumps({"files": [
            {"path": "mods/perf.jar", "downloads": []}, {"path": "mods/dependent.jar", "downloads": []},
            {"path": "mods/other.jar", "downloads": []},
        ]}).encode()})
        args = ("--input-dir", self.root, "--modpack", pack, "--json")
        first = self.run_tool(*args)
        second = self.run_tool(*args)
        self.assertEqual(first.stdout, second.stdout)
        plan = json.loads(first.stdout)["modpack_plan"]
        self.assertEqual(plan["recommended_skip"][0]["file"], "perf.jar")
        self.assertIn("other.jar", plan["install"])
        self.assertIn("missing_lib", plan["missing_dependencies"])
        self.assertEqual(plan["blockers"], ["dependent.jar"])
        md1 = self.run_tool("--input-dir", self.root, "--markdown")
        md2 = self.run_tool("--input-dir", self.root, "--markdown")
        self.assertEqual(md1.stdout, md2.stdout)
        self.assertIn("| Mod | Loader |", md1.stdout)

    def test_json_catalog_and_curseforge_manifest_inputs(self):
        archive(self.root / "sample.jar", {"fabric.mod.json": b'{"id":"sample","version":"1"}'})
        catalog = self.root / "catalog.json"
        catalog.write_text(json.dumps({"mods": [{"path": "sample.jar"}]}), encoding="utf-8")
        scanned = self.run_tool("--catalog", catalog, "--json")
        self.assertEqual(scanned.returncode, 0, scanned.stderr)
        self.assertEqual(json.loads(scanned.stdout)["mods"][0]["file"], "sample.jar")
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({"manifestType": "minecraftModpack", "files": [
            {"projectID": 44, "fileID": 55, "required": True},
        ]}), encoding="utf-8")
        planned = self.run_tool("--modpack", manifest, "--json")
        self.assertEqual(planned.returncode, 0, planned.stderr)
        self.assertEqual(json.loads(planned.stdout)["modpack_plan"]["install"], ["projectID=44,fileID=55"])

    def test_self_test_is_offline_and_passes(self):
        result = self.run_tool("--self-test")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PASS", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
