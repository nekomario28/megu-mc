"""NF-1 integration tests against the pinned NeoForge 1.21.1 and EventBus 8.0.2."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat/neoforge-events"
sys.path.insert(0, str(ROOT / "tools"))
from java_toolchain import require_java_home, tool_path

JAVA = require_java_home()
TOML = '''modLoader="javafml"
loaderVersion="[1,)"
license="MIT"
[[mods]]
modId="{id}"
version="1.0.0"
displayName="NF-1 test {id}"
{dependencies}
'''

RUNNER = '''package net.megu.neoforge;
import java.util.Map;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.neoforge.common.NeoForge;

public final class BootstrapChecks {
  static void require(boolean value, String message) {
    if (!value) throw new AssertionError(message);
  }
  public static void main(String[] args) throws Exception {
    if (args[0].equals("reject")) {
      try {
        Bootstrap.initialize(args[1]);
        throw new AssertionError("Accepted invalid mod directory");
      } catch (IllegalStateException expected) {
        require(expected.getMessage().contains("ShimCheck rejected NeoForge mod"), expected.toString());
        require(expected.getMessage().contains(args[2]), expected.toString());
        require(Bootstrap.instance == null && Bootstrap.instances().isEmpty(), "Loaded a mod before rejection");
      }
      System.out.println("PASS fail-closed " + args[2]);
      return;
    }
    if (args[0].equals("bad-constructor")) {
      try {
        Bootstrap.initialize(args[1]);
        throw new AssertionError("Accepted unsupported constructor");
      } catch (IllegalStateException expected) {
        require(expected.getMessage().contains("supported constructor"), expected.toString());
      }
      System.out.println("PASS unsupported constructor rejected");
      return;
    }
    Bootstrap.initialize(args[1]);
    if (args[0].equals("baseline")) {
      require(Bootstrap.instance == null && Bootstrap.instances().isEmpty(), "Baseline created mod");
      require(!NeoForge.EVENT_BUS.post(EventChecks.event()).isCanceled(), "Baseline canceled event");
    } else if (args[0].equals("multi")) {
      Map<String, Object> loaded = Bootstrap.instances();
      require(loaded.size() == 2 && loaded.containsKey("seedprotect") && loaded.containsKey("secondmod"), loaded.toString());
      require(Bootstrap.instance == loaded.get("seedprotect"), "First instance changed");
      Class<?> second = loaded.get("secondmod").getClass();
      require(second.getField("injectedBus").get(null) == Bootstrap.modBus("secondmod"), "Wrong mod bus");
      require(second.getField("injectedDist").get(null) == Dist.DEDICATED_SERVER, "Wrong dist");
      require(Bootstrap.modBus("seedprotect") != Bootstrap.modBus("secondmod"), "Shared mod buses");
      require(Bootstrap.modBus("secondmod") != NeoForge.EVENT_BUS, "Mod bus is game bus");
      require(NeoForge.EVENT_BUS.post(EventChecks.event()).isCanceled(), "Seed Protect was not registered");
      require(second.getField("fires").getInt(null) == 1, "Second @Mod listener did not fire");
      Class<?> subscriber = second.getClassLoader().loadClass("nf1.GameSubscriber");
      require(subscriber.getField("fires").getInt(null) == 1, "GAME subscriber did not fire");
      Class<?> defaultSubscriber = second.getClassLoader().loadClass("nf1.DefaultGameSubscriber");
      require(defaultSubscriber.getField("fires").getInt(null) == 1, "Default GAME bus subscriber did not fire");
      Class<?> modEvent = second.getClassLoader().loadClass("nf1.ModEvent");
      Bootstrap.modBus("secondmod").post((net.neoforged.bus.api.Event) modEvent.getConstructor().newInstance());
      Class<?> modSubscriber = second.getClassLoader().loadClass("nf1.ModSubscriber");
      require(modSubscriber.getField("fires").getInt(null) == 1, "MOD subscriber did not fire");
      Bootstrap.close();
      require(!NeoForge.EVENT_BUS.post(EventChecks.event()).isCanceled(), "close did not unregister mods");
      require(Bootstrap.instances().isEmpty() && Bootstrap.modBus("secondmod") == null, "close retained state");
      System.out.println("PASS two mods, per-mod injection, GAME/MOD subscribers, client exclusion, close");
      return;
    } else if (args[0].equals("constructors")) {
      require(Bootstrap.instances().size() == 4, "Wrong number of mod constructors");
      for (Object instance : Bootstrap.instances().values()) {
        require(instance.getClass().getField("ok").getBoolean(null), instance.getClass().getName());
      }
    } else {
      throw new IllegalArgumentException("Unknown mode " + args[0]);
    }
    System.out.println("PASS " + args[0]);
  }
}
'''

SOURCES = {
    "nf1/SecondMod.java": '''package nf1;
import net.neoforged.fml.common.Mod;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
@Mod("secondmod") public class SecondMod {
  public static IEventBus injectedBus;
  public static Dist injectedDist;
  public static int fires;
  public SecondMod(IEventBus bus, Dist dist) {
    injectedBus = bus; injectedDist = dist;
    NeoForge.EVENT_BUS.register(this);
  }
  @SubscribeEvent(receiveCanceled=true) public void on(FarmlandTrampleEvent event) {
    fires++; event.setCanceled(true);
  }
}
''',
    "nf1/GameSubscriber.java": '''package nf1;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
@EventBusSubscriber(bus=EventBusSubscriber.Bus.GAME, value=Dist.DEDICATED_SERVER, modid="secondmod")
public class GameSubscriber {
  public static int fires;
  @SubscribeEvent(receiveCanceled=true) public static void on(FarmlandTrampleEvent event) { fires++; }
}
''',
    "nf1/ModEvent.java": '''package nf1;
public class ModEvent extends net.neoforged.bus.api.Event { }
''',
    "nf1/DefaultGameSubscriber.java": '''package nf1;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
@EventBusSubscriber(modid="secondmod")
public class DefaultGameSubscriber {
  public static int fires;
  @SubscribeEvent(receiveCanceled=true) public static void on(FarmlandTrampleEvent event) { fires++; }
}
''',
    "nf1/ModSubscriber.java": '''package nf1;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.bus.api.SubscribeEvent;
@EventBusSubscriber(bus=EventBusSubscriber.Bus.MOD, modid="secondmod")
public class ModSubscriber {
  public static int fires;
  @SubscribeEvent public static void on(ModEvent event) { fires++; }
}
''',
    "nf1/ClientSubscriber.java": '''package nf1;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.api.distmarker.Dist;
@EventBusSubscriber(value=Dist.CLIENT, modid="secondmod")
public class ClientSubscriber {
  static { if (System.nanoTime() >= 0) throw new AssertionError("CLIENT subscriber loaded"); }
}
''',
    "nf1/ClientEntrypoint.java": '''package nf1;
import net.neoforged.fml.common.Mod;
import net.neoforged.api.distmarker.Dist;
@Mod(value="secondmod", dist=Dist.CLIENT)
public class ClientEntrypoint {
  static { if (System.nanoTime() >= 0) throw new AssertionError("CLIENT @Mod loaded"); }
}
''',
    "nf1/UnsupportedMod.java": '''package nf1;
@net.neoforged.fml.common.Mod("unsupported") public class UnsupportedMod {
  static { if (System.nanoTime() >= 0) throw new AssertionError("Unsafe mod initialized"); }
  public void unsupported() { net.minecraft.unsupported.Unavailable.call(); }
}
''',
    "net/minecraft/unsupported/Unavailable.java": '''package net.minecraft.unsupported;
public class Unavailable { public static void call() { } }
''',
    "nf1/BadConstructor.java": '''package nf1;
@net.neoforged.fml.common.Mod("badctor") public class BadConstructor {
  public BadConstructor(String unsupported) { }
}
''',
    "nf1/EmptyCtor.java": '''package nf1;
@net.neoforged.fml.common.Mod("emptyctor") public class EmptyCtor {
  public static boolean ok;
  public EmptyCtor() { ok = true; }
}
''',
    "nf1/BusCtor.java": '''package nf1;
import net.neoforged.bus.api.IEventBus;
@net.neoforged.fml.common.Mod("busctor") public class BusCtor {
  public static boolean ok;
  public BusCtor(IEventBus bus) { ok = bus == net.megu.neoforge.Bootstrap.modBus("busctor"); }
}
''',
    "nf1/DistCtor.java": '''package nf1;
import net.neoforged.api.distmarker.Dist;
@net.neoforged.fml.common.Mod("distctor") public class DistCtor {
  public static boolean ok;
  public DistCtor(Dist dist) { ok = dist == Dist.DEDICATED_SERVER; }
}
''',
    "nf1/ReversedCtor.java": '''package nf1;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.bus.api.IEventBus;
@net.neoforged.fml.common.Mod("reversector") public class ReversedCtor {
  public static boolean ok;
  public ReversedCtor(Dist dist, IEventBus bus) {
    ok = dist == Dist.DEDICATED_SERVER && bus == net.megu.neoforge.Bootstrap.modBus("reversector");
  }
}
''',
}


class NeoForgeBootstrapTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tmp = ROOT / "target/tmp"
        tmp.mkdir(parents=True, exist_ok=True)
        cls.env = dict(os.environ, TMPDIR=str(tmp),
                       _JAVA_OPTIONS=f"-Djava.io.tmpdir={tmp} -XX:-UsePerfData")
        cls.workspace = tempfile.TemporaryDirectory(prefix="neoforge-nf1-", dir=tmp)
        cls.root = Path(cls.workspace.name)
        if not (ADAPTER / ".build/classpath.txt").exists():
            subprocess.run([sys.executable, str(ADAPTER / "build.py"), "--java-home", str(JAVA)],
                           cwd=ROOT, env=cls.env, check=True, capture_output=True)
        cls.cp = (ADAPTER / ".build/classpath.txt").read_text().strip()
        src = cls.root / "src"
        for name, source in {**SOURCES, "net/megu/neoforge/BootstrapChecks.java": RUNNER}.items():
            path = src / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source)
        cls.classes = cls.root / "classes"
        cls.classes.mkdir()
        subprocess.run([str(tool_path(JAVA, "javac")), "-J-Xmx128m", "--release", "21", "-proc:none",
                        "-cp", cls.cp, "-d", str(cls.classes),
                        *map(str, sorted(src.rglob("*.java")))],
                       cwd=ROOT, env=cls.env, capture_output=True, text=True, check=True)
        cls.seed = ADAPTER / ".cache/seedprotect-1.0.0.jar"

    @classmethod
    def tearDownClass(cls):
        cls.workspace.cleanup()

    def mods(self, *mods, invalid_file=None):
        directory = self.root / self._testMethodName
        (directory / "mods").mkdir(parents=True, exist_ok=True)
        for filename, modid, names, dependencies in mods:
            if filename == "a-seedprotect.jar":
                (directory / "mods" / filename).write_bytes(self.seed.read_bytes())
                continue
            with zipfile.ZipFile(directory / "mods" / filename, "w") as archive:
                if modid is not None:
                    archive.writestr("META-INF/neoforge.mods.toml",
                                     TOML.format(id=modid, dependencies=dependencies))
                for name in names:
                    archive.write(self.classes / (name + ".class"), name + ".class")
        if invalid_file:
            (directory / "mods" / invalid_file).write_text("not a valid mod")
        return directory

    def run_java(self, name, *args):
        result = subprocess.run([str(tool_path(JAVA, "java")), "-cp", os.pathsep.join((str(self.classes), self.cp)),
                                 name, *map(str, args)], cwd=self.root, env=self.env,
                                text=True, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, (result.stdout, result.stderr))
        return result.stdout

    def test_seed_protect_original_event_contract(self):
        directory = self.mods(("a-seedprotect.jar", "", (), ""))
        self.assertIn("PASS original mod registration", self.run_java("net.megu.neoforge.EventChecks", directory))

    def test_two_mods_subscribers_and_injection(self):
        directory = self.mods(("a-seedprotect.jar", "", (), ""),
                              ("z-second.jar", "secondmod", ("nf1/SecondMod", "nf1/GameSubscriber",
                              "nf1/DefaultGameSubscriber", "nf1/ModSubscriber", "nf1/ModEvent",
                              "nf1/ClientSubscriber", "nf1/ClientEntrypoint"), ""))
        self.assertIn("PASS two mods", self.run_java("net.megu.neoforge.BootstrapChecks", "multi", directory))

    def test_baseline_empty_directory(self):
        self.assertIn("PASS baseline", self.run_java("net.megu.neoforge.BootstrapChecks", "baseline",
                                                     self.mods()))

    def test_all_supported_constructor_shapes(self):
        names = (("emptyctor", "EmptyCtor"), ("busctor", "BusCtor"),
                 ("distctor", "DistCtor"), ("reversector", "ReversedCtor"))
        directory = self.mods(*[(f"{index}-{modid}.jar", modid, ("nf1/" + name,), "")
                                for index, (modid, name) in enumerate(names)])
        self.assertIn("PASS constructors", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                         "constructors", directory))

    def test_unsupported_constructor_fails_closed(self):
        directory = self.mods(("bad.jar", "badctor", ("nf1/BadConstructor",), ""))
        self.assertIn("PASS unsupported constructor", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                                     "bad-constructor", directory))

    def test_unknown_api_rejected_before_any_class_is_loaded(self):
        directory = self.mods(("a-seedprotect.jar", "", (), ""),
                              ("z-bad.jar", "unsupported", ("nf1/UnsupportedMod",), ""))
        self.assertIn("UNSUPPORTED_SYMBOL", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                           "reject", directory, "UNSUPPORTED_SYMBOL"))

    def test_missing_metadata_fails_closed(self):
        directory = self.mods(("bad.jar", None, ("nf1/SecondMod",), ""))
        self.assertIn("MISSING_METADATA", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                        "reject", directory, "MISSING_METADATA"))

    def test_bad_nonjar_entry_fails_closed(self):
        directory = self.mods(invalid_file="README.txt")
        self.assertIn("INVALID_MOD_ENTRY", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                         "reject", directory, "INVALID_MOD_ENTRY"))

    def test_subdirectory_entry_fails_closed(self):
        directory = self.mods()
        (directory / "mods" / "nested.jar").mkdir()
        self.assertIn("INVALID_MOD_ENTRY", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                         "reject", directory, "INVALID_MOD_ENTRY"))

    def test_malformed_jar_fails_closed(self):
        directory = self.mods(invalid_file="corrupt.jar")
        self.assertIn("INVALID_JAR", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                   "reject", directory, "INVALID_JAR"))

    def test_duplicate_modid_fails_closed(self):
        directory = self.mods(("a.jar", "secondmod", ("nf1/SecondMod",), ""),
                              ("b.jar", "secondmod", ("nf1/SecondMod",), ""))
        self.assertIn("DUPLICATE_MOD_ID", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                         "reject", directory, "DUPLICATE_MOD_ID"))

    def test_missing_required_intermod_dependency_fails_closed(self):
        dependency = '''[[dependencies.secondmod]]
modId="absentmod"
type="required"
versionRange="[1,)"
side="BOTH"
'''
        directory = self.mods(("a.jar", "secondmod", ("nf1/SecondMod",), dependency))
        self.assertIn("MISSING_DEPENDENCY", self.run_java("net.megu.neoforge.BootstrapChecks",
                                                           "reject", directory, "MISSING_DEPENDENCY"))


if __name__ == "__main__":
    unittest.main()
