"""Tests for the cross-platform package metadata validator."""
import contextlib
import io
import json
import shutil
import tempfile
import unittest
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import check_pkg_manifests as checker


ROOT = Path(__file__).resolve().parents[1]


class PackageManifestTests(unittest.TestCase):
    def copied_tree(self):
        temp = tempfile.TemporaryDirectory()
        root = Path(temp.name)
        for rel in (*checker.FILES, "website/site-config.js", ".github/workflows/release.yml"):
            dest = root / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / rel, dest)
        self.addCleanup(temp.cleanup)
        return root

    def run_check(self, root):
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            result = checker.main(["--root", str(root)])
        return result, output.getvalue()

    def test_current_manifests_pass(self):
        self.assertEqual(checker.check(ROOT), [])

    def test_version_drift_fails(self):
        root = self.copied_tree()
        manifest = root / "packaging/scoop/megu.json"
        data = json.loads(manifest.read_text(encoding="utf-8"))
        data["version"] = "v9.9.9"
        manifest.write_text(json.dumps(data), encoding="utf-8")
        result, output = self.run_check(root)
        self.assertEqual(result, 1)
        self.assertIn("version is not synchronized", output)

    def test_http_release_url_fails(self):
        root = self.copied_tree()
        manifest = root / "packaging/winget/nekomario28.Megu.yaml"
        text = manifest.read_text(encoding="utf-8").replace(
            "https://github.com/nekomario28/megu-mc/releases/download/",
            "http://github.com/nekomario28/megu-mc/releases/download/", 1)
        manifest.write_text(text, encoding="utf-8")
        result, output = self.run_check(root)
        self.assertEqual(result, 1)
        self.assertIn("insecure HTTP URL", output)

    def test_non_repository_release_url_fails(self):
        root = self.copied_tree()
        manifest = root / "packaging/scoop/megu.json"
        text = manifest.read_text(encoding="utf-8").replace(
            "https://github.com/nekomario28/megu-mc/releases/download/",
            "https://example.invalid/releases/download/", 1)
        manifest.write_text(text, encoding="utf-8")
        result, output = self.run_check(root)
        self.assertEqual(result, 1)
        self.assertIn("untrusted external URL", output)

    def test_sync_version_updates_all_owned_manifests(self):
        root = self.copied_tree()
        tag = "v0.1.0-alpha.9"
        config = root / "website/site-config.js"
        text = config.read_text(encoding="utf-8").replace(
            'releaseTag: "v0.1.0-alpha.3"', f'releaseTag: "{tag}"')
        config.write_text(text, encoding="utf-8")
        result = checker.main(["--root", str(root), "--sync-version", tag])
        self.assertEqual(result, 0)
        self.assertEqual(checker.check(root), [])


if __name__ == "__main__":
    unittest.main()
