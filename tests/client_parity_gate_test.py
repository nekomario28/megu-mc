#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Self-test for tools/client_parity_gate.py using synthetic temp trees.

The gate's whole value is that it cannot be fooled: these tests pin the three
ways a feature could *look* implemented without being so — a comment, a string
literal, a ``use`` import, or a ``#[cfg(test)]`` item — plus the positive case
where a real call in a live consumer file flips a feature to WIRED_LIVE, and a
schema check against the real specs/client-1211-parity-manifest.json.

Everything runs against temporary directories; no Rust toolchain, network or
repo checkout state is touched.
"""
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve()
ROOT = HERE.parents[1]
GATE_PATH = ROOT / "tools/client_parity_gate.py"
REAL_MANIFEST = ROOT / "specs/client-1211-parity-manifest.json"

spec = importlib.util.spec_from_file_location("client_parity_gate", GATE_PATH)
gate = importlib.util.module_from_spec(spec)
sys.modules["client_parity_gate"] = gate
spec.loader.exec_module(gate)


def write_tree(root: Path, files: dict):
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def manifest_for(features):
    return {
        "manifest": "test-parity",
        "version": 1,
        "target": {"minecraft": "1.21.1", "protocol": 767},
        "categories": ["cat_a"],
        "features": features,
    }


def feature(fid, kfile, ksym, consumers, category="cat_a", **extra):
    entry = {
        "id": fid,
        "category": category,
        "description": f"test feature {fid}",
        "kernel_file": kfile,
        "kernel_symbol": ksym,
        "live_consumers": consumers,
    }
    entry.update(extra)
    return entry


def audit_tmp(root: Path, features):
    manifest_path = root / "specs/test-manifest.json"
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(json.dumps(manifest_for(features)))
    report, errors = gate.audit(root, manifest_path)
    assert not errors, errors
    return {f["id"]: f for f in report["features"]}, report


class CommentAndTestMentionsDoNotCount(unittest.TestCase):
    """(a) mentions in comments/strings/imports/cfg(test) never wire a feature."""

    def test_comment_string_import_and_cfg_test_are_not_wiring(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                # The kernel really declares the symbol...
                "kern/kernel.rs": "pub fn alpha_kernel() -> bool { true }\n",
                # ...but the only "consumer" mentions it in a comment, a
                # string literal, a `use` import and a #[cfg(test)] module.
                "live/consumer.rs": """
// alpha_kernel is mentioned here only in a comment
const DOC: &str = "alpha_kernel";
use other_crate::alpha_kernel;

pub fn run() {}

#[cfg(test)]
mod tests {
    #[test]
    fn t() { alpha_kernel(); }
}
""",
            })
            results, _ = audit_tmp(root, [
                feature("alpha", "kern/kernel.rs", "alpha_kernel",
                        ["live/consumer.rs"]),
            ])
            self.assertEqual(results["alpha"]["status"], "KERNEL_ONLY")
            self.assertEqual(results["alpha"]["live_refs"], [])

    def test_commented_out_kernel_symbol_is_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                # Symbol exists only inside a block comment and a string —
                # the kernel does NOT declare it.
                "kern/kernel.rs": """
