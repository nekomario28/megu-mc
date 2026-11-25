# SPDX-License-Identifier: LGPL-3.0-or-later
"""Guard release binary Cargo feature sets against the CI test matrix.

The documented policy is: release `megu` binaries built from the root manifest
must have their exact feature set covered by a root-binary CI test command. The
separate GPU client and installer manifests are intentionally outside this
root-binary feature audit.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RELEASE = ROOT / ".github/workflows/release.yml"
CI = ROOT / ".github/workflows/ci.yml"
DOCUMENTED_RELEASE_FEATURE_SETS = {"create-offload,mod-suite"}


def cargo_feature_sets(text: str, *, build_only: bool = False) -> set[str]:
    """Return normalized comma-separated feature arguments from Cargo lines."""
    verbs = r"build" if build_only else r"(?:build|test|check)"
    found = set()
    for line in text.splitlines():
        if not re.search(rf"\bcargo\s+{verbs}\b", line):
            continue
        if "--bin megu" not in line:
            continue
        match = re.search(r"--features\s+([A-Za-z0-9_, -]+?)(?:\s+--|\s*$)", line)
        if match:
            found.add(",".join(sorted(part.strip() for part in match.group(1).split(","))))
    return found


class DistributionFeatureMatrixAudit(unittest.TestCase):
    def setUp(self) -> None:
        self.release = RELEASE.read_text(encoding="utf-8")
        self.ci = CI.read_text(encoding="utf-8")

    def test_release_feature_sets_are_documented(self) -> None:
        shipped = cargo_feature_sets(self.release, build_only=True)
        self.assertTrue(shipped, "release workflow has no root megu binary feature set")
        self.assertEqual(shipped, DOCUMENTED_RELEASE_FEATURE_SETS)

    def test_each_shipped_set_has_an_exact_ci_binary_test(self) -> None:
        release_sets = cargo_feature_sets(self.release, build_only=True)
        tested_sets = cargo_feature_sets(self.ci)
        self.assertTrue(release_sets <= tested_sets, f"untested release features: {release_sets - tested_sets}")

    def test_release_carries_release_profile_and_locked_dependencies(self) -> None:
        for _feature_set in DOCUMENTED_RELEASE_FEATURE_SETS:
            self.assertRegex(
                self.release,
                r"cargo build --locked --release --features\s+mod-suite,create-offload\s+--bin megu",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
