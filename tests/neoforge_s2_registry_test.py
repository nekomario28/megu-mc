"""NF-3 registry tests using synthetic mod JARs and the Rust palette codec."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat/neoforge-events"
sys.path.insert(0, str(ROOT / "tools"))
from java_toolchain import require_java_home, tool_path

JAVA = require_java_home()

MOD_A = """package s2a;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredHolder;
import net.neoforged.neoforge.registries.DeferredRegister;
import net.neoforged.neoforge.registries.RegisterEvent;

@Mod("fixture_a")
@EventBusSubscriber(modid="fixture_a", bus=EventBusSubscriber.Bus.MOD)
public class ModA {
  private static final DeferredRegister<Block> BLOCKS = DeferredRegister.create(Registries.BLOCK, "s2a");
  public static final DeferredHolder<Block, Block> Z = BLOCKS.register("z",
    () -> new Block(BlockBehaviour.Properties.of()));
  private static final DeferredHolder<Block, Block> NESTED = BLOCKS.register("a/one", id -> {
    if (!id.equals(ResourceLocation.parse("s2a:a/one"))) throw new AssertionError(id);
    return new Block(BlockBehaviour.Properties.of());
  });

  public ModA(IEventBus bus) {
    if (Z.isBound() || NESTED.asOptional().isPresent()) throw new AssertionError("eager binding");
    BLOCKS.register(bus);
  }

  @SubscribeEvent public static void extra(RegisterEvent event) {
    event.register(Registries.BLOCK, helper -> helper.register(
      ResourceLocation.fromNamespaceAndPath("s2a", "m"), new Block(BlockBehaviour.Properties.of())));
  }
}
"""

MOD_B = """package s2b;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.item.BlockItem;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredBlock;
import net.neoforged.neoforge.registries.DeferredItem;
import net.neoforged.neoforge.registries.DeferredRegister;
import net.neoforged.neoforge.registries.RegisterEvent;
import s2a.ModA;

@Mod("fixture_b")
public class ModB {
  private static final DeferredRegister.Blocks BLOCKS = DeferredRegister.createBlocks("s2a-b");
  private static final DeferredRegister.Items ITEMS = DeferredRegister.createItems("s2a-b");
  private static final DeferredBlock<Block> SHARED = BLOCKS.registerSimpleBlock("shared");
  private static final DeferredItem<BlockItem> SHARED_ITEM = ITEMS.registerSimpleBlockItem(SHARED);

  public ModB(IEventBus bus) {
    ITEMS.register(bus);
    BLOCKS.register(bus);
    bus.addListener(this::checkItems);
  }

  private void checkItems(RegisterEvent event) {
    if (event.getRegistryKey() != Registries.ITEM) return;
    event.register(Registries.ITEM, ResourceLocation.parse("s2a-b:cross_mod"), () -> {
      if (SHARED_ITEM.get().getBlock() != SHARED.value() || !ModA.Z.isBound()
          || !SHARED.getId().getNamespace().equals("s2a-b")) {
        throw new AssertionError("Block holders must be bound before item registration");
      }
      return new BlockItem(ModA.Z.get(), new Item.Properties());
    });
  }
}
"""

MOD_C = """package s2c;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("fixture_c")
public class ModC {
  public ModC(IEventBus bus) {
    var blocks = DeferredRegister.createBlocks("s2a");
    blocks.registerSimpleBlock("a/zero");
    blocks.register(bus);
    var items = DeferredRegister.createItems("s2a");
    items.registerSimpleItem("a/zero");
    items.register(bus);
  }
}
"""

DUPLICATE = """package duplicate;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.RegisterEvent;

@Mod("fixture_duplicate")
public class Duplicate {
  public Duplicate(IEventBus bus) { bus.addListener(RegisterEvent.class, this::register); }

