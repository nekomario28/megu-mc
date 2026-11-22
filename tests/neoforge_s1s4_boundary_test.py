"""Focused live NeoForge S1 listener/cancellation wire boundary regression.

The Rust java_compat unit tests verify host CAS atomicity and S3/S4 frame
bounds. These tests exercise the real pinned Java event bus, the static
ShimCheck mixin contract, the post-bootstrap Java callback census and the
encoded S1 frames using the existing fixture rather than a synthetic listener.
"""

import json
import shutil
import subprocess
import unittest
import zipfile

import neoforge_s1_events_test as s1_fixture


ROOT = s1_fixture.ROOT
ADAPTER = s1_fixture.ADAPTER
JAVA = s1_fixture.JAVA
SURFACE = ADAPTER / "shim-surface.json"

# Stub Mixin annotations: the audit reads bytecode only, so fixtures compile
# against these stand-ins without pulling a real Mixin jar into the classpath.
MIXIN_STUBS = {
    "org/spongepowered/asm/mixin/Mixin.java":
        "package org.spongepowered.asm.mixin;"
        "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)"
        "@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)"
        "public @interface Mixin { String[] targets(); }",
    "org/spongepowered/asm/mixin/Overwrite.java":
        "package org.spongepowered.asm.mixin;"
        "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)"
        "@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)"
        "public @interface Overwrite {}",
    "org/spongepowered/asm/mixin/injection/At.java":
        "package org.spongepowered.asm.mixin.injection;"
        "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)"
        "@java.lang.annotation.Target(java.lang.annotation.ElementType.ANNOTATION_TYPE)"
        "public @interface At { String value(); String target() default \"\"; }",
    "org/spongepowered/asm/mixin/injection/Inject.java":
        "package org.spongepowered.asm.mixin.injection;"
        "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)"
        "@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)"
        "public @interface Inject { String method(); At at(); }",
    "org/spongepowered/asm/mixin/injection/Redirect.java":
        "package org.spongepowered.asm.mixin.injection;"
        "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)"
        "@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)"
        "public @interface Redirect { String method(); At at(); }",
    "org/spongepowered/asm/mixin/injection/ModifyArg.java":
        "package org.spongepowered.asm.mixin.injection;"
        "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)"
        "@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)"
        "public @interface ModifyArg { String method(); At at(); int index() default 0; }",
}

BOUNDARY_MOD = '''package nf2;
@net.neoforged.fml.common.Mod("s1fixture") public class BoundaryMod { }
'''

# HEAD/RETURN/TAIL @Inject is the bounded injection surface the audit accepts.
HEAD_MIXIN = '''package nf2;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(targets="net.minecraft.world.level.Level") public class BoundaryHeadMixin {
  @Inject(method="tick", at=@At("HEAD")) private void head() {}
  @Inject(method="tick", at=@At("RETURN")) private void ret() {}
  @Inject(method="tick", at=@At("TAIL")) private void tail() {}
}
'''

OVERWRITE_MIXIN = '''package nf2;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;
@Mixin(targets="net.minecraft.world.level.Level") public class BoundaryOverwriteMixin {
  @Overwrite public void overwrite() {}
}
'''

REDIRECT_MIXIN = '''package nf2;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;
@Mixin(targets="net.minecraft.world.level.Level") public class BoundaryRedirectMixin {
  @Redirect(method="tick", at=@At(value="INVOKE", target="Lnet/minecraft/world/level/Level;run()V"))
  private void redirect() {}
}
'''

MODIFY_ARG_MIXIN = '''package nf2;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyArg;
@Mixin(targets="net.minecraft.world.level.Level") public class BoundaryModifyArgMixin {
  @ModifyArg(method="tick", at=@At("HEAD"), index=0) private int arg(int value) { return value; }
}
'''

INVOKE_MIXIN = '''package nf2;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(targets="net.minecraft.world.level.Level") public class BoundaryInvokeMixin {
  @Inject(method="tick", at=@At("INVOKE")) private void invoke() {}
}
'''

CALLBACK_CHECKS = '''package net.mcspine.neoforge;
public final class BoundaryChecks {
  public static void main(String[] args) throws Exception {
    Bootstrap.initialize(args[1]);
    if (args[0].equals("callbacks")) {
      System.out.println("callbacks=" + Bootstrap.javaCallbacks());
      return;
    }
    throw new AssertionError(args[0]);
  }
}
'''


