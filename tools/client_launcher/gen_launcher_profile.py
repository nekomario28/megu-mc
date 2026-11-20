#!/usr/bin/env python3
"""Create a local Minecraft 1.21.1 version profile and reproducible bootstrapper jar."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parent
LIBRARY_NAME = "net.mcspine:bootstrapper:1.0.0"
LIBRARY_PATH = Path("net/mcspine/bootstrapper/1.0.0/bootstrapper-1.0.0.jar")
ZIP_TIME = (1980, 1, 1, 0, 0, 0)


def build_jar(out_dir):
    with tempfile.TemporaryDirectory(prefix="mc-spine-javac-") as build:
        compiled = Path(build)
        subprocess.run(["javac", f"-J-Djava.io.tmpdir={tempfile.gettempdir()}",
                        "--release", "21", "-d", str(compiled),
                        str(ROOT / "Main.java")], check=True)
        entries = [("META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\nMain-Class: net.mcspine.launcher.Main\r\n\r\n"),
                   ("integrated_bridge.py", (ROOT / "integrated_bridge.py").read_bytes())]
        entries.extend((str(path.relative_to(compiled)).replace("\\", "/"), path.read_bytes())
                       for path in sorted(compiled.rglob("*.class")))
        jar = out_dir / "mc-spine-bootstrapper.jar"
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, required=True,
                        help="Minecraft directory or staging root for versions/ and libraries/")
    parser.add_argument("--version-id", default="mc-spine-1.21.1")
    parser.add_argument("--client-bin", type=Path, help="native client executable to pin in the profile")
    parser.add_argument("--build-jar", action="store_true")
    args = parser.parse_args()
    if not args.version_id or args.version_id in (".", "..") or any(
            char in args.version_id for char in "/\\"):
        parser.error("--version-id must be a single safe directory name")
    out_dir = args.out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    if args.build_jar:
        jar = build_jar(out_dir)
        print(f"built {jar} ({jar.stat().st_size} bytes, sha256={hashlib.sha256(jar.read_bytes()).hexdigest()})")
    game_profile = {
        "id": args.version_id,
        "inheritsFrom": "1.21.1",
        "type": "release",
        "mainClass": "net.mcspine.launcher.Main",
        "libraries": [{"name": LIBRARY_NAME}],
    }
    if args.client_bin is not None:
        path = args.client_bin.resolve()
        if not path.is_file() or not path.stat().st_mode & 0o111:
            parser.error("--client-bin must point to an executable native client")
        game_profile["arguments"] = {"jvm": [f"-Dmc.spine.clientBin={path}"]}
    dest = out_dir / "versions" / args.version_id / (args.version_id + ".json")
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(json.dumps(game_profile, indent=2, ensure_ascii=False) + "\n")
    print(f"profile {dest}")


if __name__ == "__main__":
    main()
