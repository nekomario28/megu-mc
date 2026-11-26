#!/usr/bin/env python3
"""Architecture/libc/HTTP-tool selection checks for tools/install_linux.sh.

Runs the installer under a fully controlled PATH: fake `uname`, `ldd`, `curl`
and `wget` scripts plus symlinks to the real tools the script needs, so the
tests are deterministic on glibc, musl and minimal hosts alike (stdlib only).
"""

import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "tools" / "install_linux.sh"
VERSION = "v0.1.0-alpha.5"

# Real tools the script may invoke before it dies on the (failing) fake
# download; resolved from the usual binary directories at probe time.
REAL_TOOLS = (
    "sh", "env", "grep", "mktemp", "tar", "gzip", "awk", "sha256sum",
    "mkdir", "cp", "ln", "chmod", "rm", "rmdir", "cat", "tr", "sed",
    "dirname", "sleep", "test", "date",
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
if [ -n "${FIXTURE_DIR:-}" ] && [ -f "${FIXTURE_DIR}/${url##*/}" ]; then
    cp "${FIXTURE_DIR}/${url##*/}" "$out" && exit 0
fi
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
if [ -n "${FIXTURE_DIR:-}" ] && [ -f "${FIXTURE_DIR}/${url##*/}" ]; then
    cp "${FIXTURE_DIR}/${url##*/}" "$out" && exit 0
fi
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


def _write_release_fixture(fixture_dir, target, binaries):
    """Write a fake release under `fixture_dir`: a `megu-<ver>-<target>`
    bundle tarball containing `binaries` (name/body/mode triples), a fake
    bootstrapper JAR, and a SHA256SUMS-all.txt with *real* digests so the
    installer's `sha256sum -c` stage passes offline. Repeated calls (e.g.
    the split musl server + client halves) accumulate into one sums file.
    """
    bundle = f"megu-{VERSION}-{target}"
    staging = fixture_dir / "_staging" / bundle
    staging.mkdir(parents=True)
    for name, body, mode in binaries:
        path = staging / name
        path.write_text(body, encoding="utf-8")
        path.chmod(mode)
    archive = fixture_dir / f"{bundle}.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(staging, arcname=bundle)
    jar = fixture_dir / "megu-bootstrapper.jar"
    jar.write_bytes(b"fake-jar")

    def sha256(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    sums = fixture_dir / "SHA256SUMS-all.txt"
    entries = {}
    if sums.exists():
        for line in sums.read_text(encoding="utf-8").splitlines():
            digest, _, name = line.partition("  ")
            if name:
                entries[name] = digest
    entries[archive.name] = sha256(archive)
    entries[jar.name] = sha256(jar)
    sums.write_text(
        "".join(f"{digest}  {name}\n" for name, digest in entries.items()),
        encoding="utf-8",
    )


class InstallLinuxArchitectureTest(unittest.TestCase):
    def probe(
        self,
        uname_m,
        uname_s="Linux",
        libc="gnu",
        downloader="curl",
        megu_target=None,
        installer_args=("--binaries-only",),
        fixtures=None,
        work=None,
    ):
        """Run the installer; return (result, curl_requests, wget_requests).

        downloader: "curl" | "wget" | "both" | "none" controls which fake
        HTTP clients exist in the sandboxed PATH. `fixtures` points the
        fake clients at a prepared release directory; `work` keeps the
        sandbox dir alive for post-run filesystem assertions.
        """
        if work is None:
            with tempfile.TemporaryDirectory(prefix="megu-install-arch-") as tmp:
                return self._probe(
                    Path(tmp), uname_m, uname_s, libc, downloader,
                    megu_target, installer_args, fixtures,
                )
        return self._probe(
            Path(work), uname_m, uname_s, libc, downloader,
            megu_target, installer_args, fixtures,
        )

    def _probe(
        self,
        work,
        uname_m,
        uname_s,
        libc,
        downloader,
        megu_target,
        installer_args,
        fixtures,
    ):
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
        if fixtures is not None:
            env["FIXTURE_DIR"] = str(fixtures)
        result = subprocess.run(
            ["sh", str(INSTALLER), *installer_args],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=15,
        )
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

    def test_installer_links_only_the_three_slim_release_binaries(self):
        text = INSTALLER.read_text(encoding="utf-8")
        self.assertIn("for bin in megu megu-client-gpu megu-installer; do", text)
        for dropped in (
            "megu-client ",
            "megu-mod-check",
            "megu-world-migrate",
            "megu-init",
            "megu-admin",
        ):
            self.assertNotIn(dropped, text)

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

    def test_musl_split_archives_install_gpu_client_from_client_half(self):
        """The static musl server bundle ships no `megu-client-gpu`; the
        installer must download the matching `-client` half and supply the
        dynamic musl GPU client from it."""
        with tempfile.TemporaryDirectory(prefix="megu-install-musl-") as tmp:
            work = Path(tmp)
            fixtures = work / "fixtures"
            fixtures.mkdir()
            _write_release_fixture(
                fixtures,
                "linux-x86_64-musl",
                [
                    ("megu", "#!/bin/sh\nexit 0\n", 0o755),
                    ("megu-installer", "#!/bin/sh\nexit 0\n", 0o755),
                ],
            )
            _write_release_fixture(
                fixtures,
                "linux-x86_64-musl-client",
                [
                    ("megu-client-gpu", "#!/bin/sh\nexit 0\n", 0o755),
                ],
            )
            result, _, _ = self.probe(
                "x86_64",
                libc="musl",
                installer_args=("client",),
                fixtures=fixtures,
                work=work,
            )
            self.assertEqual(
                result.returncode, 0, result.stderr + result.stdout
            )
            self.assertNotIn("megu-client-gpu not included", result.stdout)
            self.assertIn("Complete!", result.stdout)
            mc_bin = work / "home" / ".minecraft" / "bin"
            self.assertTrue((mc_bin / "megu").is_file())
            self.assertTrue((mc_bin / "megu-client-gpu").is_file())
            self.assertTrue(
                os.access(mc_bin / "megu-client-gpu", os.X_OK)
            )

    def test_musl_client_archive_missing_gpu_client_fails_clearly(self):
        """When the `-client` half of a musl release lacks
        `megu-client-gpu`, the installer must die with a clear error
        instead of silently installing a clientless bundle."""
        with tempfile.TemporaryDirectory(prefix="megu-install-musl-") as tmp:
            work = Path(tmp)
            fixtures = work / "fixtures"
            fixtures.mkdir()
            _write_release_fixture(
                fixtures,
                "linux-x86_64-musl",
                [
                    ("megu", "#!/bin/sh\nexit 0\n", 0o755),
                    ("megu-installer", "#!/bin/sh\nexit 0\n", 0o755),
                ],
            )
            _write_release_fixture(
                fixtures,
                "linux-x86_64-musl-client",
                [
                    ("megu-readme", "client half without gpu binary\n", 0o644),
                ],
            )
            result, _, _ = self.probe(
                "x86_64",
                libc="musl",
                installer_args=("client",),
                fixtures=fixtures,
                work=work,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn(
                "expected dynamic musl client", result.stderr
            )

    def test_bundle_without_gpu_client_skips_copy_gracefully(self):
        """A bundle that ships no `megu-client-gpu` exercises the guarded
        copy: it must note the skip and finish instead of dying on `cp`,
        while the rest of the client install still lands."""
        with tempfile.TemporaryDirectory(prefix="megu-install-nogpu-") as tmp:
            work = Path(tmp)
            fixtures = work / "fixtures"
            fixtures.mkdir()
            _write_release_fixture(
                fixtures,
                "linux-x86_64",
                [
                    ("megu", "#!/bin/sh\nexit 0\n", 0o755),
                    ("megu-installer", "#!/bin/sh\nexit 0\n", 0o755),
                ],
            )
            result, _, _ = self.probe(
                "x86_64",
                libc="gnu",
                installer_args=("client",),
                fixtures=fixtures,
                work=work,
            )
            self.assertEqual(
                result.returncode, 0, result.stderr + result.stdout
            )
            self.assertIn("megu-client-gpu not included", result.stdout)
            self.assertIn("Complete!", result.stdout)
            mc_bin = work / "home" / ".minecraft" / "bin"
            self.assertTrue((mc_bin / "megu").is_file())
            self.assertFalse((mc_bin / "megu-client-gpu").exists())

    def test_glibc_bundle_installs_gpu_client_binary(self):
        """A bundle that does ship `megu-client-gpu` installs it into
        `~/.minecraft/bin` executable, with no skip note."""
        with tempfile.TemporaryDirectory(prefix="megu-install-glibc-") as tmp:
            work = Path(tmp)
            fixtures = work / "fixtures"
            fixtures.mkdir()
            _write_release_fixture(
                fixtures,
                "linux-x86_64",
                [
                    ("megu", "#!/bin/sh\nexit 0\n", 0o755),
                    ("megu-client-gpu", "#!/bin/sh\nexit 0\n", 0o755),
                    ("megu-installer", "#!/bin/sh\nexit 0\n", 0o755),
                ],
            )
            result, _, _ = self.probe(
                "x86_64",
                libc="gnu",
                installer_args=("client",),
                fixtures=fixtures,
                work=work,
            )
            self.assertEqual(
                result.returncode, 0, result.stderr + result.stdout
            )
            self.assertNotIn("megu-client-gpu not included", result.stdout)
            mc_bin = work / "home" / ".minecraft" / "bin"
            gpu_client = mc_bin / "megu-client-gpu"
            self.assertTrue(gpu_client.is_file())
            self.assertTrue(os.access(gpu_client, os.X_OK))
            self.assertTrue((mc_bin / "megu").is_file())


if __name__ == "__main__":
    unittest.main()
