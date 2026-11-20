#!/usr/bin/env python3
"""Build deterministic, clean-room Fabric/NeoForge Java 21 integrated bridge JAR.

Only the bridge and loader adapters are shipped. Tiny compile-time interface
stubs are discarded and never included in the output archive.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import zipfile


HERE = Path(__file__).resolve().parent
JAVA_PACKAGE = HERE / "net/mcspine/integrated"
ZIP_TIME = (1980, 1, 1, 0, 0, 0)
MOD_ID = "mc_spine_integrated"

FABRIC_STUB = """package net.fabricmc.api;
public interface ClientModInitializer { void onInitializeClient(); }
"""
NEOFORGE_STUB = """package net.neoforged.fml.common;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)
@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)
public @interface Mod { String value(); }
"""


def manifests():
    fabric = {
        "schemaVersion": 1,
        "id": MOD_ID,
        "version": "0.1.0",
        "name": "mc-spine Integrated Bridge",
        "description": "Loopback native server lifecycle coordinator for Minecraft 1.21.1",
        "environment": "client",
        "entrypoints": {"client": ["net.mcspine.integrated.FabricIntegratedMod"]},
        "depends": {"fabricloader": ">=0.16.0", "minecraft": "1.21.1", "java": ">=21"},
        "license": "GPL-3.0-or-later",
    }
    neoforge = """modLoader="javafml"
loaderVersion="[1,)"
license="GPL-3.0-or-later"

[[mods]]
modId="mc_spine_integrated"
version="0.1.0"
displayName="mc-spine Integrated Bridge"
description="Local mc-spine process lifecycle bridge for Minecraft 1.21.1"

[[dependencies.mc_spine_integrated]]
modId="minecraft"
type="required"
versionRange="[1.21.1,1.21.2)"
side="CLIENT"

[[dependencies.mc_spine_integrated]]
modId="neoforge"
type="required"
versionRange="[21.1,)"
side="CLIENT"
"""
    return {
        "META-INF/MANIFEST.MF": b"Manifest-Version: 1.0\r\nImplementation-Version: 0.1.0\r\n"
        b"Main-Class: net.mcspine.integrated.IntegratedServerBridge\r\n\r\n",
        "fabric.mod.json": (json.dumps(fabric, indent=2) + "\n").encode(),
        "META-INF/neoforge.mods.toml": neoforge.encode(),
    }


def build_jar(out_dir):
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="mc-spine-integrated-javac-") as scratch:
        scratch = Path(scratch)
        stubs = scratch / "stub_sources"
        classes = scratch / "stub_classes"
        output = scratch / "classes"
        for name, body in (
            ("net/fabricmc/api/ClientModInitializer.java", FABRIC_STUB),
            ("net/neoforged/fml/common/Mod.java", NEOFORGE_STUB),
        ):
            path = stubs / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body)
        subprocess.run([
            "javac", f"-J-Djava.io.tmpdir={tempfile.gettempdir()}",
            "-J-XX:-UsePerfData", "--release", "21", "-d", str(classes),
            *map(str, sorted(stubs.rglob("*.java"))),
        ], check=True)
        subprocess.run([
            "javac", f"-J-Djava.io.tmpdir={tempfile.gettempdir()}",
            "-J-XX:-UsePerfData", "--release", "21", "-cp", str(classes),
            "-d", str(output), *map(str, sorted(JAVA_PACKAGE.glob("*.java"))),
        ], check=True)
        entries = manifests()
        entries.update({
            path.relative_to(output).as_posix(): path.read_bytes()
            for path in output.rglob("*.class")
        })
        jar = out_dir / "mc-spine-integrated.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            for name, data in sorted(entries.items()):
                entry = zipfile.ZipInfo(name, ZIP_TIME)
                entry.create_system = 3
                entry.external_attr = 0o644 << 16
                entry.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(entry, data, compresslevel=9)
        return jar


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()
    jar = build_jar(args.out_dir)
    print(f"built {jar} ({jar.stat().st_size} bytes, sha256={hashlib.sha256(jar.read_bytes()).hexdigest()})")


if __name__ == "__main__":
    main()
