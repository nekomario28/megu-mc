#!/usr/bin/env python3
"""Verify the shipped five-binary suite and its real documentation offline."""

import hashlib
import json
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RELEASE = ROOT / "tools/release_bundle.py"
BINARIES = (
    "mc-spine",
    "mc-spine-client",
    "mc-spine-mod-check",
    "mc-spine-world-migrate",
    "mc-spine-init",
)
DOCUMENTS = (
    "LICENSE",
    "COPYING.LESSER",
    "COPYING.AGPL",
    "NOTICE",
    "LICENSING.md",
    "THIRD_PARTY.md",
    "README.md",
    "docs/mod-author-guide.md",
)
TOOLS = (
    "tools/vanilla_data/gen_vanilla_data.sh",
    "tools/vanilla_data/version_pack.py",
    "tools/mod_launcher.py",
    "tools/megu_start.py",
    "tools/megu_translate.py",
    "tools/megu_mods.py",
)
VERSION = "0.1.0"
TARGET = "x86_64-unknown-linux-gnu"
GIT_SHA = "a" * 40
PREFIX = f"mc-spine-{VERSION}-{TARGET}"


def run(*args, expect=0):
    process = subprocess.run(
        [sys.executable, str(RELEASE), *map(str, args)],
        cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    assert process.returncode == expect, (
        f"release_bundle.py {args} exited {process.returncode}, "
        f"expected {expect}:\n{process.stdout}"
    )
    return process.stdout


def test_five_binary_bundle():
    with tempfile.TemporaryDirectory(prefix="release-bundle-test-") as tmp:
        work = Path(tmp)
        bin_dir = work / "bin"
        bin_dir.mkdir()
        binary_bytes = {}
        for name in BINARIES:
            payload = f"\x7fELF self-test {name}\n".encode()
            (bin_dir / name).write_bytes(payload)
            (bin_dir / name).chmod(0o755)
            binary_bytes[name] = payload

        def build(out, companions):
            argv = ["build", "--binary", bin_dir / BINARIES[0]]
            for name in companions:
                argv += ["--extra-binary", bin_dir / name]
            argv += ["--out-dir", out, "--version", VERSION,
                     "--target", TARGET, "--repo-root", ROOT,
                     "--git-sha", GIT_SHA]
            return run(*argv)

        first, second = work / "first", work / "second"
        build(first, BINARIES[1:])
        build(second, reversed(BINARIES[1:]))
        bundle_name = f"{PREFIX}.tar.gz"
        for name in (bundle_name, "release-manifest.json", "SHA256SUMS"):
            assert (first / name).read_bytes() == (second / name).read_bytes(), name

        bundle = first / bundle_name
        manifest_path = first / "release-manifest.json"
        run("verify", "--bundle", bundle, "--manifest", manifest_path,
            "--sums", first / "SHA256SUMS")

        manifest = json.loads(manifest_path.read_text())
        assert manifest["license"] == "GPL-3.0-or-later"
        assert manifest["subcrate_license"] == (
            "LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later"
        )
        expected = {f"{PREFIX}/{name}" for name in (*BINARIES, *DOCUMENTS, *TOOLS)}
        records = {item["path"]: item for item in manifest["files"]}
        assert set(records) == expected, sorted(set(records) ^ expected)

        with tarfile.open(bundle, "r:gz") as archive:
            members = archive.getmembers()
            assert {member.name for member in members} == expected
            assert [member.name for member in members] == sorted(expected)
            for member in members:
                assert member.isfile(), member.name
                assert member.mtime == member.uid == member.gid == 0, member.name
                payload = archive.extractfile(member).read()
                item = records[member.name]
                assert item["sha256"] == hashlib.sha256(payload).hexdigest()
                assert item["bytes"] == len(payload)
                relative = member.name[len(PREFIX) + 1:]
                assert "_1211" not in relative
                assert not relative.endswith((".jar", ".class"))
                if relative in BINARIES:
                    assert payload == binary_bytes[relative]
                    assert item["mode"] == "0755"
                elif relative in DOCUMENTS:
                    assert payload == (ROOT / relative).read_bytes(), relative
                    assert item["mode"] == "0644"
                else:
                    assert payload == (ROOT / relative).read_bytes(), relative
                    if relative in ("tools/megu_start.py", "tools/megu_translate.py",
                                    "tools/megu_mods.py"):
                        assert item["mode"] == "0755", relative
                        assert member.mode == 0o755, relative

        # The guard must reject a vendored file even when supplied as a companion.
        for name in ("mojang.jar", "foreign.class", "world_overlay.bin"):
            contaminated = work / name
            contaminated.write_bytes(b"proprietary test input")
            out = work / f"refused-{name}"
            argv = ["build", "--binary", bin_dir / BINARIES[0],
                    "--extra-binary", contaminated, "--out-dir", out,
                    "--version", VERSION, "--target", TARGET,
                    "--repo-root", ROOT, "--git-sha", GIT_SHA]
            assert "forbidden member" in run(*argv, expect=1), name
            assert not out.exists(), name


def test_optional_gpu_companion():
    with tempfile.TemporaryDirectory(prefix="release-gpu-test-") as tmp:
        work = Path(tmp)
        server = work / "mc-spine"
        server.write_bytes(b"\x7fELF fake server\n")
        for gpu_name in ("mc-spine-client-gpu", "mc-spine-client-gpu.exe"):
            gpu = work / gpu_name
            gpu_bytes = f"fake gpu companion {gpu_name}\n".encode()
            gpu.write_bytes(gpu_bytes)
            out = work / f"bundle-{gpu_name}"
            run("build", "--binary", server, "--extra-binary", gpu,
                "--out-dir", out, "--version", VERSION, "--target", TARGET,
                "--repo-root", ROOT, "--git-sha", GIT_SHA)

            bundle = out / f"{PREFIX}.tar.gz"
            manifest = out / "release-manifest.json"
            run("verify", "--bundle", bundle, "--manifest", manifest,
                "--sums", out / "SHA256SUMS")
            records = {item["path"]: item for item in json.loads(manifest.read_text())["files"]}
            assert records[f"{PREFIX}/{gpu_name}"]["mode"] == "0755"
            assert records[f"{PREFIX}/tools/megu_mods.py"]["mode"] == "0755"
            with tarfile.open(bundle, "r:gz") as archive:
                gpu_member = archive.getmember(f"{PREFIX}/{gpu_name}")
                assert gpu_member.mode == 0o755
                assert archive.extractfile(gpu_member).read() == gpu_bytes
                mods_member = archive.getmember(f"{PREFIX}/tools/megu_mods.py")
                assert mods_member.mode == 0o755 and mods_member.mtime == 0


def main():
    run("--self-test")
    test_five_binary_bundle()
    test_optional_gpu_companion()
    print("release bundle: self-test, five-binary suite, and optional GPU companions verified")


if __name__ == "__main__":
    main()
