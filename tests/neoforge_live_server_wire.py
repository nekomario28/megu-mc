#!/usr/bin/env python3
"""Exercise the admitted NeoForge shim through a live protocol-767 server."""

import math
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zipfile

from command_brigadier_ops_wire import CommandClient, rcon_command, server
from container_wire import stack, wire_stack
from protocol767 import ROOT, Reader, position, string, vint
from wire_smoke import port

JAVA = Path("/usr/lib/jvm/java-21-openjdk")
CLASSPATH = ROOT / "compat/neoforge-events/.build/classpath.txt"
MOD_ID = "wire_fixture"
CHANNEL = MOD_ID + ":echo"
COMMAND = "wirefixture"
PROTECTED_STATE = 26684
FIXTURE_ITEM = 1333

MOD = """
package fixture;

import net.minecraft.commands.Commands;
import net.minecraft.core.BlockPos;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.chat.Component;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.mcspine.neoforge.DirectBatchBridge;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.RegisterCommandsEvent;
import net.neoforged.neoforge.event.ServerChatEvent;
import net.neoforged.neoforge.event.entity.player.PlayerEvent;
import net.neoforged.neoforge.event.level.BlockEvent;
import net.neoforged.neoforge.network.PacketDistributor;
import net.neoforged.neoforge.network.event.RegisterPayloadHandlersEvent;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("wire_fixture")
public final class WireMod {
    private static final CustomPacketPayload.Type<Echo> ECHO = new CustomPacketPayload.Type<>(
        ResourceLocation.fromNamespaceAndPath("wire_fixture", "echo"));
    private int logouts;
    private boolean tickSeen;

    public WireMod(IEventBus bus) {
        var blocks = DeferredRegister.createBlocks("wire_fixture");
        blocks.registerSimpleBlock("protected");
        blocks.register(bus);
        var items = DeferredRegister.createItems("wire_fixture");
        items.registerSimpleItem("token");
        items.register(bus);
        bus.addListener(RegisterPayloadHandlersEvent.class, WireMod::payloads);
        NeoForge.EVENT_BUS.register(this);
        DirectBatchBridge.register(DirectBatchBridge.Activation.ENTITY_TICK, 128, context -> {
            if (!tickSeen) {
                tickSeen = true;
                context.sendSystemMessage(context.entity().id(), "fixture S3 player tick");
            }
        });
    }

    @SubscribeEvent public void commands(RegisterCommandsEvent event) {
        event.getDispatcher().register(Commands.literal("wirefixture").executes(context -> {
            var source = context.getSource();
            var pos = source.getPosition();
            source.getLevel().setBlock(new BlockPos(pos.getX(), pos.getY() + 2, pos.getZ()),
                new BlockState(14), 3);
            source.sendSuccess(Component.literal("fixture command at " + pos.getX() + " " + pos.getY()
                + " " + pos.getZ()), false);
            return 1;
        }));
        event.getDispatcher().register(Commands.literal("stop"));
    }

    @SubscribeEvent public void login(PlayerEvent.PlayerLoggedInEvent event) {
        event.getEntity().sendSystemMessage(Component.literal("fixture login " + logouts));
    }

    @SubscribeEvent public void logout(PlayerEvent.PlayerLoggedOutEvent event) {
        logouts++;
    }

    @SubscribeEvent public void breakBlock(BlockEvent.BreakEvent event) {
        if (event.getState().stateId() == 26684) {
            event.getPlayer().sendSystemMessage(Component.literal("fixture canceled break message"));
            event.setCanceled(true);
            return;
        }
        var pos = event.getPos();
        ((Level) event.getLevel()).setBlock(new BlockPos(pos.getX(), pos.getY(), pos.getZ() + 1),
            new BlockState(14), 3);
        event.getPlayer().sendSystemMessage(Component.literal("fixture allowed break"));
    }

    @SubscribeEvent public void placeBlock(BlockEvent.EntityPlaceEvent event) {
        if (event.getPlacedAgainst().stateId() == 26684) event.setCanceled(true);
    }

    @SubscribeEvent public void chat(ServerChatEvent event) {
        if (event.getRawText().equals("fixture-suppress")) {
            event.getPlayer().sendSystemMessage(Component.literal("fixture canceled chat message"));
            event.setCanceled(true);
        } else {
            event.getPlayer().sendSystemMessage(Component.literal("fixture chat side effect"));
            event.setMessage(Component.literal("fixture rewritten chat"));
        }
    }

    private record Echo(int value) implements CustomPacketPayload {
        @Override public Type<Echo> type() { return ECHO; }
    }

    private static void payloads(RegisterPayloadHandlersEvent event) {
        StreamCodec<RegistryFriendlyByteBuf, Echo> codec = CustomPacketPayload.codec(
            (msg, buf) -> buf.writeVarInt(msg.value()), buf -> new Echo(buf.readVarInt()));
        event.registrar("v1").playBidirectional(ECHO, codec, (payload, context) -> {
            context.reply(new Echo(payload.value() + 1));
            PacketDistributor.sendToAllPlayers(new Echo(payload.value() + 2));
        });
    }
}
"""

