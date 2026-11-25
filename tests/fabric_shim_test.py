#!/usr/bin/env python3
"""Exercise Fabric 1.21.1 static admission and Java 21 runtime with synthetic JARs."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat/fabric-probe"
GATE = ADAPTER / "fabric_shimcheck.py"
sys.path.insert(0, str(ROOT / "tools"))
from java_toolchain import require_java_home, tool_path

JAVA_HOME = require_java_home()
JAVAC = os.environ.get("JAVAC", str(tool_path(JAVA_HOME, "javac")))

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


RUNTIME_SOURCES = {
    "synthetic/Main.java": """
package synthetic;
import net.fabricmc.api.ModInitializer;
public final class Main implements ModInitializer {
    static int count;
    public void onInitialize() {
        if (++count != 1) throw new IllegalStateException("shared mod class loader");
        System.out.println("main");
        try {
            Class.forName("synthetic.ClientEntry");
            throw new AssertionError("client entrypoint was loadable");
        } catch (ClassNotFoundException expected) {}
    }
}
""",
    "synthetic/Dedicated.java": """
package synthetic;
import net.fabricmc.api.DedicatedServerModInitializer;
public final class Dedicated implements DedicatedServerModInitializer {
    public void onInitializeServer() {
        if (Main.count != 1) throw new IllegalStateException("main did not run first");
        System.out.println("server");
    }
}
""",
    "synthetic/ClientEntry.java": """
package synthetic;
import net.fabricmc.api.ModInitializer;
import net.minecraft.class_310;
public final class ClientEntry implements ModInitializer {
    class_310 client;
    public void onInitialize() { throw new AssertionError("client ran"); }
}
""",
    "synthetic/Hooks.java": """
package synthetic;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback;
import net.fabricmc.fabric.api.event.EventFactory;
import net.fabricmc.fabric.api.event.lifecycle.v1.*;
import net.fabricmc.fabric.api.event.player.*;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.registry.Registry;
import net.minecraft.registry.Registries;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.server.command.CommandManager;
import net.minecraft.util.*;
public final class Hooks implements ModInitializer {
    public void onInitialize() {
        if (Registry.register(Registries.BLOCK, "test:z", "z") != "z") throw new AssertionError();
        Registry.register(Registries.BLOCK, Identifier.of("test", "a"), "a");
        net.minecraft.core.Registry.register(BuiltInRegistries.ITEM, ResourceLocation.parse("test:item"), "item");
        if (Registries.ITEM.get(Identifier.of("test:item")) != "item") throw new AssertionError("Yarn read");
        if (BuiltInRegistries.BLOCK.get(ResourceLocation.parse("test:a")) != "a") throw new AssertionError("Mojang read");

        var custom = EventFactory.createArrayBacked(Runnable.class, () -> System.out.println("empty"),
            callbacks -> () -> { for (Runnable callback : callbacks) callback.run(); });
        custom.invoker().run();
        custom.register(() -> System.out.println("custom-1"));
        custom.register(() -> System.out.println("custom-2"));
        custom.invoker().run();
        CommandRegistrationCallback.EVENT.register((dispatcher, access, environment) -> {
            if (access == null || environment != CommandManager.RegistrationEnvironment.DEDICATED) {
                throw new AssertionError("expected dedicated registration context");
            }
            var command = dispatcher.register(CommandManager.literal("zeta"));
            if (!command.getName().equals("zeta")) throw new AssertionError("wrong command");
            dispatcher.register(CommandManager.literal("alpha"));
        });
        ServerLifecycleEvents.SERVER_STARTING.register(server -> System.out.println("starting"));
        ServerLifecycleEvents.SERVER_STARTED.register(server -> {
            checkFrozen(() -> Registry.register(Registries.BLOCK, "test:late", "late"));
            checkFrozen(() -> net.minecraft.core.Registry.register(BuiltInRegistries.ITEM, "test:late", "late"));
            if (Registries.ITEM.get(Identifier.of("test:late")) != null) throw new AssertionError("late write");
            System.out.println("started");
        });
        ServerLifecycleEvents.SERVER_STOPPING.register(server -> System.out.println("stopping"));
        ServerTickEvents.START_SERVER_TICK.register(server -> System.out.println("tick-start"));
        ServerTickEvents.END_SERVER_TICK.register(server -> System.out.println("tick-end"));
        PlayerBlockBreakEvents.BEFORE.register((world, player, pos, state, entity) -> {
            System.out.println("before:" + pos.getX());
            return pos.getX() != 1;
        });
        PlayerBlockBreakEvents.BEFORE.register((world, player, pos, state, entity) -> {
            if (entity != null || pos.getY() != -2 || pos.getZ() != 3) throw new AssertionError("wrong break context");
            System.out.println("before-second");
            return true;
        });
        PlayerBlockBreakEvents.AFTER.register((world, player, pos, state, entity) -> System.out.println("after"));
        UseBlockCallback.EVENT.register((player, world, hand, hit) -> {
            if (hand != Hand.MAIN_HAND || world.getRegistryKey() == null) throw new AssertionError("wrong use context");
            System.out.println("use-first");
            return ActionResult.PASS;
        });
        UseBlockCallback.EVENT.register((player, world, hand, hit) -> switch (hit.getBlockPos().getX()) {
            case 1 -> ActionResult.FAIL;
            case 2 -> ActionResult.SUCCESS;
            default -> ActionResult.PASS;
        });
        UseBlockCallback.EVENT.register((player, world, hand, hit) -> {
            System.out.println("use-last");
            return ActionResult.PASS;
        });
    }

