#!/usr/bin/env python3
"""Compile isolated Fabric 1.21.1 JARs and exercise the static admission gate."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / "compat/fabric-probe/fabric_shimcheck.py"

STUBS = {
    "net/fabricmc/api/ModInitializer.java": "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
    "net/fabricmc/api/DedicatedServerModInitializer.java": "package net.fabricmc.api; public interface DedicatedServerModInitializer { void onInitializeServer(); }",
    "net/fabricmc/api/EnvType.java": "package net.fabricmc.api; public enum EnvType { CLIENT, SERVER }",
    "net/fabricmc/api/Environment.java": "package net.fabricmc.api; import java.lang.annotation.*; @Retention(RetentionPolicy.CLASS) @Target({ElementType.TYPE,ElementType.METHOD,ElementType.FIELD}) public @interface Environment { EnvType value(); }",
    "net/minecraft/class_1937.java": "package net.minecraft; public class class_1937 { public class_5321 method_27983(){return null;} }",
    "net/minecraft/class_5321.java": "package net.minecraft; public class class_5321 {}",
    "net/minecraft/class_310.java": "package net.minecraft; public class class_310 {}",
    "net/minecraft/class_9999.java": "package net.minecraft; public class class_9999 {}",
    "org/spongepowered/asm/mixin/Mixin.java": "package org.spongepowered.asm.mixin; import java.lang.annotation.*; @Retention(RetentionPolicy.CLASS) @Target(ElementType.TYPE) public @interface Mixin { Class<?>[] value() default {}; }",
    "org/spongepowered/asm/mixin/Overwrite.java": "package org.spongepowered.asm.mixin; import java.lang.annotation.*; @Retention(RetentionPolicy.CLASS) @Target(ElementType.METHOD) public @interface Overwrite {}",
    "org/spongepowered/asm/mixin/injection/At.java": "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; @Retention(RetentionPolicy.CLASS) @Target({ElementType.ANNOTATION_TYPE,ElementType.METHOD}) public @interface At { String value(); }",
    "org/spongepowered/asm/mixin/injection/Inject.java": "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; @Retention(RetentionPolicy.CLASS) @Target(ElementType.METHOD) public @interface Inject { String method(); At at(); }",
    "org/spongepowered/asm/mixin/injection/Redirect.java": "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; @Retention(RetentionPolicy.CLASS) @Target(ElementType.METHOD) public @interface Redirect { String method(); At at(); }",
}

SOURCES = {
    "fixture/Good.java": """
package fixture;
import net.fabricmc.api.*; import net.minecraft.class_1937;
@Environment(EnvType.SERVER) public final class Good implements DedicatedServerModInitializer {
  public void onInitializeServer() { class_1937 world = null; world.method_27983(); }
}
""",
    "fixture/ClientHelper.java": """
package fixture;
import net.fabricmc.api.*; import net.minecraft.class_310;
@Environment(EnvType.CLIENT) final class ClientHelper { class_310 client; }
""",
    "fixture/ClientMod.java": """
package fixture;
import net.fabricmc.api.*;
@Environment(EnvType.CLIENT) public final class ClientMod implements ModInitializer { public void onInitialize() {} }
""",
    "fixture/mixin/RedirectMixin.java": """
package fixture.mixin;
import org.spongepowered.asm.mixin.Mixin; import org.spongepowered.asm.mixin.injection.*;
@Mixin(Object.class) public abstract class RedirectMixin { @Redirect(method="run", at=@At("HEAD")) private void redirect() {} }
""",
    "fixture/mixin/GoodMixin.java": """