METADATA = '''modLoader="javafml"
loaderVersion="[1,)"
license="MIT"
[[mods]]
modId="wire_fixture"
version="1.0.0"
displayName="Live wire fixture"
'''


def compile_mod(root):
    source = root / "WireMod.java"
    source.write_text(MOD)
    classes = root / "classes"
    subprocess.run(
        [str(JAVA / "bin/javac"), "--release", "21", "-proc:none", "-cp",
         CLASSPATH.read_text().strip(), "-d", str(classes), str(source)],
        check=True, timeout=30,
    )
    good, rejected = root / "fixture.jar", root / "unadmitted.jar"
    with zipfile.ZipFile(good, "w") as jar, zipfile.ZipFile(rejected, "w") as bad:
        jar.writestr("META-INF/neoforge.mods.toml", METADATA)
        for cls in sorted(classes.rglob("*.class")):
            jar.write(cls, cls.relative_to(classes))
            bad.write(cls, cls.relative_to(classes))
    return good, rejected


def link_inputs(root):
    root.mkdir()
    for name in ("assets", "config_packets.bin"):
        (root / name).symlink_to(ROOT / name, target_is_directory=(ROOT / name).is_dir())


def await_message(client, text):
    if text not in client.messages:
        client.until(lambda *_: text in client.messages, text)


def await_update(client, pos, state, packet=None):
    seen = len(client.updates)
    client.until(lambda pid, _: (packet is None or pid == packet)
                 and (pos, state) in client.updates[seen:], f"block update {pos} = {state}")


def dig(client, pos, sequence):
    client.send(0x24, vint(0) + position(*pos) + b"\x01" + vint(sequence))


def place(client, pos, sequence):
    client.send(0x38, vint(0) + position(*pos) + vint(1)
                + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(sequence))


def payload_value(body):
    reader = Reader(body)
    assert reader.string() == CHANNEL
    value = reader.varint()
    assert reader.pos == len(body)
    return value


def rejected_startup(binary, root, jar):
    link_inputs(root)
    game = port()
    env = dict(os.environ, MC_SPINE_NEOFORGE_MODS=str(jar), MC_SPINE_PORT=str(game),
               MC_SPINE_BIND_ADDR="127.0.0.1", MC_SPINE_WG="old")
    result = subprocess.run([str(binary)], cwd=root, env=env, capture_output=True,
                            text=True, timeout=15)
    assert result.returncode == 1, result.stderr
    assert "ShimCheck rejected NeoForge mod" in result.stderr, result.stderr
    with socket.socket() as connection:
        assert connection.connect_ex(("127.0.0.1", game)) != 0
    assert "listening" not in result.stdout + result.stderr
    assert not (root / "world_overlay.bin").exists()


