#!/usr/bin/env python3
"""Architecture/libc/HTTP-tool selection checks for tools/install_linux.sh.

Runs the installer under a fully controlled PATH: fake `uname`, `ldd`, `curl`
and `wget` scripts plus symlinks to the real tools the script needs, so the
tests are deterministic on glibc, musl and minimal hosts alike (stdlib only).
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "tools" / "install_linux.sh"
VERSION = "v0.1.0-alpha.3"

# Real tools the script may invoke before it dies on the (failing) fake
# download; resolved from the usual binary directories at probe time.
REAL_TOOLS = (
    "sh", "env", "grep", "mktemp", "tar", "sha256sum", "mkdir", "cp", "ln",
    "chmod", "rm", "rmdir", "cat", "tr", "sed", "dirname", "sleep", "test",
    "date",
)
TOOL_DIRS = ("/usr/bin", "/bin", "/usr/local/bin", "/sbin", "/usr/sbin")

FAKE_UNAME = """#!/bin/sh
if [ "$1" = "-s" ]; then
    printf '%s\\n' "$FAKE_UNAME_S"
else
    printf '%s\\n' "$FAKE_UNAME_M"
fi
"""

# `ldd --version` reports the libc family; the installer greps for "musl".
FAKE_LDD = """#!/bin/sh
printf '%s\\n' "ldd ($FAKE_LIBC_STRING) $FAKE_LIBC_VERSION"
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

# BusyBox-style wget: `wget -q -O <out> <url>`.
FAKE_WGET = """#!/bin/sh
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -q)
            ;;
        -O)
            shift
            out="$1"
            ;;
        http://*|https://*)
            url="$1"
            ;;
    esac
    shift
done
printf '%s\\n' "$url" >> "$WGET_LOG"
case "$url" in
    *SHA256SUMS-all.txt)
        printf '0000000000000000000000000000000000000000000000000000000000000000  megu-bootstrapper.jar\\n' > "$out"
        ;;
    *)
        exit 1
        ;;
esac
"""


def _link_real_tools(bin_dir):
    for tool in REAL_TOOLS:
        for directory in TOOL_DIRS:
            source = Path(directory) / tool
            if source.exists():
                os.symlink(source, bin_dir / tool)
                break


def _release_url(target):
    return (
        "https://github.com/nekomario28/megu-mc/releases/download/"
        f"{VERSION}/megu-{VERSION}-{target}.tar.gz"
    )


