#!/usr/bin/env python3
"""Static checks for the platform GitHub Release workflow (stdlib only)."""

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/release.yml"
TOOLCHAIN = ROOT / "rust-toolchain.toml"
REQUIRED_JOBS = {"preflight", "build-matrix", "docker-check", "publish-release"}
# The full desktop+server lanes ship exactly these three end-user binaries.
BINARIES = ("megu", "megu-client-gpu", "megu-installer")
# Internal/companion tools stay source-buildable but must not be built,
# bundled or published by the release matrix.
DROPPED_BINARIES = (
    "megu-client", "megu-mod-check", "megu-world-migrate",
    "megu-init", "megu-admin",
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

    def test_platform_matrix_builds_every_binary(self):
        job = self.jobs["build-matrix"]
        self.assertRegex(job, r"(?m)^    needs: preflight\s*$")
        for runner, target in (
            ("ubuntu-22.04", "linux-x86_64"),
            ("ubuntu-22.04", "linux-x86_64-musl"),
            ("ubuntu-22.04-arm", "linux-arm64"),
            ("ubuntu-22.04-arm", "linux-arm64-musl"),
            ("ubuntu-22.04", "linux-x86_64-musl-client"),
            ("ubuntu-22.04-arm", "linux-arm64-musl-client"),
            ("ubuntu-22.04", "freebsd-x86_64"),
            ("ubuntu-22.04", "linux-riscv64"),
            ("macos-latest", "macos-arm64"),
            ("macos-latest", "macos-x86_64"),
            ("windows-latest", "windows-x86_64"),
            ("windows-11-arm", "windows-arm64"),
        ):
            self.assertIn(f"- os: {runner}\n            target: {target}", job)
        self.assertEqual(len(re.findall(r"(?m)^          - os:", job)), 12)
        for manifest in ("server-files/Cargo.toml", "client-gpu/Cargo.toml"):
            self.assertIn(manifest, job)
        # The dropped companion crates must not be built for release.
        for manifest in (
            "client-app/Cargo.toml", "mod-adapter-core/Cargo.toml",
            "state-store/Cargo.toml",
        ):
            self.assertNotIn(manifest, job)
        # Three full-lane builds (megu, megu-client-gpu, megu-installer)
        # plus the shared two-command server-only branch used by the musl,
        # FreeBSD and RISC-V lanes; the musl-client lanes build in Docker.
        self.assertEqual(job.count("cargo build --locked --release"), 5)
        self.assertNotIn("--bins", job)
        self.assertIn("--manifest-path server-files/Cargo.toml --bin megu-installer", job)
        self.assertIn("--manifest-path client-gpu/Cargo.toml --bin megu-client-gpu", job)
        self.assertIn("tools/release_bundle.py build", job)
        self.assertIn("tools/release_bundle.py verify", job)
        # Two full-lane extras (GPU client + installer) plus the single
        # server-only installer extra-binary shared by the
        # musl/FreeBSD/RISC-V lanes.
        self.assertEqual(job.count("--extra-binary"), 3)
        self.assertIn(
            '--extra-binary "server-files/target/$rel_dir/megu-installer$suffix"',
            job,
        )
        self.assertIn('$rel_dir', job)
        for binary in BINARIES:
            self.assertIn(f"/{binary}$suffix", job)
        for binary in DROPPED_BINARIES:
            self.assertNotIn(f"/{binary}$suffix", job)
        self.assertIn("if-no-files-found: error", job)
        self.assertIn(
            "cargo build --locked --release --features mod-suite,create-offload --bin megu",
            job,
        )

    def test_musl_static_targets_build_server_and_installer(self):
        job = self.jobs["build-matrix"]
        self.assertIn("rustup target add", job)
        self.assertIn("musl-tools", job)
        self.assertIn("musl-gcc", job)
        self.assertIn("linux-musl-gcc", job)
        self.assertIn("x86_64-unknown-linux-musl", job)
        self.assertIn("aarch64-unknown-linux-musl", job)
        # The musl toolchain step must not run on non-Ubuntu runners.
        self.assertIn("if: matrix.kind == 'musl'", job)
        self.assertIn("if: matrix.kind == 'cross'", job)
        self.assertIn("rustup target add", job)
        self.assertIn(
            'cargo build --locked --release --features mod-suite,create-offload'
            ' --bin megu --target "$RUST_TARGET"',
            job,
        )
        self.assertIn(
            'cargo build --locked --release --manifest-path server-files/Cargo.toml'
            ' --bin megu-installer --target "$RUST_TARGET"',
            job,
        )
        self.assertIn('server-files/target/$RUST_TARGET/release/megu-installer$suffix', job)
        self.assertIn('target/$RUST_TARGET/release/megu$suffix', job)
        # glibc runners moved to the 2.35 baseline for Debian 12+ compatibility.
        self.assertNotIn("ubuntu-latest\n            target: linux", job)
        self.assertNotIn("ubuntu-24.04-arm", job)

    def test_xplat_server_and_musl_client_lanes(self):
        job = self.jobs["build-matrix"]
        # Dynamic-musl GPU client: dedicated container step, never the host
        # musl-gcc path (that one must stay +crt-static for `FROM scratch`).
        self.assertIn("kind: musl-client", job)
        self.assertIn("if: matrix.kind == 'musl-client'", job)
        self.assertIn("-f packaging/docker/Dockerfile.alpine-client", job)
        self.assertIn('--build-arg RUST_TARGET="$RUST_TARGET"', job)
        self.assertIn("docker cp", job)
        self.assertIn('readelf -l dist-bin/megu-client-gpu | grep -q', job)
        # FreeBSD server: clang+lld against a checksum-pinned base.txz
        # sysroot — every input is first-party, no third-party VM action.
        self.assertIn("kind: bsd", job)
        self.assertIn("if: matrix.kind == 'bsd'", job)
        self.assertIn("x86_64-unknown-freebsd", job)
        self.assertIn("base.txz", job)
        self.assertIn("sha256sum -c -", job)
        self.assertIn(
            "CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER=clang", job)
        self.assertIn("CC_x86_64_unknown_freebsd=clang", job)
        self.assertIn("--sysroot=$FREEBSD_SYSROOT", job)
        # RISC-V 64 server: Ubuntu gcc-riscv64-linux-gnu cross toolchain.
        self.assertIn("kind: riscv", job)
        self.assertIn("if: matrix.kind == 'riscv'", job)
        self.assertIn("gcc-riscv64-linux-gnu", job)
        self.assertIn("riscv64gc-unknown-linux-gnu", job)
        self.assertIn(
            "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER="
            "riscv64-linux-gnu-gcc",
            job,
        )
        self.assertIn(
            "AR_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-ar", job)
        # All three server-only kinds share the musl two-binary build branch.
        self.assertIn(
            '"$MATRIX_KIND" == musl || "$MATRIX_KIND" == bsd'
            ' || "$MATRIX_KIND" == riscv',
            job,
        )

    def test_macos_deployment_target_and_extended_platforms(self):
        job = self.jobs["build-matrix"]
        # macOS binaries must target macOS 11 Big Sur or dyld refuses them
        # on anything older than the runner's SDK default.
        self.assertIn('MACOSX_DEPLOYMENT_TARGET: "11.0"', job)
        self.assertIn("x86_64-apple-darwin", job)
        self.assertIn("target: macos-x86_64", job)
        self.assertIn("target: windows-arm64", job)
        self.assertIn("windows-11-arm", job)
        # The cross-compiled x86_64 probe cannot run on an ARM64 runner.
        self.assertIn('RUNNER_ARCH" != "X64', job)
        publish = self.jobs["publish-release"]
        for target in ("macos-x86_64", "windows-arm64"):
            self.assertIn(target, publish)

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
            " --bin megu-client-gpu",
            job,
        )
        self.assertNotIn("cargo build --offline", job)
        self.assertIn(
            '"$probe" --headless-probe',
            job,
        )
        self.assertIn('if [[ -f "$probe" ]]', job)
        self.assertIn(
            '--extra-binary "client-gpu/target/$rel_dir/megu-client-gpu$suffix"',
            job,
        )
        self.assertIn("'megu-client-gpu'", self.jobs["publish-release"])

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
        self.assertIn(
            "targets = ('linux-x86_64', 'linux-x86_64-musl', 'linux-arm64',",
            job,
        )
        self.assertIn(
            "'linux-arm64-musl', 'linux-x86_64-musl-client',",
            job,
        )
        self.assertIn(
            "'linux-arm64-musl-client', 'freebsd-x86_64',",
            job,
        )
        self.assertIn(
            "'linux-riscv64', 'macos-arm64', 'macos-x86_64',",
            job,
        )
        self.assertIn(
            "'windows-x86_64', 'windows-arm64')",
            job,
        )
        for target in (
            "linux-x86_64", "linux-x86_64-musl", "linux-arm64", "linux-arm64-musl",
            "linux-x86_64-musl-client", "linux-arm64-musl-client",
            "freebsd-x86_64", "linux-riscv64",
            "macos-arm64", "macos-x86_64", "windows-x86_64", "windows-arm64",
        ):
            self.assertIn(f"'{target}'", job)
        self.assertIn(
            "desktop_binaries = ('megu', 'megu-client-gpu', 'megu-installer')",
            job,
        )
        self.assertIn("server_binaries = ('megu', 'megu-installer')", job)
        self.assertIn("client_binaries = ('megu-client-gpu',)", job)
        self.assertIn("'freebsd-x86_64', 'linux-riscv64'", job)
        self.assertIn("target.endswith('-musl-client')", job)
        self.assertIn("target.endswith('-musl')", job)
        self.assertIn("tools/release_bundle.py', 'verify'", job)
        # Every archive is still verified against its per-target
        # release-manifest.json, but only one aggregated manifest is
        # published — the twelve release-manifest-<target>.json files must
        # never reach dist/publish.
        self.assertIn("'release-manifest.json'", job)
        self.assertIn("manifests[target] = metadata", job)
        self.assertIn(
            'f\'megu-{os.environ["RELEASE_VERSION"]}-release-manifest.json\'',
            job,
        )
        self.assertNotIn("release-manifest-{target}", job)
        self.assertNotIn("release-manifest-{", job)
        self.assertIn("SHA256SUMS-all.txt", job)
        self.assertIn("--verify-tag", job)
        self.assertIn("--target", job)
        self.assertIn("--draft", job)
        self.assertIn("--prerelease", job)
        self.assertIn("gh release create", job)
        self.assertIn("GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}", job)


if __name__ == "__main__":
    unittest.main(verbosity=2)