class NeoForgeS1S4BoundaryTests(unittest.TestCase):
    fixture = s1_fixture.NeoForgeS1EventsTests

    @classmethod
    def setUpClass(cls):
        cls.fixture.setUpClass()
        sources = dict(MIXIN_STUBS)
        sources.update({
            "nf2/BoundaryMod.java": BOUNDARY_MOD,
            "nf2/BoundaryHeadMixin.java": HEAD_MIXIN,
            "nf2/BoundaryOverwriteMixin.java": OVERWRITE_MIXIN,
            "nf2/BoundaryRedirectMixin.java": REDIRECT_MIXIN,
            "nf2/BoundaryModifyArgMixin.java": MODIFY_ARG_MIXIN,
            "nf2/BoundaryInvokeMixin.java": INVOKE_MIXIN,
            "net/mcspine/neoforge/BoundaryChecks.java": CALLBACK_CHECKS,
        })
        src = cls.fixture.root / "boundary-src"
        paths = []
        for relative, content in sources.items():
            path = src / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
            paths.append(str(path))
        compiled = subprocess.run(
            [str(JAVA / "javac"), "-J-Xmx128m", "--release", "21", "-proc:none",
             "-cp", cls.fixture.cp, "-d", str(cls.fixture.classes), *paths],
            cwd=ROOT, env=cls.fixture.env, capture_output=True, text=True)
        if compiled.returncode:
            raise AssertionError(compiled.stderr + compiled.stdout)
        cls.jars = cls.fixture.root / "boundary-jars"
        cls.jars.mkdir()

    @classmethod
    def tearDownClass(cls):
        cls.fixture.tearDownClass()

    def setUp(self):
        self.runner = self.fixture(methodName="test_six_events_encoded_batch")

    def run_java(self, main, *args):
        return self.runner.run_java(main, *args)

    def run_fixture(self, mode, expected):
        result = self.run_java(
            "net.mcspine.neoforge.S1Checks", mode,
            self.fixture.mod_dir.parent,
        )
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn(expected, result.stdout)

    def mixin_jar(self, name, classes, mixin_config=None, config_body=None):
        jar = self.jars / f"{self._testMethodName}-{name}.jar"
        toml = s1_fixture.MOD_TOML
        if mixin_config is not None:
            toml += f'\n[[mixins]]\nconfig="{mixin_config}"\n'
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("META-INF/neoforge.mods.toml", toml)
            for entry in ("nf2/BoundaryMod", *classes):
                archive.write(self.fixture.classes / f"{entry}.class", f"{entry}.class")
            if config_body is not None:
                archive.writestr(mixin_config, config_body)
        return jar

    def shim(self, jar):
        return self.run_java("net.mcspine.neoforge.ShimCheck", "--json",
                             "--surface", SURFACE, jar)

    def test_listener_order_and_canceled_frames(self):
        self.run_fixture("encoded", "PASS encoded S1 six events")

    def test_nested_cancellation_and_failed_scope_cleanup(self):
        self.run_fixture("scopes", "PASS scoped mutation rollback")
        self.run_fixture("overflow", "PASS real event bus reentrancy")

    def test_event_bus_five_phase_ordering_and_cancel_short_circuit(self):
        game = self.fixture.root / f"{self._testMethodName}-game"
        (game / "mods").mkdir(parents=True)
        shutil.copy(ADAPTER / ".cache/seedprotect-1.0.0.jar",
                    game / "mods" / "seedprotect.jar")
        result = self.run_java("net.mcspine.neoforge.EventChecks", game)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("PASS original mod registration", result.stdout)

    def test_bounded_head_return_tail_mixin_passes_audit(self):
        result = self.shim(self.mixin_jar("head", ("nf2/BoundaryHeadMixin",)))
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "COMPATIBLE")
        self.assertEqual(report["jars"][0]["coverage"]["unsupported"], 0)

    def test_unsupported_mixin_injections_reject_closed(self):
        cases = (
            ("overwrite", "nf2/BoundaryOverwriteMixin", "@Overwrite"),
            ("redirect", "nf2/BoundaryRedirectMixin", "@Redirect"),
            ("modifyarg", "nf2/BoundaryModifyArgMixin", "@ModifyArg"),
        )
        for name, entry, annotation in cases:
            with self.subTest(mixin=name):
                result = self.shim(self.mixin_jar(name, (entry,)))
                self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
                report = json.loads(result.stdout)
                self.assertEqual(report["status"], "REJECTED")
                reasons = report["jars"][0]["reasons"]
                injections = [item for item in reasons
                              if item["code"] == "UNSUPPORTED_MIXIN_INJECTION"]
                self.assertTrue(any(annotation in item["symbol"] for item in injections),
                                reasons)

    def test_unknown_mixin_injection_point_rejects_closed(self):
        result = self.shim(self.mixin_jar("invoke", ("nf2/BoundaryInvokeMixin",)))
        self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        reasons = report["jars"][0]["reasons"]
        self.assertIn("UNSUPPORTED_MIXIN_AT",
                      [item["code"] for item in reasons], reasons)
        self.assertTrue(any("@At(INVOKE)" in item["symbol"] for item in reasons), reasons)

    def test_declared_mixin_config_rejects_closed(self):
        config = '{"package":"nf2","mixins":["BoundaryHeadMixin"]}'
        jar = self.mixin_jar("config", ("nf2/BoundaryHeadMixin",),
                             mixin_config="boundary.mixins.json", config_body=config)
        result = self.shim(jar)
        self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        reasons = report["jars"][0]["reasons"]
        self.assertIn("UNSUPPORTED_MIXIN_CONFIG",
                      [item["code"] for item in reasons], reasons)

    def test_missing_mixin_target_class_rejects_closed(self):
        config = '{"package":"nf2","mixins":["BoundaryHeadMixin","AbsentMixin"]}'
        jar = self.mixin_jar("missing", ("nf2/BoundaryHeadMixin",),
                             mixin_config="boundary.mixins.json", config_body=config)
        result = self.shim(jar)
        self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        reasons = report["jars"][0]["reasons"]
        missing = [item for item in reasons if item["code"] == "MISSING_MIXIN_CLASS"]
        self.assertTrue(any("nf2/AbsentMixin" in item["symbol"] for item in missing),
                        reasons)

    def test_java_callback_census_counts_bootstrap_state(self):
        def census(mod_jar):
            game = self.fixture.root / f"{self._testMethodName}-{mod_jar is not None}"
            (game / "mods").mkdir(parents=True, exist_ok=True)
            if mod_jar is not None:
                shutil.copy(mod_jar, game / "mods" / mod_jar.name)
            result = self.run_java("net.mcspine.neoforge.BoundaryChecks",
                                   "callbacks", game)
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            return int(result.stdout.strip().rsplit("callbacks=", 1)[1])

        self.assertEqual(census(None), 0)
        self.assertGreater(census(self.fixture.mod_jar), 0)

    def jni_fixture(self, name, mod_jar):
        directory = self.fixture.root / name
        (directory / "mods").mkdir(parents=True, exist_ok=True)
        if mod_jar is not None:
            shutil.copy(mod_jar, directory / "mods" / mod_jar.name)
        (directory / "classpath.txt").write_text(self.fixture.cp)
        return directory

    def run_jni(self, test, extra_env):
        env = dict(self.fixture.env)
        env.update(extra_env)
        result = subprocess.run(
            ["cargo", "test", "--offline", "--locked", "-j1",
             "--features", "neoforge-events", test,
             "--", "--exact", "--nocapture", "--test-threads=1"],
            cwd=ROOT, env=env, capture_output=True, text=True, timeout=600)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 passed", result.stdout)
        return result.stdout

    def test_jni_10k_localref_exception_and_thread_attach_stress(self):
        fixture = self.jni_fixture("jni-10k-fixture", self.fixture.mod_jar)
        stdout = self.run_jni(
            "java_compat::tests::jni_contract_10k_localref_exception_and_thread_attach_stress",
            {"MC_SPINE_JNI_FIXTURE": str(fixture)})
        self.assertIn("PASS 10000 local-frame dispatches", stdout)

    def test_jni_callback_census_matches_fixture_mods(self):
        for tag, mod_jar, expected in (
            ("empty", None, "0"),
            ("populated", self.fixture.mod_jar, "1"),
        ):
            with self.subTest(mods=tag):
                fixture = self.jni_fixture(f"jni-census-{tag}", mod_jar)
                self.run_jni(
                    "java_compat::tests::jni_contract_callback_census_matches_fixture",
                    {"MC_SPINE_JNI_FIXTURE": str(fixture),
                     "MC_SPINE_JNI_EXPECT_CALLBACKS": expected})


if __name__ == "__main__":
    unittest.main()