/*
pub fn ghost_kernel() {}
*/
pub const LABEL: &str = "ghost_kernel";
""",
                "live/consumer.rs": "pub fn go() { ghost_kernel(); }\n",
            })
            results, _ = audit_tmp(root, [
                feature("ghost", "kern/kernel.rs", "ghost_kernel",
                        ["live/consumer.rs"]),
            ])
            # Even though the consumer really calls it, the kernel symbol
            # does not exist -> MISSING, never WIRED_LIVE.
            self.assertEqual(results["ghost"]["status"], "MISSING")
            self.assertFalse(results["ghost"]["kernel_exists"])

    def test_test_only_consumer_file_does_not_count(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                "kern/kernel.rs": "pub fn beta_kernel() {}\n",
                # A tests/-directory file is never a live consumer even when
                # it calls the symbol for real.
                "tests/kernel_test.rs": "fn t() { beta_kernel(); }\n",
            })
            results, _ = audit_tmp(root, [
                feature("beta", "kern/kernel.rs", "beta_kernel",
                        ["tests/kernel_test.rs"]),
            ])
            self.assertEqual(results["beta"]["status"], "KERNEL_ONLY")


class RealConsumerCallWiresLive(unittest.TestCase):
    """(b) a genuine reference in a listed live consumer -> WIRED_LIVE."""

    def test_plain_symbol_call_in_live_consumer(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                "kern/kernel.rs": "pub fn delta_kernel() -> u32 { 7 }\n",
                "live/session.rs": "fn pump() -> u32 { delta_kernel() }\n",
                "live/unused.rs": "fn idle() {}\n",
            })
            results, report = audit_tmp(root, [
                feature("delta", "kern/kernel.rs", "delta_kernel",
                        ["live/session.rs", "live/unused.rs"]),
            ])
            self.assertEqual(results["delta"]["status"], "WIRED_LIVE")
            self.assertEqual(results["delta"]["live_refs"], ["live/session.rs"])
            self.assertEqual(report["summary"]["wired_live"], 1)

    def test_type_member_symbol_needs_both_identifiers(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                "kern/kernel.rs": """
