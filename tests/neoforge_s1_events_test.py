"""NF-2 S1 events, bytecode surface and fail-closed scoped mutation integration."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat/neoforge-events"
JAVA = Path("/usr/lib/jvm/java-21-openjdk/bin")
MOD_TOML = '''modLoader="javafml"
loaderVersion="[1,)"
license="MIT"
[[mods]]
modId="s1fixture"
version="1.0.0"
displayName="S1 test"
'''

MOD = """package nf2;
import net.minecraft.network.chat.Component;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.fml.common.Mod;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent;
import net.neoforged.neoforge.event.entity.player.PlayerEvent;
import net.neoforged.neoforge.event.ServerChatEvent;
import net.neoforged.neoforge.event.RegisterCommandsEvent;
import net.minecraft.commands.Commands;
import net.mcspine.neoforge.ServerHooks;

@Mod("s1fixture")
public class FixtureMod {
  public static int breakCount, placeCount, logoutCount, farmlandCount;
  public FixtureMod() { NeoForge.EVENT_BUS.register(this); }

  @SubscribeEvent public void onBreak(BlockEvent.BreakEvent event) {
    breakCount++;
    if (event.getPos().x() == 100) {
      event.getPlayer().sendSystemMessage(Component.literal("reverted break"));
      event.setCanceled(true);
      return;
    }
    if (((Level) event.getLevel()).getBlockState(event.getPos()).stateId() != 11)
      throw new AssertionError("world snapshot");
    event.setExpToDrop(7);
    if (event.getExpToDrop() != 7) throw new AssertionError("break experience");
    event.getPlayer().sendSystemMessage(Component.literal("broke"));
    ((Level) event.getLevel()).setBlock(event.getPos(), new BlockState(88), 3);
  }

  @SubscribeEvent public void onPlace(BlockEvent.EntityPlaceEvent event) {
    placeCount++;
    if (event.getPlacedBlock().stateId() != 65 || event.getPlacedAgainst().stateId() != 9
        || event.getState().stateId() != 65 || event.getLevel() == null)
      throw new AssertionError("place facade");
    if (event.getPos().x() == 200) {
      ((net.minecraft.world.entity.player.Player)event.getEntity())
        .sendSystemMessage(Component.literal("reverted place"));
      event.setCanceled(true);
      return;
    }
    ((Level) event.getLevel()).setBlock(event.getPos(), new BlockState(65), 3);
  }

  @SubscribeEvent public void onLogout(PlayerEvent.PlayerLoggedOutEvent event) {
    logoutCount++;
    event.getEntity().sendSystemMessage(Component.literal("bye"));
  }

  @SubscribeEvent public void onFarmland(BlockEvent.FarmlandTrampleEvent event) {
    farmlandCount++;
    if (event.getPos().x() == 99) event.setCanceled(true);
  }

  @SubscribeEvent public void onChat(ServerChatEvent event) {
    if (!event.getUsername().equals("Alex")) throw new AssertionError("username");
    event.getPlayer().sendSystemMessage(Component.literal("chat fired"));
    if (event.getRawText().equals("reject") || event.getRawText().equals("inner")) {
      event.setCanceled(true);
    } else if (event.getRawText().equals("hello")) {
      event.setMessage(Component.literal("HELLO"));
    } else if (event.getRawText().equals("outer")) {
      ServerHooks.onServerChat(event.getPlayer(), "inner");
      event.setMessage(Component.literal("outer rewritten"));
    } else if (event.getRawText().equals("loop")) {
      ServerHooks.onServerChat(event.getPlayer(), "loop");
    }
  }

  @SubscribeEvent public void onCommands(RegisterCommandsEvent event) {
    if (event.getCommandSelection() != Commands.CommandSelection.DEDICATED
        || event.getBuildContext() == null) throw new AssertionError("command context");
    if (!event.getDispatcher().register(Commands.literal("greet")).getLiteral().equals("greet"))
      throw new AssertionError("command literal");
  }
}
"""

SUBSCRIBER = """package nf2;
import net.minecraft.network.chat.Component;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.neoforge.event.entity.player.PlayerEvent;