class InstallLinuxArchitectureTest(unittest.TestCase):
    def probe(
        self,
        uname_m,
        uname_s="Linux",
        libc="gnu",
        downloader="curl",
        megu_target=None,
    ):
        """Run the installer; return (result, curl_requests, wget_requests).

        downloader: "curl" | "wget" | "both" | "none" controls which fake
        HTTP clients exist in the sandboxed PATH.
        """
        with tempfile.TemporaryDirectory(prefix="megu-install-arch-") as tmp:
            work = Path(tmp)
            bin_dir = work / "bin"
            bin_dir.mkdir()
            for name, body in (("uname", FAKE_UNAME), ("ldd", FAKE_LDD)):
                path = bin_dir / name
                path.write_text(body, encoding="utf-8")
                path.chmod(0o755)
            if downloader in ("curl", "both"):
                path = bin_dir / "curl"
                path.write_text(FAKE_CURL, encoding="utf-8")
                path.chmod(0o755)
            if downloader in ("wget", "both"):
                path = bin_dir / "wget"
                path.write_text(FAKE_WGET, encoding="utf-8")
                path.chmod(0o755)
            _link_real_tools(bin_dir)

            curl_log = work / "curl.log"
            wget_log = work / "wget.log"
            env = {
                "PATH": str(bin_dir),
                "HOME": str(work / "home"),
                "CURL_LOG": str(curl_log),
                "WGET_LOG": str(wget_log),
                "FAKE_UNAME_M": uname_m,
                "FAKE_UNAME_S": uname_s,
                "FAKE_LIBC_STRING": "musl libc" if libc == "musl" else "GNU libc",
                "FAKE_LIBC_VERSION": "1.2.5" if libc == "musl" else "2.35",
            }
            if megu_target is not None:
                env["MEGU_TARGET"] = megu_target
            result = subprocess.run(
                ["sh", str(INSTALLER), "--binaries-only"],
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=15,
            )
            requests = []
            for log in (curl_log, wget_log):
                if log.exists():
                    requests.extend(log.read_text(encoding="utf-8").splitlines())
            curl_requests = (
                curl_log.read_text(encoding="utf-8").splitlines()
                if curl_log.exists()
                else []
            )
            wget_requests = (
                wget_log.read_text(encoding="utf-8").splitlines()
                if wget_log.exists()
                else []
            )
            return result, curl_requests, wget_requests

    def test_supported_linux_architectures_select_release_assets(self):
        for uname_m, libc, target in (
            ("x86_64", "gnu", "linux-x86_64"),
            ("amd64", "gnu", "linux-x86_64"),
            ("aarch64", "gnu", "linux-arm64"),
            ("arm64", "gnu", "linux-arm64"),
            ("x86_64", "musl", "linux-x86_64-musl"),
            ("amd64", "musl", "linux-x86_64-musl"),
            ("aarch64", "musl", "linux-arm64-musl"),
            ("arm64", "musl", "linux-arm64-musl"),
        ):
            with self.subTest(uname_m=uname_m, libc=libc):
                result, curl_requests, wget_requests = self.probe(
                    uname_m, libc=libc
                )
                self.assertIn(f"({target})", result.stdout)
                self.assertIn(_release_url(target), curl_requests + wget_requests)

    def test_wget_only_environment_downloads_release(self):
        result, curl_requests, wget_requests = self.probe(
            "x86_64", libc="musl", downloader="wget"
        )
        self.assertIn("(linux-x86_64-musl)", result.stdout)
        self.assertEqual(curl_requests, [])
        self.assertIn(_release_url("linux-x86_64-musl"), wget_requests)

    def test_curl_is_preferred_and_wget_is_the_fallback(self):
        result, curl_requests, wget_requests = self.probe(
            "aarch64", downloader="both"
        )
        self.assertIn("(linux-arm64)", result.stdout)
        sums_url = (
            "https://github.com/nekomario28/megu-mc/releases/download/"
            f"{VERSION}/SHA256SUMS-all.txt"
        )
        # The first (sums) download succeeds via curl, so wget never sees it;
        # the failing archive fetch falls back to wget.
        self.assertIn(sums_url, curl_requests)
        self.assertNotIn(sums_url, wget_requests)
        self.assertIn(_release_url("linux-arm64"), curl_requests)
        self.assertIn(_release_url("linux-arm64"), wget_requests)

    def test_missing_http_client_fails_with_actionable_error(self):
        result, curl_requests, wget_requests = self.probe(
            "x86_64", downloader="none"
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("neither curl nor wget", result.stderr)
        self.assertIn("apk add wget", result.stderr)
        self.assertEqual(curl_requests + wget_requests, [])

    def test_megu_target_override_selects_artifact(self):
        result, curl_requests, _wget_requests = self.probe(
            "x86_64", libc="gnu", megu_target="linux-x86_64-musl"
        )
        self.assertIn("(linux-x86_64-musl)", result.stdout)
        self.assertIn(_release_url("linux-x86_64-musl"), curl_requests)

    def test_unsupported_architecture_lists_supported_targets(self):
        result, curl_requests, wget_requests = self.probe("riscv64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("unsupported Linux architecture: riscv64", result.stderr)
        for needle in (
            "x86_64", "amd64", "linux-x86_64", "linux-x86_64-musl",
            "aarch64", "arm64", "linux-arm64", "linux-arm64-musl",
            "MEGU_TARGET",
        ):
            self.assertIn(needle, result.stderr)
        self.assertEqual(curl_requests + wget_requests, [])

    def test_non_linux_os_is_rejected(self):
        result, curl_requests, wget_requests = self.probe("x86_64", uname_s="Darwin")
        self.assertEqual(result.returncode, 1)
        self.assertIn("this installer targets Linux", result.stderr)
        self.assertEqual(curl_requests + wget_requests, [])


if __name__ == "__main__":
    unittest.main()