package fixture.mixin;
import org.spongepowered.asm.mixin.Mixin; import org.spongepowered.asm.mixin.injection.*;
@Mixin(Object.class) public abstract class GoodMixin { @Inject(method=\"run\", at=@At(\"HEAD\")) private void inject() {} }
""",
    "fixture/mixin/InvokeMixin.java": """
package fixture.mixin;
import org.spongepowered.asm.mixin.Mixin; import org.spongepowered.asm.mixin.injection.*;
@Mixin(Object.class) public abstract class InvokeMixin { @Inject(method="run", at=@At("INVOKE")) private void inject() {} }
""",
    "fixture/Unknown.java": """
package fixture;
import net.fabricmc.api.DedicatedServerModInitializer; import net.minecraft.class_9999;
public final class Unknown implements DedicatedServerModInitializer { class_9999 value; public void onInitializeServer(){} }
""",
}


def write_jar(path, entries):
    with ZipFile(path, "w", ZIP_DEFLATED) as zf:
        for name, data in sorted(entries.items()):
            zi = ZipInfo(name, (2000, 1, 1, 0, 0, 0)); zi.compress_type = ZIP_DEFLATED
            zf.writestr(zi, data.encode() if isinstance(data, str) else data)


def main():
    tmpdir = Path(os.environ.get("TMPDIR", ROOT / "target/tmp")).resolve()
    tmpdir.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="fabric-shim-test-", dir=tmpdir))
    try:
        src, classes = work / "src", work / "classes"
        for rel, text in {**STUBS, **SOURCES}.items():
            target = src / rel; target.parent.mkdir(parents=True, exist_ok=True); target.write_text(text)
        classes.mkdir()
        javac = os.environ.get("JAVAC", "javac")
        subprocess.run([javac, "-J-Djava.io.tmpdir=" + str(tmpdir), "-J-XX:-UsePerfData", "--release", "21", "-proc:none", "-d", str(classes), *map(str, src.rglob("*.java"))], check=True)
        compiled = {str(p.relative_to(classes)): p.read_bytes() for p in classes.rglob("*.class")}
        classpath = {k: v for k, v in compiled.items() if k.startswith("fixture/")}
        mixin_classes = {k: v for k, v in classpath.items() if "/mixin/" in k}
        server_classes = {k: v for k, v in classpath.items() if "/mixin/" not in k and k == "fixture/Good.class"}

        def metadata(mod_id="fixture", env="server", entry="fixture.Good", mixins=None):
            value = {"schemaVersion": 1, "id": mod_id, "version": "1.0.0", "environment": env,
                     "entrypoints": {"server": [entry]}, "depends": {"fabricloader": ">=0.15.0", "minecraft": "1.21.1"}}
            if mixins: value["mixins"] = mixins
            return json.dumps(value, sort_keys=True)

        good = {**server_classes, "fixture/ClientHelper.class": classpath["fixture/ClientHelper.class"], "fabric.mod.json": metadata()}
        mixin_cfg = json.dumps({"package": "fixture.mixin", "server": ["RedirectMixin"]})
        good_mixin = {**good, "fixture/mixin/GoodMixin.class": mixin_classes["fixture/mixin/GoodMixin.class"],
                      "fabric.mod.json": metadata(mixins=["good.mixins.json"]),
                      "good.mixins.json": json.dumps({"package":"fixture.mixin","server":["GoodMixin"]})}
        client_mod = {"fixture/ClientMod.class": classpath["fixture/ClientMod.class"], "fabric.mod.json": metadata(env="client", entry="fixture.ClientMod")}
        aw = {**good, "fabric.mod.json": metadata()[:-1] + ', "accessWidener": "fixture.accesswidener"}', "fixture.accesswidener": "accessWidener v2 named"}
        redirect = {**good_mixin, "fixture/mixin/RedirectMixin.class": mixin_classes["fixture/mixin/RedirectMixin.class"]}
        invoke = {**good, "fixture/mixin/InvokeMixin.class": mixin_classes["fixture/mixin/InvokeMixin.class"], "fabric.mod.json": metadata(mixins=["invoke.mixins.json"]), "invoke.mixins.json": json.dumps({"package":"fixture.mixin","server":["InvokeMixin"]})}
        unknown = {"fixture/Unknown.class": classpath["fixture/Unknown.class"], "fabric.mod.json": metadata(entry="fixture.Unknown")}
        cases = {"good": good, "good_mixin": good_mixin, "client_mod": client_mod, "access_widener": aw,
                 "redirect": redirect, "invoke": invoke, "unknown": unknown,
                 "missing_metadata": {"fixture/Good.class": server_classes["fixture/Good.class"]}}
        paths = {}
        for name, entries in cases.items():
            paths[name] = work / (name + ".jar"); write_jar(paths[name], entries)

        command = [sys.executable, str(GATE), *map(str, paths.values())]
        # Both invocations must produce byte-for-byte identical machine reports.
        first = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
        second = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
        assert first.returncode == second.returncode == 2, (first.returncode, first.stderr)
        assert first.stdout == second.stdout, "report is not deterministic"
        report = json.loads(first.stdout)
        assert report["status"] == "REJECTED"
        rows = {Path(row["jar"]).stem: row for row in report["jars"]}
        assert rows["good"]["status"] == "COMPATIBLE", rows["good"]
        assert "fixture/ClientHelper" in rows["good"]["client_classes"], rows["good"]
        assert rows["good_mixin"]["status"] == "COMPATIBLE", rows["good_mixin"]
        expected = {"client_mod":"CLIENT_ONLY_MOD", "access_widener":"UNSUPPORTED_ACCESS_WIDENER",
                    "redirect":"UNSUPPORTED_MIXIN_INJECTION", "invoke":"UNSUPPORTED_MIXIN_AT",
                    "unknown":"UNSUPPORTED_SYMBOL", "missing_metadata":"MISSING_METADATA"}
        for name, code in expected.items():
            assert code in {d["code"] for d in rows[name]["diagnostics"]}, (name, rows[name])
        print(json.dumps({"passed": len(cases), "status": "ok", "report_deterministic": True}, sort_keys=True))
    finally:
        shutil.rmtree(work)


if __name__ == "__main__":
    import sys
    main()
