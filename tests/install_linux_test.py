#!/usr/bin/env python3
"""Architecture selection checks for tools/install_linux.sh (stdlib only)."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "tools" / "install_linux.sh"
VERSION = "v0.1.0-alpha.1"


FAKE_UNAME = """#!/bin/sh
if [ "$1" = "-s" ]; then
    printf '%s\\n' "$FAKE_UNAME_S"
else
    printf '%s\\n' "$FAKE_UNAME_M"
fi
"""

FAKE_CURL = """#!/bin/sh
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            shift
            out="$1"
            ;;
        http://*|https://*)
            url="$1"
            ;;
    esac
    shift
done
printf '%s\\n' "$url" >> "$CURL_LOG"
case "$url" in
    *SHA256SUMS-all.txt)
        printf '0000000000000000000000000000000000000000000000000000000000000000  megu-bootstrapper.jar\\n' > "$out"
        ;;
    *)
        exit 1
        ;;
esac
"""


class InstallLinuxArchitectureTest(unittest.TestCase):
    def probe(self, uname_m, uname_s="Linux"):
        with tempfile.TemporaryDirectory(prefix="megu-install-arch-") as tmp:
            work = Path(tmp)
            bin_dir = work / "bin"
            bin_dir.mkdir()
            curl_log = work / "curl.log"
            for name, body in (("uname", FAKE_UNAME), ("curl", FAKE_CURL)):
                path = bin_dir / name
                path.write_text(body, encoding="utf-8")
                path.chmod(0o755)
            env = dict(os.environ)
            env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
            env["HOME"] = str(work / "home")
            env["CURL_LOG"] = str(curl_log)
            env["FAKE_UNAME_M"] = uname_m
            env["FAKE_UNAME_S"] = uname_s
            result = subprocess.run(
                ["sh", str(INSTALLER), "--binaries-only"],
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=15,
            )
            requests = curl_log.read_text(encoding="utf-8").splitlines() if curl_log.exists() else []
            return result, requests

    def test_supported_linux_architectures_select_release_assets(self):
        for uname_m, target in (
            ("x86_64", "linux-x86_64"),
            ("amd64", "linux-x86_64"),
            ("aarch64", "linux-arm64"),
            ("arm64", "linux-arm64"),
        ):
            with self.subTest(uname_m=uname_m):
                result, requests = self.probe(uname_m)
                archive = f"megu-{VERSION}-{target}.tar.gz"
                expected_url = (
                    "https://github.com/nekomario28/megu-mc/releases/download/"
                    f"{VERSION}/{archive}"
                )
                self.assertIn(f"({target})", result.stdout)
                self.assertIn(expected_url, requests)

    def test_unsupported_architecture_lists_supported_targets(self):
        result, requests = self.probe("riscv64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("unsupported Linux architecture: riscv64", result.stderr)
        for needle in (
            "x86_64", "amd64", "linux-x86_64",
            "aarch64", "arm64", "linux-arm64",
        ):
            self.assertIn(needle, result.stderr)
        self.assertEqual(requests, [])

    def test_non_linux_os_is_rejected(self):
        result, requests = self.probe("x86_64", uname_s="Darwin")
        self.assertEqual(result.returncode, 1)
        self.assertIn("this installer targets Linux", result.stderr)
        self.assertEqual(requests, [])


if __name__ == "__main__":
    unittest.main()
