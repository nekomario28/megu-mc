#!/usr/bin/env python3
"""Static checks for the three-OS GitHub Release workflow (stdlib only)."""

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/release.yml"
TOOLCHAIN = ROOT / "rust-toolchain.toml"
REQUIRED_JOBS = {"preflight", "build-matrix", "docker-check", "publish-release"}
BINARIES = (
    "mc-spine", "mc-spine-client", "mc-spine-mod-check",
    "mc-spine-world-migrate", "mc-spine-init", "mc-spine-admin",
    "mc-spine-client-gpu",
)


def yaml_sections(contents, indent):
    """Read mapping sections at an exact indentation without a YAML dependency."""
    pattern = re.compile(rf"(?m)^{' ' * indent}([A-Za-z][\w-]*):(?:\s|$)")
    matches = list(pattern.finditer(contents))
    return {
        match.group(1): contents[match.end():
                                 matches[index + 1].start() if index + 1 < len(matches)
                                 else len(contents)]
        for index, match in enumerate(matches)
    }


class ReleaseWorkflowTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")
        cls.top = yaml_sections(cls.workflow, 0)
        cls.jobs = yaml_sections(cls.top["jobs"], 2)

    def test_required_structure_and_triggers(self):
        self.assertIn("on", self.top)
        self.assertIn("permissions", self.top)
        self.assertEqual(set(self.jobs), REQUIRED_JOBS)
        events = self.top["on"]
        self.assertRegex(events, r"(?m)^  push:\s*$")
        self.assertRegex(events, r"(?m)^    tags: \['v\*'\]\s*$")
        self.assertRegex(events, r"(?m)^  workflow_dispatch:\s*$")
        self.assertRegex(events, r"(?m)^      version:\s*$")
        self.assertRegex(events, r"(?m)^        required: true\s*$")
        self.assertRegex(events, r"(?m)^      draft:\s*$")
        self.assertRegex(events, r"(?m)^        default: true\s*$")

    def test_least_privilege_and_checkout_safety(self):
        self.assertRegex(self.top["permissions"], r"(?m)^  contents: read\s*$")
        for name, job in self.jobs.items():
            if name == "publish-release":
                self.assertRegex(job, r"(?m)^    permissions:\s*\n      contents: write\s*$")
            else:
                self.assertNotRegex(job, r"(?m)^    permissions:")
        checkout = re.findall(r"(?m)^      - uses: actions/checkout@[^\s#]+", self.workflow)
        protected = re.findall(
            r"(?m)^      - uses: actions/checkout@[^\s#]+[^\n]*\n"
            r"        with:\s*\n          persist-credentials: false\s*$",
            self.workflow,
        )
        self.assertEqual(len(checkout), 4)
        self.assertEqual(len(protected), len(checkout))

    def test_all_actions_are_pinned_to_full_commit(self):
        actions = re.findall(r"(?m)^\s+(?:-\s+)?uses:\s*([^\s#]+)", self.workflow)
        self.assertTrue(actions)
        for action in actions:
            with self.subTest(action=action):
                self.assertRegex(action, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}$")
        self.assertTrue(any(action.startswith("actions/upload-artifact@") for action in actions))
        self.assertTrue(any(action.startswith("actions/download-artifact@") for action in actions))

    def test_toolchain_matches_repo_pin(self):
        toolchain_match = re.search(
            r'(?m)^channel\s*=\s*"([^"]+)"', TOOLCHAIN.read_text(encoding="utf-8")
        )
        self.assertIsNotNone(toolchain_match)
        self.assertEqual(toolchain_match.group(1), "1.98.0")
        self.assertRegex(self.jobs["build-matrix"], r"(?m)^          toolchain: 1\.98\.0\s*$")

    def test_preflight_and_docker_gates(self):
        preflight = self.jobs["preflight"]
        for command in (
            "python3 tools/provenance_check.py",
            "python3 tools/provenance_check.py --self-test",
            "bash tools/debt_ratchet.sh",
            "python3 tools/vanilla_parity_audit.py --format text --strict --min-c2s-handled 58 --min-gamerules-wired 51",
            "python3 tools/modloader_parity_audit.py --format text --strict --min-overall-pct 100",
            "python3 website/check_site.py",
            "python3 tests/release_bundle_test.py",
        ):
            with self.subTest(command=command):
                self.assertIn(command, preflight)
        self.assertIn("python3 tests/release_workflow_test.py", preflight)
        self.assertIn("docker build -f docker/Dockerfile .", self.jobs["docker-check"])

    def test_three_os_matrix_builds_every_binary(self):
        job = self.jobs["build-matrix"]
        self.assertRegex(job, r"(?m)^    needs: preflight\s*$")
        for runner, target in (
            ("ubuntu-latest", "linux-x86_64"),
            ("macos-latest", "macos-arm64"),
            ("windows-latest", "windows-x86_64"),
        ):
            self.assertIn(f"- os: {runner}\n            target: {target}", job)
        self.assertEqual(len(re.findall(r"(?m)^          - os:", job)), 3)
        for manifest in (
            "client-app/Cargo.toml", "mod-adapter-core/Cargo.toml",
            "state-store/Cargo.toml", "server-files/Cargo.toml",
            "client-gpu/Cargo.toml",
        ):
            self.assertIn(manifest, job)
        self.assertEqual(job.count("cargo build --locked --release"), 6)
        self.assertIn("--manifest-path server-files/Cargo.toml --bins", job)
        self.assertIn("--manifest-path client-gpu/Cargo.toml --bin mc-spine-client-gpu", job)
        self.assertIn("tools/release_bundle.py build", job)
        self.assertIn("tools/release_bundle.py verify", job)
        self.assertEqual(job.count("--extra-binary"), 6)
        for binary in BINARIES:
            self.assertIn(f"release/{binary}$suffix", job)
        self.assertIn("if-no-files-found: error", job)
        self.assertIn(
            "cargo build --locked --release --features mod-suite,create-offload --bin mc-spine",
            job,
        )

    def test_publish_release_includes_bootstrapper_bundles_and_sbom(self):
        job = self.jobs["publish-release"]
        self.assertIn("tools/client_launcher/gen_launcher_profile.py", job)
        self.assertIn("--build-jar", job)
        self.assertIn("tools/client_launcher/export_launcher_bundle.py", job)
        self.assertIn("CycloneDX", job)
        self.assertIn("sbom.cdx.json", job)

    def test_client_gpu_headless_probe_smoke(self):
        job = self.jobs["build-matrix"]
        self.assertIn(
            "cargo build --locked --release --manifest-path client-gpu/Cargo.toml"
            " --bin mc-spine-client-gpu",
            job,
        )
        self.assertNotIn("cargo build --offline", job)
        self.assertIn(
            '"client-gpu/target/release/mc-spine-client-gpu$suffix" --headless-probe',
            job,
        )
        self.assertIn(
            '--extra-binary "client-gpu/target/release/mc-spine-client-gpu$suffix"',
            job,
        )
        self.assertIn("'mc-spine-client-gpu'", self.jobs["publish-release"])

    def test_workflow_yaml_parses(self):
        try:
            import yaml
        except ImportError:
            self.skipTest("PyYAML not installed; yaml_sections checks above stand in")
        document = yaml.safe_load(self.workflow)
        self.assertIsInstance(document, dict)
        self.assertEqual(set(document["jobs"]), REQUIRED_JOBS)

    def test_publishing_checksums_and_release_flags(self):
        job = self.jobs["publish-release"]
        self.assertIn("needs: [preflight, build-matrix, docker-check]", job)
        self.assertIn("pattern: release-*", job)
        self.assertIn("tools/release_bundle.py', 'verify'", job)
        self.assertIn("'release-manifest.json'", job)
        self.assertIn("SHA256SUMS-all.txt", job)
        self.assertIn("--verify-tag", job)
        self.assertIn("--target", job)
        self.assertIn("--draft", job)
        self.assertIn("--prerelease", job)
        self.assertIn("gh release create", job)
        self.assertIn("GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}", job)


if __name__ == "__main__":
    unittest.main(verbosity=2)
