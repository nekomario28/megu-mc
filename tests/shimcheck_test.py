"""NF-0 static NeoForge compatibility contract and negative fixtures."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat" / "neoforge-events"
TOOL = ROOT / "tools" / "shimcheck.py"
JAVA_HOME = Path("/usr/lib/jvm/java-21-openjdk")

MODS_TOML = '''modLoader="javafml"
loaderVersion="[1,)"
license="MIT"
[[mods]]
modId="testmod"
version="1.2.3"
displayName="Test Mod"
[[dependencies.testmod]]
modId="neoforge"
mandatory=true
versionRange="[21.1,)"
side="BOTH"
[[dependencies.testmod]]
modId="minecraft"
mandatory=true
versionRange="[1.21.1,1.22)"
side="BOTH"
'''


class ShimCheckTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tmp_root = ROOT / "target" / "tmp"
        tmp_root.mkdir(parents=True, exist_ok=True)
        cls.workspace = tempfile.TemporaryDirectory(prefix="shimcheck-", dir=tmp_root)
        cls.tmp = Path(cls.workspace.name)
        cls.env = dict(os.environ, TMPDIR=str(tmp_root),
                       _JAVA_OPTIONS=f"-Djava.io.tmpdir={tmp_root} -XX:-UsePerfData")
        if not (ADAPTER / ".build/classpath.txt").is_file():
            subprocess.run([sys.executable, str(ADAPTER / "build.py"), "--java-home", str(JAVA_HOME)],
                           cwd=ROOT, env=cls.env, check=True, capture_output=True)
        cls.seed = ADAPTER / ".cache/seedprotect-1.0.0.jar"
        cls.cp = (ADAPTER / ".build/classpath.txt").read_text().strip()
        cls.output = cls.tmp / "classes"
        cls.output.mkdir()
        sources = {
            "testmod/SimpleMod.java": '''package testmod;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
@Mod("testmod") public class SimpleMod {
  public SimpleMod() { NeoForge.EVENT_BUS.register(this); }
  @SubscribeEvent public void on(FarmlandTrampleEvent event) { event.setCanceled(true); }
}
''',
            "testmod/BadClassMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("testmod") public class BadClassMod {
  public void bad() { net.minecraft.unsupported.MissingApi.absent(); }
}
''',
            "testmod/BadMemberMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("testmod") public class BadMemberMod {
  public void bad(net.minecraft.world.level.Level level) { level.missingApi(); }
}
''',
            "testmod/BadArrayMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("testmod") public class BadArrayMod {
  public Object bad(Object value) { return (net.minecraft.unsupported.MissingApi[]) value; }
}
''',
            "testmod/PriorityMod.java": '''package testmod;
import net.neoforged.bus.api.EventPriority;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
@net.neoforged.fml.common.Mod("testmod") public class PriorityMod {
  @SubscribeEvent(priority=EventPriority.HIGH)
  public void on(FarmlandTrampleEvent event) { event.setCanceled(true); }
  public EventPriority priority() { return EventPriority.HIGH; }
}
''',
            "testmod/ConstructEventMod.java": '''package testmod;
import net.minecraft.core.BlockPos;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
@net.neoforged.fml.common.Mod("testmod") public class ConstructEventMod {
  public FarmlandTrampleEvent make() {
    return new FarmlandTrampleEvent(new Level(), new BlockPos(1, 2, 3), new BlockState(), 0.5f, new Entity());
  }
}
''',
            "testmod/DynamicMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("testmod") public class DynamicMod {
  public Runnable bad() { return net.minecraft.unsupported.MissingApi::absent; }
}
''',
            "testmod/ClientSubscriber.java": '''package testmod;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.api.distmarker.Dist;
@EventBusSubscriber(value={Dist.CLIENT}, modid="testmod")
public class ClientSubscriber {
  public static void init() { net.minecraft.client.Minecraft.getInstance(); }
}
''',
            "testmod/ServerClientRefMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("testmod") public class ServerClientRefMod {
  public void reference() { ClientSubscriber.init(); }
}
''',
            "testmod/SecondMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("secondmod") public class SecondMod { }
''',
            "testmod/BothSubscriber.java": '''package testmod;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.api.distmarker.Dist;
@EventBusSubscriber(value={Dist.CLIENT,Dist.DEDICATED_SERVER}, modid="testmod")
public class BothSubscriber {
  public static void init() { net.minecraft.client.Minecraft.getInstance(); }
}
''',
            "testmod/OnlyInClient.java": '''package testmod;
import net.neoforged.api.distmarker.OnlyIn;
import net.neoforged.api.distmarker.Dist;
@OnlyIn(Dist.CLIENT) public class OnlyInClient {
  public static void init() { net.minecraft.client.Minecraft.getInstance(); }
}
''',
            "testmod/MethodOnlyInMod.java": '''package testmod;
import net.neoforged.api.distmarker.OnlyIn;
import net.neoforged.api.distmarker.Dist;
@net.neoforged.fml.common.Mod("testmod") public class MethodOnlyInMod {
  @OnlyIn(Dist.CLIENT) public void clientMethod() { net.minecraft.client.Minecraft.getInstance(); }
}
''',
            "testmod/BadMixin.java": '''package testmod;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;
@Mixin(targets="net.minecraft.world.level.Level") public class BadMixin {
  @Redirect(method="tick", at=@At(value="INVOKE", target="Lnet/minecraft/world/level/Level;run()V"))
  private void redirect() {}
}
''',
            "testmod/ArrayMixin.java": '''package testmod;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(targets="net.minecraft.world.level.Level") public class ArrayMixin {
  @Inject(method="tick", at={@At("HEAD"), @At("INVOKE")}) private void invoke() {}
  @Inject(method="tick", at={@At("STORE"), @At("TAIL")}) private void store() {}
}
''',
            "testmod/BoundedFacadeMod.java": '''package testmod;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.LevelAccessor;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.neoforged.neoforge.event.entity.living.LivingDamageEvent;
import net.neoforged.neoforge.event.tick.ServerTickEvent;
@net.neoforged.fml.common.Mod("testmod") public class BoundedFacadeMod {
  int coordinate(BlockPos pos) { return pos.getX() + pos.getY() + pos.getZ(); }
  void schedule(Level level, LevelAccessor world, BlockPos pos, Block block) {
    world.scheduleTick(pos, block, 2);
    level.scheduleTick(pos, block, 3);
  }
  Object dimension(BlockEntity entity) { return entity.getLevel().dimension(); }
  void limit(LivingDamageEvent.Pre event) {
    event.setNewDamage(Math.min(event.getContainer().getNewDamage(), 8));
  }
  boolean time(ServerTickEvent event, ServerTickEvent.Pre pre, ServerTickEvent.Post post) {
    return event.hasTime() && pre.hasTime() && post.hasTime();
  }
}
''',
            "testmod/UnsupportedLevelMod.java": '''package testmod;
@net.neoforged.fml.common.Mod("testmod") public class UnsupportedLevelMod {
  boolean check(net.minecraft.world.level.Level level, net.minecraft.core.BlockPos pos) {
    return level.isOutsideBuildHeight(pos);
  }
}
''',
            "net/minecraft/unsupported/MissingApi.java": '''package net.minecraft.unsupported;
public class MissingApi { public static void absent() {} }
''',
            "net/minecraft/client/Minecraft.java": '''package net.minecraft.client;
public class Minecraft { public static Minecraft getInstance() { return null; } }
''',
            "net/minecraft/world/level/Level.java": '''package net.minecraft.world.level;
import net.minecraft.core.BlockPos;
import net.minecraft.resources.ResourceKey;
import net.minecraft.world.level.block.Block;
public class Level implements LevelAccessor {
  public void missingApi() {}
  public native boolean isOutsideBuildHeight(BlockPos pos);
  public native void scheduleTick(BlockPos pos, Block block, int delay);
  public native ResourceKey<Level> dimension();
}
''',
            "org/spongepowered/asm/mixin/Mixin.java": '''package org.spongepowered.asm.mixin;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)
@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)
public @interface Mixin { String[] targets(); }
''',
            "org/spongepowered/asm/mixin/injection/At.java": '''package org.spongepowered.asm.mixin.injection;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)
@java.lang.annotation.Target(java.lang.annotation.ElementType.ANNOTATION_TYPE)
public @interface At { String value(); String target() default ""; }
''',
            "org/spongepowered/asm/mixin/injection/Inject.java": '''package org.spongepowered.asm.mixin.injection;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)
@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)
public @interface Inject { String method(); At[] at(); }
''',
            "org/spongepowered/asm/mixin/injection/Redirect.java": '''package org.spongepowered.asm.mixin.injection;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)
@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)
public @interface Redirect { String method(); At at(); }
''',
        }
        source_paths = []
        for name, contents in sources.items():
            destination = cls.tmp / "src" / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(contents)
            source_paths.append(str(destination))
        subprocess.run([str(JAVA_HOME / "bin/javac"), "-J-Xmx128m", "--release", "21", "-proc:none",
                        "-cp", cls.cp, "-d", str(cls.output), *source_paths],
                       cwd=ROOT, env=cls.env, check=True, capture_output=True)

    @classmethod
    def tearDownClass(cls):
        cls.workspace.cleanup()

    def fixture(self, name="SimpleMod", toml=MODS_TOML, extra=None, include_client=False,
                extra_classes=()):
        jar = self.tmp / f"test-{self._testMethodName}-{name}.jar"
        with zipfile.ZipFile(jar, "w", compression=zipfile.ZIP_DEFLATED) as output:
            if toml is not None:
                output.writestr("META-INF/neoforge.mods.toml", toml)
            output.write(self.output / "testmod" / f"{name}.class", f"testmod/{name}.class")
            if include_client:
                output.write(self.output / "testmod/ClientSubscriber.class", "testmod/ClientSubscriber.class")
            for extra_class in extra_classes:
                output.write(self.output / "testmod" / f"{extra_class}.class",
                             f"testmod/{extra_class}.class")
            for path, contents in (extra or {}).items():
                output.writestr(path, contents)
        return jar

    def invoke(self, *jars, surface=None):
        command = [sys.executable, str(TOOL), "--json", "--java-home", str(JAVA_HOME)]
        if surface is not None:
            command += ["--surface", str(surface)]
        command += [str(jar) for jar in jars]
        proc = subprocess.run(command, cwd=ROOT, env=self.env, capture_output=True, text=True)
        return proc, json.loads(proc.stdout) if proc.stdout.strip() else None

    def assert_rejected(self, jar, code, symbol=None):
        proc, report = self.invoke(jar)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(report["status"], "REJECTED")
        reasons = report["jars"][0]["reasons"]
        self.assertIn(code, [item["code"] for item in reasons], reasons)
        if symbol is not None:
            self.assertTrue(any(symbol in item["symbol"] for item in reasons), reasons)
        return report["jars"][0]

    def test_pinned_seed_protect_compatible(self):
        proc, result = self.invoke(self.seed)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(result["status"], "COMPATIBLE")
        report = result["jars"][0]
        self.assertEqual(report["coverage"]["percent"], 100)
        self.assertGreater(report["coverage"]["supported"], 0)
        self.assertEqual(report["coverage"]["unsupported"], 0)
        self.assertEqual(report["reasons"], [])

    def test_compatible_event_mod(self):
        proc, result = self.invoke(self.fixture())
        self.assertEqual(proc.returncode, 0, (proc.stderr, result))
        self.assertEqual(result["status"], "COMPATIBLE")
        self.assertEqual(result["jars"][0]["coverage"]["unsupported"], 0)

    def test_real_event_constructor_and_priority_are_supported(self):
        for name in ("ConstructEventMod", "PriorityMod"):
            with self.subTest(name=name):
                proc, result = self.invoke(self.fixture(name=name))
                self.assertEqual(proc.returncode, 0, (proc.stderr, result))
                self.assertEqual(result["jars"][0]["coverage"]["unsupported"], 0)

    def test_missing_metadata(self):
        self.assert_rejected(self.fixture(toml=None), "MISSING_METADATA")

    def test_invalid_mod_loader(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML.replace('modLoader="javafml"', 'modLoader="lowcodefml"')),
                             "UNSUPPORTED_MOD_LOADER", "modLoader")

    def test_malformed_toml(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML.replace('loaderVersion="[1,)"', 'loaderVersion=')),
                             "MALFORMED_METADATA")

    def test_duplicate_modid(self):
        extra_mod = '\n[[mods]]\nmodId="testmod"\nversion="1.2.3"\ndisplayName="Duplicate"\n'
        self.assert_rejected(self.fixture(toml=MODS_TOML + extra_mod), "DUPLICATE_MOD_ID", "testmod")

    def test_incompatible_minecraft_range(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML.replace('[1.21.1,1.22)', '[1.20,1.21)')),
                             "INCOMPATIBLE_DEPENDENCY", "minecraft")

    def test_incompatible_neoforge_range(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML.replace('[21.1,)', '[22.0,)')),
                             "INCOMPATIBLE_DEPENDENCY", "neoforge")

    def test_incompatible_loader_range(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML.replace('[1,)', '[5,)')),
                             "INVALID_LOADER_VERSION", "loaderVersion")

    def test_pinned_fml_loader_range(self):
        proc, report = self.invoke(self.fixture(toml=MODS_TOML.replace('loaderVersion="[1,)"',
                                                               'loaderVersion="[4,)"')))
        self.assertEqual(proc.returncode, 0, (proc.stderr, report))

    def test_missing_required_other_mod(self):
        dep = '\n[[dependencies.testmod]]\nmodId="othermod"\nmandatory=true\nversionRange="[1,)"\n'
        self.assert_rejected(self.fixture(toml=MODS_TOML + dep), "MISSING_DEPENDENCY", "othermod")

    def test_dependency_across_two_jars(self):
        dep = '\n[[dependencies.testmod]]\nmodId="secondmod"\nmandatory=true\nversionRange="[1,)"\n'
        main = self.fixture(toml=MODS_TOML + dep)
        second = self.fixture(name="SecondMod", toml=MODS_TOML.replace("testmod", "secondmod"))
        proc, result = self.invoke(main, second)
        self.assertEqual(proc.returncode, 0, (proc.stderr, result))
        self.assertEqual(result["status"], "COMPATIBLE")

    def test_cyclic_dependencies(self):
        dep = '\n[[dependencies.testmod]]\nmodId="secondmod"\nmandatory=true\nversionRange="[1,)"\n'
        second_dep = '\n[[dependencies.secondmod]]\nmodId="testmod"\nmandatory=true\nversionRange="[1,)"\n'
        main = self.fixture(toml=MODS_TOML + dep)
        second = self.fixture(name="SecondMod", toml=MODS_TOML.replace("testmod", "secondmod") + second_dep)
        proc, result = self.invoke(main, second)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertTrue(all("CYCLIC_DEPENDENCY" in [reason["code"] for reason in report["reasons"]]
                            for report in result["jars"]))

    def test_client_only_mod_rejected(self):
        toml = MODS_TOML.replace('displayName="Test Mod"', 'displayName="Test Mod"\nclientSideOnly=true')
        self.assert_rejected(self.fixture(toml=toml), "CLIENT_ONLY_MOD")

    def test_client_only_must_be_boolean(self):
        toml = MODS_TOML.replace('displayName="Test Mod"', 'displayName="Test Mod"\nclientSideOnly="true"')
        self.assert_rejected(self.fixture(toml=toml), "INVALID_CLIENT_SIDE_ONLY")

    def test_dependency_mandatory_must_be_boolean(self):
        toml = MODS_TOML.replace('mandatory=true', 'mandatory="true"', 1)
        self.assert_rejected(self.fixture(toml=toml), "INVALID_DEPENDENCY", "testmod")

    def test_unknown_minecraft_class(self):
        self.assert_rejected(self.fixture(name="BadClassMod"), "UNSUPPORTED_SYMBOL", "MissingApi")

    def test_unknown_minecraft_method(self):
        self.assert_rejected(self.fixture(name="BadMemberMod"), "UNSUPPORTED_SYMBOL", "Level#missingApi()V")

    def test_unknown_array_checkcast_type(self):
        self.assert_rejected(self.fixture(name="BadArrayMod"), "UNSUPPORTED_SYMBOL", "MissingApi")

    def test_malformed_table_header(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML + '\n[invalid..table]\nflag=true\n'),
                             "MALFORMED_METADATA")

    def test_invokedynamic_handle_audit(self):
        self.assert_rejected(self.fixture(name="DynamicMod"), "UNSUPPORTED_SYMBOL", "MissingApi#absent()V")

    def test_access_transformers_rejected(self):
        toml = MODS_TOML + '\n[[accessTransformers]]\nfile="META-INF/accesstransformer.cfg"\n'
        config = "META-INF/accesstransformer.cfg"
        jar = self.fixture(toml=toml, extra={config: "public net.minecraft.world.level.Level\n",
            "META-INF/MANIFEST.MF": "Manifest-Version: 1.0\r\nAccessTransformer: accesstransformer.cfg\r\n\r\n"})
        report = self.assert_rejected(jar, "UNSUPPORTED_ACCESS_TRANSFORMER", config)
        self.assertEqual([item["symbol"] for item in report["unsupportedAccessTransformers"]], [config])

    def test_undeclared_access_transformer_configs_rejected(self):
        names = ["META-INF/accesstransformer.cfg", "META-INF/extra/CUSTOM-AccessTransformer-2.cfg"]
        jar = self.fixture(extra={names[0]: "", names[1]: "public net.minecraft.world.level.Level\n",
                                  "META-INF/settings.cfg": "unrelated"})
        report = self.assert_rejected(jar, "UNSUPPORTED_ACCESS_TRANSFORMER", names[0])
        self.assertEqual({item["symbol"] for item in report["unsupportedAccessTransformers"]}, set(names))

    def test_manifest_only_transformer_rejected(self):
        jar = self.fixture(extra={
            "META-INF/MANIFEST.MF": "Manifest-Version: 1.0\r\naccesstransformer: META-INF/patch.cfg\r\n\r\n",
            "META-INF/patch.cfg": "public net.minecraft.world.level.Level\n",
        })
        report = self.assert_rejected(jar, "UNSUPPORTED_ACCESS_TRANSFORMER", "META-INF/patch.cfg")
        self.assertEqual(len(report["unsupportedAccessTransformers"]), 1)

    def test_redirect_mixin_and_invoke_rejected(self):
        toml = MODS_TOML + '\n[[mixins]]\nconfig="bad.mixins.json"\n'
        config = '{"package":"testmod","mixins":["BadMixin"]}'
        jar = self.fixture(toml=toml, extra={"bad.mixins.json": config})
        with zipfile.ZipFile(jar, "a") as output:
            output.write(self.output / "testmod/BadMixin.class", "testmod/BadMixin.class")
        report = self.assert_rejected(jar, "UNSUPPORTED_MIXIN_INJECTION", "@Redirect")
        reasons = {reason["code"] for reason in report["reasons"]}
        self.assertIn("UNSUPPORTED_MIXIN_AT", reasons)
        self.assertIn("UNSUPPORTED_MIXIN_CONFIG", reasons)

    def test_inject_at_array_reports_only_unsupported_points(self):
        jar = self.fixture(name="ArrayMixin", extra_classes=("SimpleMod",))
        report = self.assert_rejected(jar, "UNSUPPORTED_MIXIN_AT")
        self.assertEqual([(item["code"], item["symbol"]) for item in report["reasons"]], [
            ("UNSUPPORTED_MIXIN_AT", "testmod/ArrayMixin @At(INVOKE)"),
            ("UNSUPPORTED_MIXIN_AT", "testmod/ArrayMixin @At(STORE)"),
        ])

    def test_bounded_facades_leave_unimplemented_members_rejected(self):
        ok = self.fixture(name="BoundedFacadeMod")
        unsupported = self.fixture(name="UnsupportedLevelMod", toml=MODS_TOML.replace("testmod", "secondmod"))
        proc, report = self.invoke(ok, unsupported)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(report["jars"][0]["status"], "COMPATIBLE")
        self.assertEqual(report["jars"][1]["unsupportedSymbols"], [
            "net/minecraft/world/level/Level#isOutsideBuildHeight(Lnet/minecraft/core/BlockPos;)Z",
        ])

    def test_client_only_subscriber_is_excluded(self):
        proc, result = self.invoke(self.fixture(include_client=True))
        self.assertEqual(proc.returncode, 0, (proc.stderr, result))
        report = result["jars"][0]
        self.assertIn("testmod/ClientSubscriber", report["clientClasses"])
        self.assertFalse(any("net/minecraft/client/Minecraft" in x for x in report["unsupportedSymbols"]))
        annotated = next(a for a in report["annotations"] if a["class"] == "testmod/ClientSubscriber")
        self.assertEqual(annotated["eventBusSubscriber"]["dist"], ["CLIENT"])
        self.assertEqual(annotated["eventBusSubscriber"]["bus"], "GAME")

    def test_onlyin_client_class_excluded(self):
        proc, result = self.invoke(self.fixture(extra_classes=["OnlyInClient"]))
        self.assertEqual(proc.returncode, 0, (proc.stderr, result))
        self.assertIn("testmod/OnlyInClient", result["jars"][0]["clientClasses"])

    def test_onlyin_method_does_not_exclude_class(self):
        report = self.assert_rejected(self.fixture(name="MethodOnlyInMod"),
                                      "UNSUPPORTED_SYMBOL", "net/minecraft/client/Minecraft")
        self.assertNotIn("testmod/MethodOnlyInMod", report["clientClasses"])

    def test_dual_dist_subscriber_remains_server_reachable(self):
        report = self.assert_rejected(self.fixture(extra_classes=["BothSubscriber"]),
                                      "UNSUPPORTED_SYMBOL", "net/minecraft/client/Minecraft")
        self.assertNotIn("testmod/BothSubscriber", report["clientClasses"])

    def test_missing_mixin_config(self):
        self.assert_rejected(self.fixture(toml=MODS_TOML + '\n[[mixins]]\nconfig="missing.mixins.json"\n'),
                             "MISSING_MIXIN_CONFIG")

    def test_manifest_mixin_config(self):
        jar = self.fixture(extra={"META-INF/MANIFEST.MF":
                                      "Manifest-Version: 1.0\r\nMixinConfigs: empty.mixins.json\r\n\r\n",
                                  "empty.mixins.json": '{"package":"testmod","mixins":[]}'})
        self.assert_rejected(jar, "UNSUPPORTED_MIXIN_CONFIG", "empty.mixins.json")

    def test_server_references_client_only_class(self):
        self.assert_rejected(self.fixture(name="ServerClientRefMod", include_client=True),
                             "SERVER_REFERENCES_CLIENT_CLASS", "ClientSubscriber")

    def test_deterministic_json(self):
        jar = self.fixture()
        first = self.invoke(jar)
        second = self.invoke(jar)
        self.assertEqual(first[0].returncode, 0)
        self.assertEqual(first[0].stdout, second[0].stdout)

    def test_missing_jar_is_io_error(self):
        proc, result = self.invoke(self.tmp / "missing.jar")
        self.assertEqual(proc.returncode, 1)
        self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
