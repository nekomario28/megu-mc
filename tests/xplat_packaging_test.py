#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Static checks for the cross-platform packaging surface (stdlib only).

Covers the BSD Vulkan unlock in client-gpu/Cargo.toml, the Alpine dynamic-musl
client image, the Alpine/FreeBSD/Void packaging recipes, and the consistency
between the release build matrix and the publish-verify target list.
"""

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/release.yml"
CLIENT_GPU_MANIFEST = ROOT / "client-gpu/Cargo.toml"
ALPINE_DOCKERFILE = ROOT / "packaging/docker/Dockerfile.alpine-client"
PACKAGING_FILES = ()
NEW_DOCS = ROOT / "docs/PLATFORMS.md"

UNIFIED_LICENSE = "LGPL-3.0-or-later"
# The retired triple expression must never come back on a header; built, not
# written, so this file does not trip tools/license_spdx_audit.py.
OBSOLETE_LICENSE = " OR ".join(
    ("LGPL-3.0-or-later", "GPL-3.0-or-later", "AGPL-3.0-or-later")
)

BSD_OSES = ("freebsd", "openbsd", "netbsd", "dragonfly")


def workflow_text():
    return WORKFLOW.read_text(encoding="utf-8")


def matrix_targets():
    return re.findall(
        r"(?m)^          - os: [\w.-]+\s*\n            target: ([\w-]+)",
        workflow_text(),
    )


class ClientGpuBsdVulkanTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.manifest = CLIENT_GPU_MANIFEST.read_text(encoding="utf-8")

    def test_vulkan_cfg_covers_linux_and_all_four_bsds(self):
        self.assertIn(
            "[target.'cfg(any(target_os = \"linux\", "
            'target_os = "freebsd", target_os = "openbsd", '
            'target_os = "netbsd", target_os = "dragonfly"))'
            "'.dependencies]",
            self.manifest,
        )
        for os_name in BSD_OSES:
            self.assertIn(f'target_os = "{os_name}"', self.manifest)
        # The BSD stanza must keep the Vulkan-only feature set (the base
        # wgpu dependency deliberately ships wgsl+std only).
        bsd_section = self.manifest.split(
            'target_os = "dragonfly"))\'.dependencies]', 1
        )[1]
        self.assertIn('features = ["vulkan"]', bsd_section)

    def test_windows_and_macos_stanzas_untouched(self):
        self.assertIn(
            "[target.'cfg(target_os = \"windows\")'.dependencies]",
            self.manifest,
        )
        self.assertIn('features = ["dx12", "vulkan"]', self.manifest)
        self.assertIn(
            "[target.'cfg(target_os = \"macos\")'.dependencies]",
            self.manifest,
        )
        self.assertIn('features = ["metal"]', self.manifest)
        self.assertIn('license = "LGPL-3.0-or-later"', self.manifest)


class AlpineClientImageTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.dockerfile = ALPINE_DOCKERFILE.read_text(encoding="utf-8")

    def test_pinned_alpine_base_and_toolchain(self):
        self.assertRegex(self.dockerfile, r"(?m)^FROM alpine:\$\{ALPINE_VERSION\}")
        self.assertIn('ARG ALPINE_VERSION=3.21', self.dockerfile)
        self.assertIn('ARG RUST_VERSION=1.98.0', self.dockerfile)
        self.assertIn('ARG RUST_TARGET=x86_64-unknown-linux-musl', self.dockerfile)

    def test_dynamic_musl_flag_and_driver_deps(self):
        # The whole point of the lane: -crt-static so the client can dlopen
        # Vulkan/Wayland/X11 from the host system.
        self.assertIn(
            'RUSTFLAGS="-C target-feature=-crt-static"', self.dockerfile)
        for dep in (
            "vulkan-loader-dev",
            "wayland-dev",
            "libxkbcommon-dev",
            "alsa-lib-dev",
            "pkgconf",
            "build-base",
        ):
            self.assertIn(dep, self.dockerfile)

    def test_rustup_init_is_checksum_verified(self):
        self.assertIn("rustup-init.sha256", self.dockerfile)
        self.assertIn("sha256sum -c -", self.dockerfile)
        self.assertIn("static.rust-lang.org", self.dockerfile)

    def test_build_is_locked_and_result_asserted_dynamic(self):
        self.assertIn("--locked", self.dockerfile)
        self.assertIn("--bin megu-client-gpu", self.dockerfile)
        self.assertIn('readelf -l /out/megu-client-gpu', self.dockerfile)
        self.assertIn("ld-musl", self.dockerfile)
        self.assertIn("NEEDED.*libc\\.musl", self.dockerfile)
        self.assertRegex(self.dockerfile, r"(?m)^FROM scratch AS export")

    def test_copies_every_path_dependency_the_client_needs(self):
        # client-kernel/build.rs reads the server crate's src/ tree.
        self.assertIn("COPY src ./src", self.dockerfile)
        for crate in (
            "core", "proto", "client-kernel", "client-port",
            "client-app", "client-ui", "client-gpu",
        ):
            with self.subTest(crate=crate):
                self.assertIn(f"COPY {crate}/src {crate}/src", self.dockerfile)
        self.assertIn("COPY client-kernel/build.rs", self.dockerfile)


class PackagingRecipesTest(unittest.TestCase):
    def test_every_recipe_exists_with_lgpl_header(self):
        for path in PACKAGING_FILES + (ALPINE_DOCKERFILE, NEW_DOCS):
            with self.subTest(path=path):
                self.assertTrue(path.is_file(), f"missing {path}")
                text = path.read_text(encoding="utf-8")
                self.assertIn(UNIFIED_LICENSE, text)
                self.assertNotIn(OBSOLETE_LICENSE, text)
                self.assertNotIn("SPDX-License-Identifier: LGPL-3.0-or-later OR", text)


class ReleaseMatrixConsistencyTest(unittest.TestCase):
    def test_matrix_targets_match_publish_verify_tuple(self):
        built = matrix_targets()
        publish = re.search(
            r"targets = \(([^)]*)\)",
            workflow_text().split("publish-release:", 1)[1],
        )
        self.assertIsNotNone(publish)
        published = set(re.findall(r"'([\w-]+)'", publish.group(1)))
        self.assertEqual(set(built), published)
        self.assertEqual(len(built), len(published))

    def test_every_target_name_matches_release_bundle_label(self):
        # tools/release_bundle.py::_NAME_RE gates artifact labels.
        name_re = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+~-]*$")
        for target in matrix_targets():
            with self.subTest(target=target):
                self.assertRegex(target, name_re)
                self.assertNotIn("..", target)

    def test_new_lane_kinds_are_wired_to_toolchain_steps(self):
        text = workflow_text()
        for kind in ("musl-client", "bsd", "riscv"):
            with self.subTest(kind=kind):
                self.assertIn(f"kind: {kind}", text)
                self.assertIn(f"if: matrix.kind == '{kind}'", text)
        # The musl-client lanes must never hit the +crt-static host path:
        # their step runs docker build, not cargo/musl-gcc on the runner.
        step = re.search(
            r"(?ms)^      - name: Build dynamic musl GPU client.*?(?=^      - )",
            text,
        )
        self.assertIsNotNone(step)
        block = step.group(0)
        self.assertIn("Dockerfile.alpine-client", block)
        self.assertNotIn("cargo build", block)
        self.assertNotIn("musl-tools", block)

    def test_platform_doc_lists_every_release_target(self):
        text = NEW_DOCS.read_text(encoding="utf-8")
        for target in matrix_targets():
            with self.subTest(target=target):
                self.assertIn(target, text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
