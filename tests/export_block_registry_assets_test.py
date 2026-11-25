#!/usr/bin/env python3
"""Java toolchain boundary for runtime block-registry asset generation."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import vanilla_java  # noqa: E402

spec = importlib.util.spec_from_file_location(
    "export_block_registry_assets",
    ROOT / "tools/vanilla_data/export_block_registry_assets.py")
exporter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(exporter)


class ExportJavaRequirement(unittest.TestCase):
    def test_jre_only_export_fails_before_reading_inputs_or_opening_runtime(self):
        def which(tool):
            return "/runtime/bin/java" if tool == "java" else None

        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "missing"
            with patch.object(vanilla_java.shutil, "which", side_effect=which), \
                    patch.object(exporter, "pinned_runtime") as runtime:
                with self.assertRaisesRegex(OSError, "JDK.*block registry asset generation"):
                    exporter.export_assets(missing, missing, missing)
        runtime.assert_not_called()

    def test_jdk_preflight_passes_through_to_input_validation(self):
        def which(tool):
            return f"/jdk/bin/{tool}" if tool in ("java", "javac") else None

        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "missing"
            with patch.object(vanilla_java.shutil, "which", side_effect=which):
                with self.assertRaises(FileNotFoundError):
                    exporter.export_assets(missing, missing, missing)


if __name__ == "__main__":
    unittest.main()