  private void register(RegisterEvent event) {
    try {
      event.register(Registries.BLOCK, ResourceLocation.parse("s2a:z"),
        () -> new Block(BlockBehaviour.Properties.of()));
    } catch (IllegalStateException ignored) {
      // A mod swallowing a duplicate must still prevent publication.
    }
  }
}
"""

INVALID = """package invalid;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("fixture_invalid")
public class Invalid {
  public Invalid(IEventBus bus) {
    var blocks = DeferredRegister.createBlocks("s2a");
    blocks.registerSimpleBlock("Invalid:Path");
    blocks.register(bus);
  }
}
"""

UNSUPPORTED = """package unsupported;
import net.minecraft.resources.ResourceLocation;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("fixture_unsupported")
public class Unsupported {
  public Unsupported() {
    DeferredRegister.createBlocks("s2a").addAlias(
      ResourceLocation.parse("s2a:old"), ResourceLocation.parse("s2a:new"));
  }
}
"""

RUNNER = """package net.megu.neoforge;
import java.nio.file.Files;
import java.nio.file.Path;

public class S2Checks {
  private static void requireUnfrozen() {
    if (Bootstrap.instance != null || !Bootstrap.instances().isEmpty()) {
      throw new AssertionError("partial mod set visible");
    }
    try {
      Bootstrap.frozenRegistryEncoded();
      throw new AssertionError("partial registry visible");
    } catch (IllegalStateException expected) { }
  }

  public static void main(String[] args) throws Exception {
    requireUnfrozen();
    if (args[0].equals("reject")) {
      try {
        Bootstrap.initialize(args[1]);
        throw new AssertionError("invalid registry initialized");
      } catch (Exception error) {
        Throwable cause = error;
        while (cause != null && !cause.toString().contains(args[2])) cause = cause.getCause();
        if (cause == null) throw error;
      }
      requireUnfrozen();
      Bootstrap.initialize(args[3]);
      if (!Bootstrap.frozenRegistryEncoded().equals("S2\\nend")) {
        throw new AssertionError("retry did not start with an empty registry");
      }
    } else {
      Bootstrap.initialize(args[1]);
      if (!Bootstrap.instance.getClass().getSimpleName().equals(args[2])) {
        throw new AssertionError("unexpected mod construction order");
      }
      Files.writeString(Path.of(args[3]), Bootstrap.frozenRegistryEncoded());
    }
    Bootstrap.close();
    requireUnfrozen();
  }
}
"""

CODEC_RUNNER = """
use landing_types::ModRegistrySnapshot;
use std::{env, fs};

fn main() {
    let args: Vec<_> = env::args().collect();
    let snapshot = ModRegistrySnapshot::from_wire(&fs::read_to_string(&args[2]).unwrap()).unwrap();
    let palette = &args[3];
    match args[1].as_str() {
        "save" => {
            let states: Vec<u32> = args[4..].iter().map(|arg| arg.parse().unwrap()).collect();
            fs::write(palette, snapshot.encode_block_palette(&states).unwrap().join("\\n")).unwrap();
        }
        "load" => {
            let persisted = fs::read_to_string(palette).unwrap();
            let names: Vec<_> = if persisted.is_empty() { Vec::new() }
                else { persisted.split('\\n').collect() };
            match snapshot.decode_block_palette(&names) {
                Ok(states) => println!("{states:?}"),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(2);
                }
            }
        }
        _ => panic!("expected save or load"),
    }
}
"""


class NeoForgeS2RegistryTests(unittest.TestCase):
    @classmethod
    def run_process(cls, *args):
        return subprocess.run(list(map(str, args)), cwd=ROOT, env=cls.env,
                              capture_output=True, text=True, timeout=45)

    @classmethod
    def checked(cls, *args):
        result = cls.run_process(*args)
        if result.returncode:
            raise AssertionError(result.stderr + result.stdout)
        return result.stdout

    @classmethod
    def setUpClass(cls):
        tmp = ROOT / "target/tmp"
        tmp.mkdir(parents=True, exist_ok=True)
        cls.env = dict(os.environ, TMPDIR=str(tmp),
                       _JAVA_OPTIONS=f"-Djava.io.tmpdir={tmp} -XX:-UsePerfData")
        workspace = tempfile.TemporaryDirectory(prefix="neoforge-nf3-", dir=tmp)
        cls.addClassCleanup(workspace.cleanup)
        cls.root = Path(workspace.name)
        if not (ADAPTER / ".build/classpath.txt").is_file():
            cls.checked(sys.executable, ADAPTER / "build.py", "--java-home", JAVA)
        cls.cp = (ADAPTER / ".build/classpath.txt").read_text().strip()
        compiled = cls.root / "compiled"
        fixtures = {
            "fixture_a": ("s2a/ModA", MOD_A),
            "fixture_b": ("s2b/ModB", MOD_B),
            "fixture_c": ("s2c/ModC", MOD_C),
            "fixture_duplicate": ("duplicate/Duplicate", DUPLICATE),
            "fixture_invalid": ("invalid/Invalid", INVALID),
            "fixture_unsupported": ("unsupported/Unsupported", UNSUPPORTED),
        }

        def source(name, content):
            path = cls.root / "src" / f"{name}.java"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
            return path

        # Compile mod API calls against the pinned upstream NeoForge classes.
        # At runtime only the adapter's copy of the registry facade is first.
        upstream = ADAPTER / ".cache/neoforge-21.1.172-universal.jar"
        cls.checked(tool_path(JAVA, "javac"), "--release", "21", "-proc:none", "-cp",
                    os.pathsep.join((str(upstream), cls.cp)), "-d", compiled,
                    source("net/minecraft/world/level/ItemLike", """package net.minecraft.world.level;