def exercise(binary, root, jar):
    link_inputs(root)
    os.environ["MC_SPINE_NEOFORGE_MODS"] = str(jar)
    with server(binary, root) as (game, rcon):
        with CommandClient(game, "NeoWire") as player:
            player.login()
            assert player.initial_tree[COMMAND][0] == 1
            assert "stop" not in player.initial_tree, "mod literal must not expose operator command"
            await_message(player, "fixture login 0")
            await_message(player, "fixture S3 player tick")
            assert rcon_command(rcon, COMMAND) == "fixture command at 0 16 0"
            await_update(player, (0, 18, 0), 14)
            rcon_command(rcon, "setblock 0 18 0 air")
            await_update(player, (0, 18, 0), 0)
            x, y, z = [math.floor(axis) for axis in player.pos]
            seen = len(player.updates)
            player.send(0x04, string(COMMAND))
            await_message(player, f"fixture command at {x} {y} {z}")
            if ((x, y + 2, z), 14) not in player.updates[seen:]:
                await_update(player, (x, y + 2, z), 14)
            assert rcon_command(rcon, "gamemode creative NeoWire")

            x, y, z = [math.floor(axis) for axis in player.pos]
            protected = (x + 1, y, z)
            allowed = (x + 2, y, z)
            neighbor = (x + 2, y, z + 1)
            for pos, name, state in ((protected, "wire_fixture:protected", PROTECTED_STATE),
                                     (allowed, "stone", 1)):
                coords = " ".join(map(str, pos))
                assert "Set block at" in rcon_command(rcon, f"setblock {coords} {name}")
                await_update(player, pos, state)

            assert "1 target(s)" in rcon_command(rcon, "give NeoWire wire_fixture:token")
            update = Reader(player.until_id(0x15))
            assert update.number("B") == 0
            update.varint()
            assert update.number(">h") == 36 and stack(update) == (FIXTURE_ITEM, 1)

            dig(player, protected, 41)
            await_update(player, protected, PROTECTED_STATE, 0x09)
            assert "fixture canceled break message" not in player.messages

            dig(player, allowed, 42)
            await_message(player, "fixture allowed break")
            player.until(lambda *_: (allowed, 0) in player.updates
                         and (neighbor, 14) in player.updates, "S1 guarded adjacent mutation")
            assert (protected, 0) not in player.updates

            player.send(0x32, struct.pack(">h", 36) + wire_stack(1, 4))
            player.until_id(0x15)
            place(player, protected, 43)
            await_update(player, (protected[0], protected[1] + 1, protected[2]), 0, 0x09)
            place(player, neighbor, 44)
            await_update(player, (neighbor[0], neighbor[1] + 1, neighbor[2]), 1)

            # The shared headless client decodes chunk palettes against vanilla IDs.
            coords = " ".join(map(str, protected))
            assert "Set block at" in rcon_command(rcon, f"setblock {coords} air")
            await_update(player, protected, 0)

            with CommandClient(game, "NeoPeer") as peer:
                peer.login()
                await_message(peer, "fixture login 0")
                player.chat("fixture-suppress")
                player.chat("fixture-allow")
                await_message(player, "fixture chat side effect")
                await_message(peer, "<NeoWire> fixture rewritten chat")
                assert "fixture canceled chat message" not in player.messages
                assert "<NeoWire> fixture-suppress" not in peer.messages

                player.send(0x12, string(CHANNEL) + vint(70))
                assert payload_value(player.until_id(0x19, "S4 targeted reply")) == 71
                assert payload_value(player.until_id(0x19, "S4 broadcast to sender")) == 72
                assert payload_value(peer.until_id(0x19, "S4 broadcast to peer")) == 72
            # Logout runs before the server removes the connection from the player list.
            wait_for_player_logout(rcon, "NeoPeer")
            with CommandClient(game, "NeoReturn") as returning:
                returning.login()
                await_message(returning, "fixture login 1")


def wait_for_player_logout(rcon, name):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        roster = rcon_command(rcon, "list")
        if name not in roster:
            return
        time.sleep(0.05)
    raise AssertionError(f"logout did not complete for {name}")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target/debug/mc-spine"
    os.environ["JAVA_HOME"] = str(JAVA)
    os.environ.pop("MC_SPINE_FABRIC_CLASSPATH", None)
    os.environ["MC_SPINE_NEOFORGE_CLASSPATH"] = str(CLASSPATH)
    with tempfile.TemporaryDirectory(prefix="spine-neoforge-wire-") as directory:
        root = Path(directory)
        good, rejected = compile_mod(root)
        rejected_startup(binary, root / "rejected", rejected)
        exercise(binary, root / "live", good)
    print("NeoForge live wire PASS: admission, S1, S2 IDs, S3 player tick, S4 targeted/broadcast")
    print("NeoForge commands: Play and RCON handlers deliver feedback and commit block mutations")