    private static void checkFrozen(Runnable register) {
        try {
            register.run();
            throw new AssertionError("late registration succeeded");
        } catch (IllegalStateException expected) {
            if (!expected.getMessage().contains("frozen")) throw expected;
        }
    }
}
""",
    "synthetic/Late.java": """
package synthetic;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerTickEvents;
import net.minecraft.registry.*;
public final class Late implements ModInitializer {
    public void onInitialize() {
        Registry.register(Registries.ITEM, "test:early", "early");
        ServerTickEvents.START_SERVER_TICK.register(server ->
            Registry.register(Registries.ITEM, "test:late", "late"));
    }
}
""",
    "synthetic/ClientHelper.java": """
package synthetic;
import net.fabricmc.api.*;
@Environment(EnvType.CLIENT)
final class ClientHelper { static void hidden() {} }
""",
    "synthetic/ClientHandle.java": """
package synthetic;
import net.fabricmc.api.ModInitializer;
public final class ClientHandle implements ModInitializer {
    public void onInitialize() {}
    static Runnable callback() { return ClientHelper::hidden; }
}
""",
    "synthetic/ClientMember.java": """
package synthetic;
import net.fabricmc.api.*;
public final class ClientMember implements ModInitializer {
    public void onInitialize() {}
    @Environment(EnvType.CLIENT) void hidden() {}
}
""",
    "synthetic/Contaminated.java": """