public interface ItemLike { net.minecraft.world.item.Item asItem(); }
"""),
                    *(source(name, code) for name, code in fixtures.values()))
        cls.runner = cls.root / "runner"
        cls.checked(tool_path(JAVA, "javac"), "--release", "21", "-proc:none", "-cp", cls.cp,
                    "-d", cls.runner, source("net/megu/neoforge/S2Checks", RUNNER))

        cls.jars = {}
        for mod_id, (name, _) in fixtures.items():
            cls.jars[mod_id] = cls.pack(compiled, mod_id, name)
        cls.mixin_jar = cls.pack(compiled, "fixture_c", "s2c/ModC", mixin=True)

        core_rlib = cls.root / "libmegu_core.rlib"
        cls.checked("rustc", "--edition=2021", "--crate-type", "rlib",
                    "--crate-name", "megu_core", ROOT / "core/src/lib.rs",
                    "-o", core_rlib)
        codec = cls.root / "codec.rs"
        codec.write_text(f'#[path = "{ROOT / "src/landing_types.rs"}"]\n'
                         f'mod landing_types;\n{CODEC_RUNNER}')
        cls.codec = cls.root / "codec"
        cls.checked("rustc", "--edition=2021", codec, "--extern", f"megu_core={core_rlib}",
                    "-o", cls.codec)

    @classmethod
    def pack(cls, compiled, mod_id, name, *, mixin=False):
        staging = cls.root / ("mixin" if mixin else mod_id)
        (staging / "META-INF").mkdir(parents=True)
        metadata = (f'modLoader="javafml"\nloaderVersion="[1,)"\nlicense="MIT"\n'
                    f'[[mods]]\nmodId="{mod_id}"\nversion="1.0.0"\n'
                    f'displayName="{mod_id}"\n')
        if mixin:
            metadata += '[[mixins]]\nconfig="fixture.mixins.json"\n'
            (staging / "fixture.mixins.json").write_text('{"mixins":[]}')
        (staging / "META-INF/neoforge.mods.toml").write_text(metadata)
        target = staging / (name + ".class")
        target.parent.mkdir(parents=True)
        shutil.copyfile(compiled / (name + ".class"), target)
        jar = staging.with_suffix(".jar")
        cls.checked(tool_path(JAVA, "jar"), "--create", "--file", jar, "-C", staging, ".")
        return jar

    def game(self, label, *mods):
        game = self.root / label
        directory = game / "mods"
        directory.mkdir(parents=True)
        for index, mod in enumerate(mods):
            shutil.copyfile(self.jars[mod], directory / f"{index}-{mod}.jar")
        return game

    def run_boot(self, *args):
        return self.checked(tool_path(JAVA, "java"), "-cp", os.pathsep.join((str(self.runner), self.cp)),
                            "net.megu.neoforge.S2Checks", *args)

    def shimcheck(self, *jars):
        return self.run_process(tool_path(JAVA, "java"), "-cp", self.cp, "net.megu.neoforge.ShimCheck",
                                "--json", "--surface", ADAPTER / "shim-surface.json", *jars)

    def test_shimcheck_accepts_s2_mod_jars(self):
        jars = [self.jars[key] for key in ("fixture_a", "fixture_b", "fixture_c")]
        result = self.shimcheck(*jars)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "COMPATIBLE")
        self.assertEqual([jar["coverage"]["unsupported"] for jar in report["jars"]], [0, 0, 0])

    def test_shimcheck_still_rejects_unsupported_registry_methods_and_mixins(self):
        for jar, reason in ((self.jars["fixture_unsupported"], "UNSUPPORTED_SYMBOL"),
                            (self.mixin_jar, "UNSUPPORTED_MIXIN_CONFIG")):
            with self.subTest(reason=reason):
                result = self.shimcheck(jar)
                self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
                self.assertIn(reason, result.stdout)
                if reason == "UNSUPPORTED_SYMBOL":
                    self.assertIn("#addAlias", result.stdout)

    def test_frozen_ids_are_stable_and_saved_names_survive_restart(self):
        old, reordered, added = (self.root / name for name in ("ab.tsv", "ba.tsv", "cba.tsv"))
        self.run_boot("snapshot", self.game("ab", "fixture_a", "fixture_b"), "ModA", old)
        self.run_boot("snapshot", self.game("ba", "fixture_b", "fixture_a"), "ModB", reordered)
        ab, ba = old.read_text(), reordered.read_text()
        self.assertEqual(ab, ba)
        self.assertEqual(ab.splitlines(), [
            "S2",
            "minecraft:block\t26684\ts2a:a/one",
            "minecraft:block\t26685\ts2a:m",
            "minecraft:block\t26686\ts2a:z",
            "minecraft:block\t26687\ts2a-b:shared",
            "minecraft:item\t1333\ts2a-b:cross_mod",
            "minecraft:item\t1334\ts2a-b:shared",
            "end",
        ])
        self.run_boot("snapshot", self.game("cba", "fixture_c", "fixture_b", "fixture_a"), "ModC", added)
        cba = added.read_text()
        self.assertIn("minecraft:block\t26685\ts2a:a/zero", cba)
        self.assertIn("minecraft:item\t1333\ts2a:a/zero", cba)
        self.assertIn("minecraft:item\t1335\ts2a-b:shared", cba)
        palette = self.root / "block.palette"
        self.checked(self.codec, "save", old, palette, 26687, 26684, 26686, 26687)
        self.assertEqual(palette.read_text(), "s2a-b:shared\ns2a:a/one\ns2a:z\ns2a-b:shared")
        self.assertEqual(json.loads(self.checked(self.codec, "load", reordered, palette)),
                         [26687, 26684, 26686, 26687])
        self.assertEqual(json.loads(self.checked(self.codec, "load", added, palette)),
                         [26688, 26684, 26687, 26688])
        for name, error in (("gone:block", "missing dynamic block"),
                            ("s2a:a::one", "invalid dynamic registry name")):
            palette.write_text("s2a:a/one\n" + name)
            result = self.run_process(self.codec, "load", added, palette)
            self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
            self.assertEqual(result.stdout, "")
            self.assertIn(error, result.stderr)

    def test_duplicate_registrations_fail_closed_in_both_orders(self):
        for index, order in enumerate((("fixture_a", "fixture_duplicate"),
                                       ("fixture_duplicate", "fixture_a"))):
            with self.subTest(order=order):
                self.run_boot("reject", self.game(f"duplicate-{index}", *order),
                              "Duplicate registry entry minecraft:block s2a:z",
                              self.game(f"duplicate-retry-{index}"))

    def test_invalid_resource_location_fails_closed(self):
        self.run_boot("reject", self.game("invalid", "fixture_a", "fixture_invalid"),
                      "Invalid resource location: s2a:Invalid:Path", self.game("invalid-retry"))


if __name__ == "__main__":
    unittest.main()