@EventBusSubscriber(modid="s1fixture")
public class LoginSubscriber {
  public static int logins;
  @SubscribeEvent public static void onLogin(PlayerEvent.PlayerLoggedInEvent event) {
    logins++;
    event.getEntity().sendSystemMessage(Component.literal("welcome"));
  }
}
"""

UNKNOWN = """package nf2;
@net.neoforged.fml.common.Mod("s1fixture")
public class UnsupportedMod {
  public void unsupported() { com.mojang.brigadier.unknown.Unavailable.invoke(); }
}
"""

UNKNOWN_CLASS = """package com.mojang.brigadier.unknown;
public class Unavailable { public static void invoke() {} }
"""

RUNNER = """package net.mcspine.neoforge;
import java.util.List;
import com.mojang.brigadier.CommandDispatcher;
import net.minecraft.core.BlockPos;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
import nf2.FixtureMod;
import nf2.LoginSubscriber;

public class S1Checks {
  static void check(boolean condition, String detail) {
    if (!condition) throw new AssertionError(detail);
  }
  static void batch(ServerPlayer player) {
    Level level = new Level(2);
    BlockPos pos = new BlockPos(2, 70, 3);
    var result = ServerHooks.onBlockBreak(level, pos, new BlockState(11), player, 1);
    check(!result.canceled() && result.mutations().equals(List.of(
      new MutationBuffer.SendSystemMessage(42, "broke"),
      new MutationBuffer.SetBlockIf(2, 2, 70, 3, 88, 11),
      new MutationBuffer.BreakExp(7))), "break batch " + result);
    check(level.getBlockState(pos).stateId() == 11, "snapshot was applied before host CAS");
    check(FixtureMod.breakCount == 1, "break listener");

    result = ServerHooks.onBlockBreak(level, new BlockPos(100, 70, 3), new BlockState(11), player, 1);
    check(result.canceled() && result.mutations().isEmpty(), "cancel break leaked mutations");

    BlockPos placePos = new BlockPos(5, 70, 3);
    result = ServerHooks.onBlockPlace(level, placePos, new BlockState(33),
      new BlockState(65), new BlockState(9), player);
    check(!result.canceled() && result.mutations().equals(List.of(
      new MutationBuffer.SetBlockIf(2, 5, 70, 3, 65, 33))), "place batch " + result);
    check(level.getBlockState(placePos).stateId() == 33, "place snapshot overwritten");
    result = ServerHooks.onBlockPlace(level, new BlockPos(200, 70, 3),
      new BlockState(), new BlockState(65), new BlockState(9), player);
    check(result.canceled() && result.mutations().isEmpty(), "cancel place leaked mutations");
    check(FixtureMod.placeCount == 2, "place listeners");

    result = ServerHooks.onPlayerLoggedIn(player);
    check(!result.canceled() && result.mutations().equals(List.of(
      new MutationBuffer.SendSystemMessage(42, "welcome"))) && LoginSubscriber.logins == 1,
      "login static subscriber " + result);
    result = ServerHooks.onPlayerLoggedOut(player);
    check(result.mutations().equals(List.of(
      new MutationBuffer.SendSystemMessage(42, "bye"))) && FixtureMod.logoutCount == 1,
      "logout instance subscriber " + result);

    result = ServerHooks.onServerChat(player, "hello");
    check(!result.canceled() && result.mutations().equals(List.of(
      new MutationBuffer.SendSystemMessage(42, "chat fired"),
      new MutationBuffer.ChatMessage("HELLO"))), "chat rewrite " + result);
    result = ServerHooks.onServerChat(player, "reject");
    check(result.canceled() && result.mutations().isEmpty(), "chat cancel leaked mutation");
    result = ServerHooks.onServerChat(player, "outer");
    check(!result.canceled() && result.mutations().equals(List.of(
      new MutationBuffer.SendSystemMessage(42, "chat fired"),
      new MutationBuffer.ChatMessage("outer rewritten"))),
      "nested canceled child must preserve parent " + result);

    result = ServerHooks.onRegisterCommands();
    check(result.mutations().equals(List.of(new MutationBuffer.RegisterCommand("greet"))),
      "literal registration " + result);
    var trample = new FarmlandTrampleEvent(level, new BlockPos(99, 70, 3),
      new BlockState(11), 2.0f, player);
    NeoForge.EVENT_BUS.post(trample);
    check(trample.isCanceled() && FixtureMod.farmlandCount == 1, "farmland legacy event");
    System.out.println("PASS S1 six events, subscriber, chat rewrite, scoped cancel, CAS batch, command, farmland");
  }
  static void scopes() {
    MutationBuffer.beginEvent();
    MutationBuffer.append(new MutationBuffer.RegisterCommand("outer"));
    MutationBuffer.beginEvent();
    MutationBuffer.append(new MutationBuffer.RegisterCommand("discarded"));
    MutationBuffer.cancelCurrent();
    MutationBuffer.endEvent();
    check(MutationBuffer.depth() == 1, "nested depth");
    try { MutationBuffer.commitBatch(); throw new AssertionError("commit during scope"); }
    catch (IllegalStateException expected) { }
    MutationBuffer.endEvent();
    check(MutationBuffer.commitBatch().equals(List.of(new MutationBuffer.RegisterCommand("outer"))),
      "child cancellation removed parent");
    MutationBuffer.beginEvent();
    MutationBuffer.beginEvent();
    MutationBuffer.append(new MutationBuffer.ChatMessage("nested"));
    MutationBuffer.endEvent();
    MutationBuffer.cancelCurrent();
    MutationBuffer.endEvent();
    check(MutationBuffer.commitBatch().isEmpty(), "parent cancellation retained nested changes");
    MutationBuffer.beginEvent();
    for (int i = 0; i < 3; i++) MutationBuffer.beginEvent();
    MutationBuffer.append(new MutationBuffer.BreakExp(999));
    try { MutationBuffer.beginEvent(); throw new AssertionError("depth 5 allowed"); }
    catch (IllegalStateException expected) {
      check(expected.getMessage().contains("reentrancy"), "wrong overflow error " + expected);
    }
    for (int i = 0; i < 4; i++) MutationBuffer.endEvent();
    try { MutationBuffer.commitBatch(); throw new AssertionError("failed batch committed"); }
    catch (IllegalStateException expected) { }
    MutationBuffer.discardAll();
    check(MutationBuffer.depth() == 0 && MutationBuffer.commitBatch().isEmpty(),
      "discardAll failed to reset depth and pending");
    System.out.println("PASS scoped mutation rollback and depth 5 fail-closed");
  }
  static void overflow(ServerPlayer player) {
    boolean failedClosed = false;
    try {
      ServerHooks.onServerChat(player, "loop");
    } catch (Throwable error) {
      for (Throwable cause = error; cause != null; cause = cause.getCause()) {
        if (cause instanceof IllegalStateException
            && cause.getMessage().contains("reentrancy depth exceeds 4")) failedClosed = true;
      }
      if (!failedClosed) throw new AssertionError("unexpected failure", error);
    }
    check(failedClosed, "recursive event was accepted");
    check(MutationBuffer.depth() == 0 && MutationBuffer.commitBatch().isEmpty(),
      "overflow leaked pending mutations or scopes");
    check(!ServerHooks.onServerChat(player, "reject").mutations().iterator().hasNext(),
      "next event after overflow not reset");
    System.out.println("PASS real event bus reentrancy depth >4 rollback and subsequent recovery");
  }
  public static void main(String[] args) throws Exception {
    Bootstrap.initialize(args[1]);
    check(Bootstrap.instances().containsKey("s1fixture"), "mod not bootstrapped");
    var player = new ServerPlayer(42, "Alex");
    check(player.getEid() == 42 && player.getName().getString().equals("Alex"), "player facade");
    switch (args[0]) {
      case "batch" -> batch(player);
      case "scopes" -> scopes();
      case "overflow" -> overflow(player);
      default -> throw new AssertionError(args[0]);
    }
    Bootstrap.close();
  }
}
"""


class NeoForgeS1EventsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tmp = ROOT / "target/tmp"
        tmp.mkdir(parents=True, exist_ok=True)
        cls.env = dict(os.environ, TMPDIR=str(tmp),
                       _JAVA_OPTIONS=f"-Djava.io.tmpdir={tmp} -XX:-UsePerfData")
        cls.workspace = tempfile.TemporaryDirectory(prefix="neoforge-nf2-", dir=tmp)
        cls.root = Path(cls.workspace.name)
        if not (ADAPTER / ".build/classpath.txt").is_file():
            subprocess.run([sys.executable, str(ADAPTER / "build.py"),
                            "--java-home", str(JAVA.parent)], cwd=ROOT, env=cls.env,
                           capture_output=True, check=True)
        cls.cp = (ADAPTER / ".build/classpath.txt").read_text().strip()
        sources = {
            "nf2/FixtureMod.java": MOD,
            "nf2/LoginSubscriber.java": SUBSCRIBER,
            "nf2/UnsupportedMod.java": UNKNOWN,
            "com/mojang/brigadier/unknown/Unavailable.java": UNKNOWN_CLASS,
            "net/mcspine/neoforge/S1Checks.java": RUNNER,
        }
        src = cls.root / "src"
        for relative, content in sources.items():
            path = src / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        cls.classes = cls.root / "classes"
        cls.classes.mkdir()
        compiled = subprocess.run(
            [str(JAVA / "javac"), "-J-Xmx128m", "--release", "21", "-proc:none",
             "-cp", cls.cp, "-d", str(cls.classes),
             *map(str, sorted(src.rglob("*.java")))],
            cwd=ROOT, env=cls.env, capture_output=True, text=True,
        )
        if compiled.returncode:
            raise AssertionError(compiled.stderr + compiled.stdout)

        cls.mod_dir = cls.root / "game/mods"
        cls.mod_dir.mkdir(parents=True)
        cls.mod_jar = cls.mod_dir / "s1fixture.jar"
        cls.bad_jar = cls.root / "unsupported.jar"
        for destination, names in ((cls.mod_jar, ("nf2/FixtureMod", "nf2/LoginSubscriber")),
                                   (cls.bad_jar, ("nf2/UnsupportedMod",))):
            with zipfile.ZipFile(destination, "w") as jar:
                jar.writestr("META-INF/neoforge.mods.toml", MOD_TOML)
                for name in names:
                    jar.write(cls.classes / f"{name}.class", f"{name}.class")

    @classmethod
    def tearDownClass(cls):
        cls.workspace.cleanup()

    def run_java(self, main, *args):
        return subprocess.run([str(JAVA / "java"), "-Djava.io.tmpdir=" + str(ROOT / "target/tmp"),
                               "-cp", os.pathsep.join((str(self.classes), self.cp)),
                               main, *map(str, args)],
                              cwd=ROOT, env=self.env, capture_output=True, text=True, timeout=30)

    def test_shimcheck_accepts_s1_event_mod(self):
        result = self.run_java("net.mcspine.neoforge.ShimCheck", "--json",
                               "--surface", ADAPTER / "shim-surface.json", self.mod_jar)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        data = json.loads(result.stdout)
        self.assertEqual(data["status"], "COMPATIBLE")
        self.assertEqual(data["jars"][0]["coverage"]["unsupported"], 0)

    def test_s1_rejects_unsupported_brigadier_class(self):
        result = self.run_java("net.mcspine.neoforge.ShimCheck", "--json",
                               "--surface", ADAPTER / "shim-surface.json", self.bad_jar)
        self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
        self.assertIn("com/mojang/brigadier/unknown/Unavailable", result.stdout)
        self.assertIn("UNSUPPORTED_SYMBOL", result.stdout)

    def test_six_events_bootstrap_and_batch(self):
        result = self.run_java("net.mcspine.neoforge.S1Checks", "batch", self.mod_dir.parent)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("PASS S1 six events", result.stdout)

    def test_mutation_scopes_and_overflow(self):
        result = self.run_java("net.mcspine.neoforge.S1Checks", "scopes", self.mod_dir.parent)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("PASS scoped mutation rollback", result.stdout)

    def test_actual_event_bus_recursive_dispatch_fail_closed(self):
        result = self.run_java("net.mcspine.neoforge.S1Checks", "overflow", self.mod_dir.parent)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("PASS real event bus reentrancy", result.stdout)


if __name__ == "__main__":
    unittest.main()