pub enum WireEvent {
    ChunkData,
    Other,
}
""",
                # Qualified variant use -> wired.
                "live/handler.rs": "fn h(e: WireEvent) -> bool {\n    matches!(e, WireEvent::ChunkData)\n}\n",
                # Mentions the member name but not the type -> not wired.
                "live/unrelated.rs": "fn u() { let _x = ChunkData; }\n",
            })
            results, _ = audit_tmp(root, [
                feature("chunk", "kern/kernel.rs", "WireEvent::ChunkData",
                        ["live/handler.rs", "live/unrelated.rs"]),
                # Same member name on an undeclared type -> MISSING.
                feature("ghost_member", "kern/kernel.rs",
                        "WireEvent::BossBar", ["live/handler.rs"]),
            ])
            self.assertEqual(results["chunk"]["status"], "WIRED_LIVE")
            self.assertEqual(results["chunk"]["live_refs"], ["live/handler.rs"])
            self.assertEqual(results["ghost_member"]["status"], "MISSING")


class StrictModes(unittest.TestCase):
    def test_strict_kernels_rejects_unplanned_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                "kern/kernel.rs": "pub fn present() {}\n",
            })
            features = [
                feature("ok", "kern/kernel.rs", "present", ["live/x.rs"],
                        status=None),
                feature("gap", "kern/kernel.rs", "absent_symbol",
                        ["live/x.rs"]),  # MISSING, no planned marker
                feature("planned_gap", "kern/kernel.rs", "also_absent",
                        ["live/x.rs"], status="planned"),
            ]
            features[0].pop("status", None)
            manifest_path = root / "specs/m.json"
            manifest_path.parent.mkdir(parents=True, exist_ok=True)
            manifest_path.write_text(json.dumps(manifest_for(features)))
            buf = io.StringIO()
            with redirect_stdout(buf):
                code = gate.main(["--root", str(root), "--manifest",
                                  str(manifest_path), "--strict-kernels"])
            self.assertEqual(code, 1)
            self.assertIn("gap", buf.getvalue())
            # With the unplanned gap marked planned, strict-kernels passes.
            features[1]["status"] = "planned"
            manifest_path.write_text(json.dumps(manifest_for(features)))
            with redirect_stdout(io.StringIO()):
                code = gate.main(["--root", str(root), "--manifest",
                                  str(manifest_path), "--strict-kernels"])
            self.assertEqual(code, 0)

    def test_strict_live_fails_unless_all_wired(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                "kern/kernel.rs": "pub fn wired_one() {}\npub fn idle_one() {}\n",
                "live/x.rs": "fn go() { wired_one(); }\n",
            })
            features = [
                feature("wired", "kern/kernel.rs", "wired_one", ["live/x.rs"]),
                feature("idle", "kern/kernel.rs", "idle_one", ["live/x.rs"]),
            ]
            manifest_path = root / "specs/m.json"
            manifest_path.parent.mkdir(parents=True, exist_ok=True)
            manifest_path.write_text(json.dumps(manifest_for(features)))
            with redirect_stdout(io.StringIO()):
                code = gate.main(["--root", str(root), "--manifest",
                                  str(manifest_path), "--strict-live"])
            self.assertEqual(code, 1)

    def test_json_mode_emits_parseable_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, {
                "kern/kernel.rs": "pub fn k() {}\n",
                "live/x.rs": "fn go() { k(); }\n",
            })
            manifest_path = root / "specs/m.json"
            manifest_path.parent.mkdir(parents=True, exist_ok=True)
            manifest_path.write_text(json.dumps(manifest_for([
                feature("k", "kern/kernel.rs", "k", ["live/x.rs"]),
            ])))
            buf = io.StringIO()
            with redirect_stdout(buf):
                code = gate.main(["--root", str(root), "--manifest",
                                  str(manifest_path), "--json"])
            self.assertEqual(code, 0)
            report = json.loads(buf.getvalue())
            self.assertEqual(report["summary"]["wired_live"], 1)
            self.assertEqual(report["features"][0]["status"], "WIRED_LIVE")


class RealManifestSchema(unittest.TestCase):
    """(c) the committed manifest is schema-valid and fully classifiable."""

    def test_real_manifest_schema_and_audit(self):
        self.assertTrue(REAL_MANIFEST.is_file(),
                        "specs/client-1211-parity-manifest.json is missing")
        doc = json.loads(REAL_MANIFEST.read_text())
        self.assertEqual(gate.validate_manifest(doc), [])

        report, errors = gate.audit(ROOT, REAL_MANIFEST)
        self.assertEqual(errors, [])
        self.assertIsNotNone(report)
        summary = report["summary"]
        self.assertEqual(summary["features_total"], len(doc["features"]))
        self.assertGreater(summary["features_total"], 200)
        # Every one of the 8 required categories is present and populated.
        self.assertEqual(len(report["categories"]), 8)
        for row in report["categories"]:
            self.assertGreater(row["total"], 0)
            self.assertEqual(
                row["wired_live"] + row["kernel_only"] + row["missing"],
                row["total"])
        for feature in report["features"]:
            self.assertIn(feature["status"], gate.STATUSES)
        # All catalogued ids are unique (schema check) and the f3/menu/entity
        # sweeps are complete: 15 chords, 25 menus, 130 entity ids + extras.
        ids = {f["id"] for f in doc["features"]}
        self.assertEqual(len(ids), len(doc["features"]))
        f3 = [f for f in doc["features"] if f["category"] == "f3_debug_chords"]
        menus = [f for f in doc["features"] if f["category"] == "menu_screens"]
        entities = [f for f in doc["features"]
                    if f["category"] == "entity_rig_coverage"]
        bers = [f for f in doc["features"]
                if f["category"] == "block_entity_renderers"]
        self.assertEqual(len(f3), 15)
        self.assertEqual(len(menus), 25)
        self.assertEqual(len(entities), 133)  # 130 ids + skin layers + .jem/.jpm
        self.assertEqual(len(bers), 12)
        # Every entity protocol id 0..=129 appears exactly once.
        entity_ids = sorted(
            int(f["id"].split(".")[1][:3]) for f in entities
            if f["id"].startswith("entity_rig.") and f["id"][11:14].isdigit())
        self.assertEqual(entity_ids, list(range(130)))
        menu_ids = sorted(int(f["id"].split(".")[1][:3]) for f in menus)
        self.assertEqual(menu_ids, list(range(25)))


if __name__ == "__main__":
    unittest.main(verbosity=2)