package synthetic;
import net.fabricmc.api.ModInitializer;
import net.minecraft.class_310;
import net.minecraft.client.render.WorldRenderer;
public final class Contaminated implements ModInitializer {
    class_310 client;
    WorldRenderer renderer;
    public void onInitialize() {}
}
""",
}

INTERMEDIARY_SOURCE = """
package intermediary;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback;
import net.fabricmc.fabric.api.event.player.*;
import net.minecraft.*;
public final class Hooks implements ModInitializer {
    public void onInitialize() {
        var id = class_2960.method_60654("test:intermediary");
        class_2378.method_10230(class_7923.field_41175, id, "marker");
        if (class_7923.field_41175.method_10223(id) != "marker") throw new AssertionError("missing registration");
        PlayerBlockBreakEvents.BEFORE.register((world, player, pos, state, entity) ->
            world.method_27983() != null && pos.method_10263() > 0);
        UseBlockCallback.EVENT.register((player, world, hand, hit) ->
            hit.method_17777().method_10263() == 5 ? class_1269.field_5812 : class_1269.field_5811);
        CommandRegistrationCallback.EVENT.register((dispatcher, access, environment) -> {
            if (environment != class_2170.class_5364.field_25420) throw new AssertionError("wrong environment");
            dispatcher.register(class_2170.method_9247("intermediary"));
        });
    }
}
"""


def compile_runtime_fixture(work, name, sources, runtime):
    src, classes = work / name / "src", work / name / "classes"
    classes.mkdir(parents=True)
    for name, data in sources.items():
        target = src / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(data)
    subprocess.run([JAVAC, "--release", "21", "-proc:none", "-cp", str(runtime),
                    "-d", str(classes), *map(str, sorted(src.rglob("*.java")))], check=True)
    return {str(p.relative_to(classes)): p.read_bytes() for p in classes.rglob("*.class")}


def runtime_checks(work, static_paths):
    build = work / "build"
    subprocess.run([sys.executable, str(ADAPTER / "build.py"), "--java-home", str(JAVA_HOME),
                    "--build-dir", str(build)], check=True, stdout=subprocess.DEVNULL)
    named = build / "fabric-runtime.jar"
    intermediary = build / "fabric-runtime-intermediary.jar"
    compiled = compile_runtime_fixture(work, "named", {
        **RUNTIME_SOURCES,
        "net/minecraft/class_310.java": STUBS["net/minecraft/class_310.java"],
        "net/minecraft/client/render/WorldRenderer.java":
            "package net.minecraft.client.render; public final class WorldRenderer {}",
    }, named)
    projected = compile_runtime_fixture(work, "intermediary", {"intermediary/Hooks.java": INTERMEDIARY_SOURCE},
                                        intermediary)

    def mod(mod_id, entries, classes, *, env="*", extra=None, source=compiled):
        jar = work / (mod_id + ".jar")
        metadata = {"schemaVersion": 1, "id": mod_id, "version": "1.0.0", "environment": env,
                    "entrypoints": entries, "depends": {"fabricloader": ">=0.15.0", "minecraft": "1.21.1"}}
        payload = {name.replace(".", "/") + ".class": source[name.replace(".", "/") + ".class"]
                   for name in classes}
        write_jar(jar, {**payload, "fabric.mod.json": json.dumps(metadata), **(extra or {})})
        return jar

    def run(runtime, batch, *jars, exit_code=0):
        proc = subprocess.run([str(tool_path(JAVA_HOME, "java")), "-jar", str(runtime), str(GATE), batch,
                               *map(str, jars)], capture_output=True, text=True, cwd=ROOT, timeout=60)
        assert proc.returncode == exit_code, (proc.returncode, proc.stdout, proc.stderr)
        log = [line for line in proc.stderr.splitlines() if not line.startswith("Picked up JAVA_TOOL_OPTIONS:")]
        return proc.stdout, json.loads(proc.stdout), log

    # Separate JARs may use the same class names, with independent static state.
    entries = {"main": ["synthetic.Main"], "server": ["synthetic.Dedicated"],
               "client": ["synthetic.ClientEntry"]}
    classes = [group[0] for group in entries.values()]
    universal = mod("universal", entries, classes)
    dedicated = mod("dedicated", entries, classes, env="server")
    _, report, log = run(named, "-", universal, dedicated)
    assert report["status"] == "OK" and log == ["main", "main", "server", "server"], (report, log)
    assert [(e["mod"], e["kind"]) for e in report["entrypoints"]] == [
        ("universal", "main"), ("dedicated", "main"), ("universal", "server"), ("dedicated", "server")], report
    passed = 1

    hooks = mod("hooks", {"main": ["synthetic.Hooks"]}, ["synthetic.Hooks"])
    batch = "tick,break:1:-2:3,break:2:-2:3,use:1:2:3,use:2:2:3,use:3:2:3"
    raw, report, log = run(named, batch, hooks)
    assert run(named, batch, hooks)[0] == raw, "runtime report is not deterministic"
    assert report["status"] == "OK" and report["registry_frozen"], report
    assert report["mods"] == [{"id": "hooks", "sha256": hashlib.sha256(hooks.read_bytes()).hexdigest()}], report
    assert report["surface_sha256"] == hashlib.sha256((ADAPTER / "shim-surface.json").read_bytes()).hexdigest(), report
    assert report["registry"] == {"minecraft:block": ["test:a", "test:z"], "minecraft:item": ["test:item"]}, report
    assert report["commands"] == ["zeta", "alpha"], report
    assert report["batch"] == [
        {"kind": "tick"},
        {"kind": "break", "pos": [1, -2, 3], "canceled": True, "mutations": []},
        {"kind": "break", "pos": [2, -2, 3], "canceled": False, "mutations": []},
        {"kind": "use", "pos": [1, 2, 3], "result": "FAIL", "handled": True, "canceled": True, "mutations": []},
        {"kind": "use", "pos": [2, 2, 3], "result": "SUCCESS", "handled": True, "canceled": False, "mutations": []},
        {"kind": "use", "pos": [3, 2, 3], "result": "PASS", "handled": False, "canceled": False, "mutations": []},
    ], report
    assert log == ["empty", "custom-1", "custom-2", "starting", "started", "tick-start", "tick-end",
                   "before:1", "before:2", "before-second", "after", "use-first", "use-first",
                   "use-first", "use-last", "stopping"], log
    passed += 1

    late = mod("late", {"main": ["synthetic.Late"]}, ["synthetic.Late"])
    _, report, _ = run(named, "break:0:0:0,tick", late, exit_code=1)
    assert (report["stage"], report["exception"]) == ("batch[1].tick", "java.lang.IllegalStateException"), report
    assert "frozen" in report["message"] and "batch" not in report and "registry" not in report, report
    passed += 1

    inter = mod("projected", {"main": ["intermediary.Hooks"]}, ["intermediary.Hooks"], source=projected)
    _, report, log = run(intermediary, "break:-1:2:3,use:5:1:0", inter)
    assert report["namespace"] == "intermediary" and not log, (report, log)
    assert report["commands"] == ["intermediary"] and report["registry"]["minecraft:block"] == ["test:intermediary"], report
    assert report["batch"][0]["canceled"] and report["batch"][1]["result"] == "SUCCESS", report
    passed += 1

    contaminated = mod("contaminated", {"main": ["synthetic.Contaminated"]}, ["synthetic.Contaminated"])
    client_handle = mod("client_handle", {"main": ["synthetic.ClientHandle"]},
                       ["synthetic.ClientHandle", "synthetic.ClientHelper"])
    client_member = mod("client_member", {"main": ["synthetic.ClientMember"]}, ["synthetic.ClientMember"])
    manifest = mod("manifest", {}, [], extra={
        "META-INF/MANIFEST.MF": "Manifest-Version: 1.0\r\nClass-Path: sibling.jar\r\n\r\n"})
    for jar, code in [
        (static_paths["good_mixin"], "UNSUPPORTED_RUNTIME_MIXIN"),
        (static_paths["access_widener"], "UNSUPPORTED_ACCESS_WIDENER"),
        (static_paths["client_mod"], "CLIENT_ONLY_MOD"),
        (contaminated, "UNSUPPORTED_SYMBOL"),
        (client_handle, "CLIENT_ONLY_REFERENCE"),
        (client_member, "CLIENT_ONLY_MEMBER"),
        (manifest, "UNSUPPORTED_MANIFEST_CLASS_PATH"),
        (inter, "UNSUPPORTED_SYMBOL"),
    ]:
        _, report, log = run(named, "-", universal, jar, exit_code=2)
        diagnostics = [d for row in report["admission"]["jars"] for d in row["diagnostics"]]
        assert code in {d["code"] for d in diagnostics} and not log, (jar.name, report, log)
        if jar == contaminated:
            assert {"net/minecraft/class_310", "net/minecraft/client/render/WorldRenderer"} <= {
                d["resource"].split("|")[0] for d in diagnostics if d["code"] == "UNSUPPORTED_SYMBOL"}, report
        passed += 1

    wrong_spi = mod("wrong_spi", {"server": ["synthetic.Hooks"]}, ["synthetic.Hooks"])
    _, report, log = run(named, "-", universal, wrong_spi, exit_code=1)
    assert report["stage"] == "entrypoint_preflight" and not log, (report, log)
    return passed + 1


def main():
    tmpdir = Path(os.environ.get("TMPDIR", ROOT / "target/tmp")).resolve()
    tmpdir.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="fabric-shim-test-", dir=tmpdir))
    try:
        src, classes = work / "src", work / "classes"
        for rel, text in {**STUBS, **SOURCES}.items():
            target = src / rel; target.parent.mkdir(parents=True, exist_ok=True); target.write_text(text)
        classes.mkdir()
        subprocess.run([JAVAC, "-J-Djava.io.tmpdir=" + str(tmpdir), "-J-XX:-UsePerfData", "--release", "21", "-proc:none", "-d", str(classes), *map(str, src.rglob("*.java"))], check=True)
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
        redirect_mixin_cfg = json.dumps({"package": "fixture.mixin", "server": ["RedirectMixin"]})
        good_mixin = {**good, "fixture/mixin/GoodMixin.class": mixin_classes["fixture/mixin/GoodMixin.class"],
                      "fabric.mod.json": metadata(mixins=["good.mixins.json"]),
                      "good.mixins.json": json.dumps({"package":"fixture.mixin","server":["GoodMixin"]})}
        client_mod = {"fixture/ClientMod.class": classpath["fixture/ClientMod.class"], "fabric.mod.json": metadata(env="client", entry="fixture.ClientMod")}
        aw = {**good, "fabric.mod.json": metadata()[:-1] + ', "accessWidener": "fixture.accesswidener"}', "fixture.accesswidener": "accessWidener v2 named"}
        redirect = {**good, "fixture/mixin/RedirectMixin.class": mixin_classes["fixture/mixin/RedirectMixin.class"],
                    "fabric.mod.json": metadata(mixins=["redirect.mixins.json"]),
                    "redirect.mixins.json": redirect_mixin_cfg}
        invoke = {**good, "fixture/mixin/InvokeMixin.class": mixin_classes["fixture/mixin/InvokeMixin.class"], "fabric.mod.json": metadata(mixins=["invoke.mixins.json"]), "invoke.mixins.json": json.dumps({"package":"fixture.mixin","server":["InvokeMixin"]})}
        unknown = {"fixture/Unknown.class": classpath["fixture/Unknown.class"], "fabric.mod.json": metadata(entry="fixture.Unknown")}
        cases = {"good": good, "good_mixin": good_mixin, "client_mod": client_mod, "access_widener": aw,
                 "redirect": redirect, "invoke": invoke, "unknown": unknown,
                 "missing_metadata": {"fixture/Good.class": server_classes["fixture/Good.class"]}}
        paths = {}
        for name, entries in cases.items():
            paths[name] = work / (name + ".jar"); write_jar(paths[name], entries)

        command = [sys.executable, str(GATE), "--java-home", str(JAVA_HOME), *map(str, paths.values())]
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
        runtime_passed = runtime_checks(work, paths)
        print(json.dumps({"passed": len(cases) + runtime_passed, "static_passed": len(cases),
                          "runtime_passed": runtime_passed, "status": "ok", "report_deterministic": True},
                         sort_keys=True))
    finally:
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
