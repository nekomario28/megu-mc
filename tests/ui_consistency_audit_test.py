#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Fixture-driven tests for tools/ui_consistency_audit.py."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
AUDIT_PATH = REPO_ROOT / "tools" / "ui_consistency_audit.py"
SPEC = importlib.util.spec_from_file_location("ui_consistency_audit", AUDIT_PATH)
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)

DRAW_OK = """\
use megu_core::ui_contract::VANILLA_1_21_1 as V;
pub mod palette {
    pub const PANEL: UiColor = UiColor::from_rgba8(V::PANEL_FACE);
}
"""

RETAINED_OK = """\
use megu_core::dep_graph::{IncrementalDag, NodeId};
use megu_core::rev::{DirtyLayers, RetainedLayerTracker, UiLayer, UiLayerBus};
pub struct UiRetainedCache { tracker: RetainedLayerTracker<8>, bus: UiLayerBus }
"""

VANILLA_WIDGETS_OK = """\
use megu_core::ui_contract::{audit_manifest, compute_gui_scale};
pub fn draw_mod_ui_manifest() {
    let scale = compute_gui_scale(1280, 720, 2);
    let _ = audit_manifest(1280, 720, scale);
}
"""

GPU_CONSUMER = "let mut cache = megu_ui::retained_cache::UiRetainedCache::new();"


class FixtureCase(unittest.TestCase):
    def audit_files(self, files):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for relative, text in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        return AUDIT.audit(root)

    def guard_rows(self, report, guard):
        return [row for row in report["findings"] if row["guard"] == guard]

    def assert_guard_fails(self, report, guard):
        self.assertTrue(
            self.guard_rows(report, guard),
            f"{guard} did not report the defective fixture: {report}",
        )

    def assert_guard_passes(self, report, guard):
        self.assertEqual(
            [],
            self.guard_rows(report, guard),
            f"{guard} reported a clean fixture: {report}",
        )


class LegacyPaletteGuardTest(FixtureCase):
    def test_blue_slate_color_fails(self):
        report = self.audit_files({
            "client-ui/src/draw.rs": "pub const BG: UiColor = rgb(24, 26, 32);\n",
        })
        self.assert_guard_fails(report, "legacy-blue-slate-palette")

    def test_blue_slate_with_whitespace_fails(self):
        report = self.audit_files({
            "client-ui/src/draw.rs": "const PANEL: UiColor = rgb(  84 ,96 , 130 );\n",
        })
        self.assert_guard_fails(report, "legacy-blue-slate-palette")

    def test_vanilla_token_passes(self):
        report = self.audit_files({
            "client-ui/src/draw.rs": DRAW_OK,
        })
        self.assert_guard_passes(report, "legacy-blue-slate-palette")
        self.assert_guard_passes(report, "vanilla-palette-derivation")

    def test_missing_draw_rs_fails(self):
        report = self.audit_files({})
        self.assert_guard_fails(report, "legacy-blue-slate-palette")


class PaletteDerivationGuardTest(FixtureCase):
    def test_no_contract_reference_fails(self):
        report = self.audit_files({
            "client-ui/src/draw.rs": "pub const PANEL: UiColor = rgb(1, 2, 3);\n",
        })
        self.assert_guard_fails(report, "vanilla-palette-derivation")


class RetainedWiringGuardTest(FixtureCase):
    def _files(self, retained=RETAINED_OK, gpu_consumer=GPU_CONSUMER):
        files = {"client-ui/src/retained_cache.rs": retained}
        if gpu_consumer is not None:
            files["client-gpu/src/window.rs"] = gpu_consumer
        return files

    def test_full_wiring_passes(self):
        report = self.audit_files(self._files())
        self.assert_guard_passes(report, "retained-cache-wiring")

    def test_missing_bus_fails(self):
        report = self.audit_files(
            self._files(retained=RETAINED_OK.replace("UiLayerBus", "UiBus"))
        )
        self.assert_guard_fails(report, "retained-cache-wiring")

    def test_unwired_cache_fails(self):
        report = self.audit_files(self._files(gpu_consumer=None))
        self.assert_guard_fails(report, "retained-cache-wiring")

    def test_missing_file_fails(self):
        report = self.audit_files({})
        self.assert_guard_fails(report, "retained-cache-wiring")


class ManifestPurityGuardTest(FixtureCase):
    def test_full_enforcement_passes(self):
        report = self.audit_files({
            "client-ui/src/vanilla_widgets.rs": VANILLA_WIDGETS_OK,
        })
        self.assert_guard_passes(report, "mod-manifest-purity")

    def test_missing_audit_fails(self):
        report = self.audit_files({
            "client-ui/src/vanilla_widgets.rs": VANILLA_WIDGETS_OK.replace(
                "audit_manifest", "check_manifest"
            ),
        })
        self.assert_guard_fails(report, "mod-manifest-purity")

    def test_missing_scale_fails(self):
        report = self.audit_files({
            "client-ui/src/vanilla_widgets.rs": VANILLA_WIDGETS_OK.replace(
                "compute_gui_scale", "scale_fn"
            ),
        })
        self.assert_guard_fails(report, "mod-manifest-purity")

    def test_missing_file_fails(self):
        report = self.audit_files({})
        self.assert_guard_fails(report, "mod-manifest-purity")


class LiveTreeTest(unittest.TestCase):
    """The real tree must be green — the audit exists to pin what shipped."""

    def test_repo_is_clean(self):
        report = AUDIT.audit(REPO_ROOT)
        self.assertTrue(report["ok"], f"live tree has findings: {report['findings']}")

    def test_strict_cli_passes(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT_PATH), "--strict"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr or result.stdout)


if __name__ == "__main__":
    unittest.main()
