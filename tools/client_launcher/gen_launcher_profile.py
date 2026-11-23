#!/usr/bin/env python3
"""Create a local Minecraft version profile and reproducible bootstrapper jar."""
import argparse
import hashlib
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parent
LIBRARY_NAME = "net.megu:bootstrapper:1.0.0"
LIBRARY_PATH = Path("net/megu/bootstrapper/1.0.0/bootstrapper-1.0.0.jar")
ZIP_TIME = (1980, 1, 1, 0, 0, 0)
MC_VERSIONS = ("1.21.1", "26.3")
WINDOWS_EXECUTABLE_SUFFIXES = (".exe", ".bat", ".cmd")


def iso_utc_now():
    """ISO-8601 UTC stamp; reproducible when SOURCE_DATE_EPOCH is pinned."""
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    stamp = (datetime.fromtimestamp(int(epoch), timezone.utc) if epoch
             else datetime.now(timezone.utc))
    return stamp.strftime("%Y-%m-%dT%H:%M:%S+00:00")


def build_jar(out_dir):
    with tempfile.TemporaryDirectory(prefix="megu-javac-") as build:
        compiled = Path(build)
        subprocess.run(["javac", f"-J-Djava.io.tmpdir={tempfile.gettempdir()}",
                        "--release", "21", "-d", str(compiled),
                        str(ROOT / "Main.java")], check=True)
        entries = [("META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\nMain-Class: net.megu.launcher.Main\r\n\r\n")]
        entries.extend((str(path.relative_to(compiled)).replace("\\", "/"), path.read_bytes())
                       for path in sorted(compiled.rglob("*.class")))
        jar = out_dir / "megu-bootstrapper.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            for name, data in entries:
                info = zipfile.ZipInfo(name, ZIP_TIME)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = 0o644 << 16
                archive.writestr(info, data, compresslevel=9)
        if jar.stat().st_size >= 32 * 1024:
            raise ValueError(f"bootstrapper jar exceeds 32 KiB: {jar.stat().st_size}")
        library = out_dir / "libraries" / LIBRARY_PATH
        library.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(jar, library)
        return jar


def detect_minecraft_dir():
    """Return the OS-standard .minecraft directory without touching the real home."""
    if sys.platform == "win32" or os.name == "nt":
        appdata = os.environ.get("APPDATA")
        if appdata:
            return Path(appdata) / ".minecraft"
        return Path.home() / "AppData" / "Roaming" / ".minecraft"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "minecraft"
    standard = Path.home() / ".minecraft"
    if not standard.is_dir():
        flatpak = Path.home() / ".var/app/com.mojang.Minecraft/.minecraft"
        if flatpak.is_dir():
            return flatpak
    return standard


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path,
                        help="Minecraft directory or staging root for versions/ and libraries/")
    parser.add_argument("--version-id", help="profile id (default: megu-<mc-version>)")
    parser.add_argument("--mc-version", choices=MC_VERSIONS, default="1.21.1",
                        help="Minecraft release the profile inheritsFrom (default: 1.21.1)")
    parser.add_argument("--detect-minecraft-dir", action="store_true",
                        help="print the OS-standard .minecraft directory and exit")
    parser.add_argument("--client-bin", type=Path, help="native client executable to pin in the profile")
    parser.add_argument("--client-sha256", help="verify native client hash before bootstrapper launch")
    parser.add_argument("--build-jar", action="store_true")
    args = parser.parse_args()
    if args.detect_minecraft_dir:
        print(detect_minecraft_dir())
        return
    if args.out_dir is None:
        parser.error("--out-dir is required unless --detect-minecraft-dir is used")
    args.version_id = args.version_id or f"megu-{args.mc_version}"
    if not args.version_id or args.version_id in (".", "..") or any(
            char in args.version_id for char in "/\\"):
        parser.error("--version-id must be a single safe directory name")
    out_dir = args.out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    if args.build_jar:
        jar = build_jar(out_dir)
        print(f"built {jar} ({jar.stat().st_size} bytes, sha256={hashlib.sha256(jar.read_bytes()).hexdigest()})")
    stamp = iso_utc_now()
    game_profile = {
        "id": args.version_id,
        "inheritsFrom": args.mc_version,
        "type": "release",
        "mainClass": "net.megu.launcher.Main",
        "releaseTime": stamp,
        "time": stamp,
        "libraries": [{"name": LIBRARY_NAME}],
    }
    if args.client_bin is not None:
        path = args.client_bin.resolve()
        # Windows executables cannot be exec-bit checked on a POSIX filesystem
        # (and st_mode & 0o111 is meaningless on Windows), so .exe/.bat/.cmd are
        # accepted by suffix alone; other binaries still require the exec bit.
        if not path.is_file() or (
                path.suffix.lower() not in WINDOWS_EXECUTABLE_SUFFIXES
                and not path.stat().st_mode & 0o111):
            parser.error("--client-bin must point to an executable native client")
        game_profile["arguments"] = {"jvm": [f"-Dmegu.clientBin={path}"]}
        if args.client_sha256 and hashlib.sha256(path.read_bytes()).hexdigest() != args.client_sha256.lower():
            parser.error("native client SHA-256 mismatch")
    if args.client_sha256:
        if len(args.client_sha256) != 64 or any(char not in "0123456789abcdefABCDEF"
                                                for char in args.client_sha256):
            parser.error("--client-sha256 must be 64 hexadecimal characters")
        game_profile.setdefault("arguments", {}).setdefault("jvm", []).append(
            f"-Dmegu.clientSha256={args.client_sha256.lower()}")
    dest = out_dir / "versions" / args.version_id / (args.version_id + ".json")
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(json.dumps(game_profile, indent=2, ensure_ascii=False) + "\n")
    print(f"profile {dest}")


if __name__ == "__main__":
    main()
