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
JAVA_PACKAGE = HERE / "net/megu/integrated"
ZIP_TIME = (1980, 1, 1, 0, 0, 0)
MOD_ID = "megu_integrated"

COMPILE_STUBS = {
    "net/fabricmc/api/ClientModInitializer.java": """package net.fabricmc.api;
public interface ClientModInitializer { void onInitializeClient(); }
""",
    "net/neoforged/fml/common/Mod.java": """package net.neoforged.fml.common;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)
@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)
public @interface Mod { String value(); }
""",
    "net/neoforged/bus/api/Event.java": """package net.neoforged.bus.api;
public abstract class Event { }
""",
    "net/neoforged/bus/api/IEventBus.java": """package net.neoforged.bus.api;
public interface IEventBus {
    <T extends Event> void addListener(java.util.function.Consumer<T> consumer);
}
""",
    "net/neoforged/neoforge/common/NeoForge.java": """package net.neoforged.neoforge.common;
public class NeoForge { public static final net.neoforged.bus.api.IEventBus EVENT_BUS = null; }
""",
    "net/neoforged/neoforge/event/GameShuttingDownEvent.java": """package net.neoforged.neoforge.event;
public class GameShuttingDownEvent extends net.neoforged.bus.api.Event { }
""",
    "net/neoforged/neoforge/client/event/ClientPlayerNetworkEvent.java": """package net.neoforged.neoforge.client.event;
public abstract class ClientPlayerNetworkEvent extends net.neoforged.bus.api.Event {
    public static class LoggingOut extends ClientPlayerNetworkEvent { }
}
""",
}


def manifests():
    fabric = {
        "schemaVersion": 1,
        "id": MOD_ID,
        "version": "0.1.0",
        "name": "megu Integrated Bridge",
        "description": "Loopback native server lifecycle coordinator for Minecraft 1.21.1",
        "environment": "client",
        "entrypoints": {"client": ["net.megu.integrated.FabricIntegratedMod"]},
        "depends": {"fabricloader": ">=0.16.0", "minecraft": "1.21.1", "java": ">=21"},
        "license": "LGPL-3.0-or-later",
    }
    neoforge = """modLoader="javafml"
loaderVersion="[1,)"
license="LGPL-3.0-or-later"

[[mods]]
modId="megu_integrated"
version="0.1.0"
displayName="megu Integrated Bridge"
description="Local megu process lifecycle bridge for Minecraft 1.21.1"

[[dependencies.megu_integrated]]
modId="minecraft"
type="required"
versionRange="[1.21.1,1.21.2)"
side="CLIENT"

[[dependencies.megu_integrated]]
modId="neoforge"
type="required"
versionRange="[21.1,)"
side="CLIENT"
"""
    return {
        "META-INF/MANIFEST.MF": b"Manifest-Version: 1.0\r\nImplementation-Version: 0.1.0\r\n"
        b"Main-Class: net.megu.integrated.IntegratedServerBridge\r\n\r\n",
        "fabric.mod.json": (json.dumps(fabric, indent=2) + "\n").encode(),
        "META-INF/neoforge.mods.toml": neoforge.encode(),
    }


def build_jar(out_dir):
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="megu-integrated-javac-") as scratch:
        scratch = Path(scratch)
        stubs = scratch / "stub_sources"
        classes = scratch / "stub_classes"
        output = scratch / "classes"
        for name, body in COMPILE_STUBS.items():
            path = stubs / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body)
        subprocess.run([
            "javac", f"-J-Djava.io.tmpdir={tempfile.gettempdir()}",
            "-J-XX:-UsePerfData", "--release", "21", "-d", str(classes),
            *map(str, sorted(stubs.rglob("*.java"))),
        ], check=True)
        sources = sorted(JAVA_PACKAGE.rglob("*.java"))
        if not sources:
            raise RuntimeError("no integrated mod Java sources found")
        subprocess.run([
            "javac", f"-J-Djava.io.tmpdir={tempfile.gettempdir()}",
            "-J-XX:-UsePerfData", "--release", "21", "-cp", str(classes),
            "-d", str(output), *map(str, sources),
        ], check=True)
        # Include every compiled class, including nested records such as
        # IntegratedServerBridge$DirectFrame.class. Sorted names plus the fixed
        # ZIP timestamp make the archive byte-for-byte reproducible.
        compiled = sorted(
            path.relative_to(output).as_posix()
            for path in output.rglob("*.class")
        )
        if not compiled:
            raise RuntimeError("javac produced no integrated mod classes")
        if any(name.startswith(("net/fabricmc/", "net/neoforged/"))
               for name in compiled):
            raise RuntimeError("compile-only loader stubs leaked into the integrated jar")
        entries = manifests()
        entries.update({name: (output / name).read_bytes() for name in compiled})
        jar = out_dir / "megu-integrated.jar"
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
