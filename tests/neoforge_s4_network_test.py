#!/usr/bin/env python3
"""NF-5 smoke tests through ShimCheck, the pinned mod bus and the Rust JNI owner."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat/neoforge-events"
JAVA = Path("/usr/lib/jvm/java-21-openjdk")
METADATA = ('modLoader="javafml"\nloaderVersion="[1,)"\nlicense="MIT"\n'
            '[[mods]]\nmodId="nf5_fixture"\nversion="1.0.0"\ndisplayName="S4 fixture"\n')

MOD = """
package fixture;

import net.minecraft.core.Direction;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.PacketFlow;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.Container;
import net.minecraft.world.level.Level;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.capabilities.Capabilities;
import net.neoforged.neoforge.capabilities.RegisterCapabilitiesEvent;
import net.neoforged.neoforge.items.wrapper.InvWrapper;
import net.neoforged.neoforge.network.PacketDistributor;
import net.neoforged.neoforge.network.event.RegisterPayloadHandlersEvent;
import net.neoforged.neoforge.network.handling.IPayloadContext;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("nf5_fixture")
public final class S4Mod {
    private static final CustomPacketPayload.Type<Message> ECHO = id("echo");
    private static final CustomPacketPayload.Type<Message> IN = id("in");
    private static final CustomPacketPayload.Type<Message> OUT = id("out");
    private static final CustomPacketPayload.Type<Message> UNKNOWN = id("unknown");

    public S4Mod(IEventBus bus) {
        var blocks = DeferredRegister.createBlocks("nf5_fixture");
        var container = blocks.registerSimpleBlock("z_container");
        blocks.registerSimpleBlock("a_other");
        blocks.register(bus);
        var items = DeferredRegister.createItems("nf5_fixture");
        items.registerSimpleItem("z_item");
        items.registerSimpleItem("a_item");
        items.register(bus);
        bus.addListener(RegisterPayloadHandlersEvent.class, S4Mod::registerPayloads);
        bus.addListener(RegisterCapabilitiesEvent.class, event -> {
            event.registerBlock(Capabilities.ItemHandler.BLOCK, (level, pos, state, be, side) -> {
                if (level.getBlockState(pos) != state || state.stateId() != 26685) throw new AssertionError("bad snapshot");
                if (be.getLevel() != level || be.getLevel().dimension() != Level.END) {
                    throw new AssertionError("wrong block entity level or dimension");
                }
                if (side == Direction.DOWN) return null;
                return new InvWrapper((Container) be);
            }, container.get());
            if (!event.isBlockRegistered(Capabilities.ItemHandler.BLOCK, container.get())) {
                throw new AssertionError("missing provider");
            }
        });
    }

    private static CustomPacketPayload.Type<Message> id(String name) {
        return new CustomPacketPayload.Type<>(ResourceLocation.fromNamespaceAndPath("nf5_fixture", name));
    }

    private record Message(CustomPacketPayload.Type<Message> type, int command, int depth, byte[] data)
            implements CustomPacketPayload {
        Message(CustomPacketPayload.Type<Message> type, int command, int depth) {
            this(type, command, depth, new byte[0]);
        }
    }

    private static StreamCodec<RegistryFriendlyByteBuf, Message> codec(CustomPacketPayload.Type<Message> type) {
        return CustomPacketPayload.codec(
            (msg, buf) -> buf.writeVarInt(msg.command()).writeVarInt(msg.depth()).writeByteArray(msg.data()),
            buf -> new Message(type, buf.readVarInt(), buf.readVarInt(), buf.readByteArray()));
    }

    private static void registerPayloads(RegisterPayloadHandlersEvent event) {
        event.registrar("v1")
            .playBidirectional(ECHO, codec(ECHO), S4Mod::handle)
            .versioned("v2")
            .playToServer(IN, codec(IN), S4Mod::handle)
            .playToClient(OUT, codec(OUT), (payload, context) -> { throw new AssertionError("ran S2C handler"); });
    }

    private static void handle(Message input, IPayloadContext context) {
        var player = (ServerPlayer) context.player();
        if (context.flow() != PacketFlow.SERVERBOUND || !player.getName().getString().equals("Rust owner")) {
            throw new AssertionError("wrong S4 context");
        }
        switch (input.command()) {
            case 0 -> context.enqueueWork(() -> {
                PacketDistributor.sendToPlayer(player, new Message(ECHO, 42, input.depth()));
                PacketDistributor.sendToAllPlayers(new Message(OUT, 43, input.depth()));
            }).join();
            case 1 -> {
                context.reply(new Message(ECHO, 40, input.depth()));
                if (input.depth() > 1) {
                    try {
                        context.handle(new Message(IN, 1, input.depth() - 1));
                    } catch (IllegalStateException error) {
                        if (!error.getMessage().contains("depth exceeds 4")) throw error;
                    }
                }
            }
            case 2 -> {
                context.reply(new Message(ECHO, 1, 0));
                try {
                    context.reply(new Message(UNKNOWN, 1, 0));
                } catch (IllegalStateException expected) {
                    if (!expected.getMessage().contains("Unregistered S4 S2C")) throw expected;
                }
            }
            case 3 -> context.reply(new Message(OUT, 1, 0, new byte[32768]));
            case 4 -> {
                for (int i = 0; i < 65; i++) context.reply(new Message(OUT, 1, 0));
            }
            case 5 -> context.reply(new Message(OUT, 5, 0, input.data()));
            case 6 -> context.reply(new Message(IN, 6, 0));
            case 7 -> {
                for (int i = 0; i < 8; i++) context.reply(new Message(OUT, 7, 0, new byte[32763]));
            }
            default -> throw new AssertionError("unexpected command");
        }
    }
}
"""

DIMENSION_CODEC = """
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import net.minecraft.core.registries.Registries;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.Level;

public record DimensionCheck(ResourceKey<Level> dimension) implements CustomPacketPayload {
    private static final Type<DimensionCheck> TYPE = new Type<>(ResourceLocation.parse("nf5_fixture:dimension"));
    private static final StreamCodec<RegistryFriendlyByteBuf, DimensionCheck> CODEC = CustomPacketPayload.codec(
        (msg, buffer) -> buffer.writeResourceLocation(msg.dimension().location()),
        buffer -> new DimensionCheck(buffer.readResourceKey(Registries.DIMENSION)));

    public Type<DimensionCheck> type() { return TYPE; }

    public static void main(String[] args) {
        ResourceKey<?>[] keys = {Level.OVERWORLD, Level.NETHER, Level.END};
        String[] names = {"minecraft:overworld", "minecraft:the_nether", "minecraft:the_end"};
        for (int slot = 0; slot < keys.length; slot++) {
            var key = new Level(slot).dimension();
            var output = new RegistryFriendlyByteBuf(null);
            CODEC.encode(output, new DimensionCheck(key));
            byte[] name = names[slot].getBytes(StandardCharsets.UTF_8);
            byte[] wire = new byte[name.length + 1];
            wire[0] = (byte) name.length;
            System.arraycopy(name, 0, wire, 1, name.length);
            if (key != keys[slot] || !Arrays.equals(output.finish(), wire)) throw new AssertionError("dimension wire or slot");
            var input = new RegistryFriendlyByteBuf(wire);
            if (CODEC.decode(input).dimension() != key) throw new AssertionError("dimension key identity");
            input.finish();
        }
        var invalid = new RegistryFriendlyByteBuf(new byte[] {3, 'b', 'a', 'd'});
        try {
            CODEC.decode(invalid);
            throw new AssertionError("accepted an unknown world key");
        } catch (IllegalArgumentException expected) {
            if (!expected.getMessage().contains("Unsupported S4 dimension")) throw expected;
        }
        try {
            invalid.finish();
            throw new AssertionError("published a failed codec view");
        } catch (IllegalArgumentException expected) {
            if (!expected.getMessage().contains("buffer unavailable")) throw expected;
        }
    }
}
"""

UNSUPPORTED = """
package fixture;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.network.PacketDistributor;
@Mod("nf5_fixture")
public final class Unsupported {
    public Unsupported() { PacketDistributor.sendToServer(null); }
}
"""

INTERNAL = """
package fixture;
import net.mcspine.neoforge.S4NetworkBridge;
import net.neoforged.fml.common.Mod;
@Mod("nf5_fixture")
public final class Internal {
    public Internal() { S4NetworkBridge.send(null, null, null); }
}
"""


class NeoForgeS4NetworkTests(unittest.TestCase):
    @classmethod
    def run_process(cls, *args, extra_env=None):
        return subprocess.run(
            [str(arg) for arg in args], cwd=ROOT, env=cls.env | (extra_env or {}),
            capture_output=True, text=True, timeout=300,
        )

    @classmethod
    def checked(cls, *args, extra_env=None):
        result = cls.run_process(*args, extra_env=extra_env)
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        return result.stdout

    @classmethod
    def setUpClass(cls):
        tmp = ROOT / "target/tmp"
        tmp.mkdir(parents=True, exist_ok=True)
        cls.env = dict(os.environ, TMPDIR=str(tmp), JAVA_HOME=str(JAVA),
                       _JAVA_OPTIONS=f"-Djava.io.tmpdir={tmp} -XX:-UsePerfData")
        workspace = tempfile.TemporaryDirectory(prefix="neoforge-nf5-", dir=tmp)
        cls.addClassCleanup(workspace.cleanup)
        cls.root = Path(workspace.name)
        classpath_file = ADAPTER / ".build/classpath.txt"
        if not classpath_file.is_file():
            cls.checked(sys.executable, ADAPTER / "build.py", "--java-home", JAVA)
        cls.classpath = classpath_file.read_text().strip()
        (cls.root / "classpath.txt").write_text(cls.classpath)
        (cls.root / "mods").mkdir()
        cls.valid = cls.pack_mod("S4Mod", MOD, cls.root / "mods")

    @classmethod
    def pack_mod(cls, name, source, destination, classpath=None):
        src = cls.root / f"{name}.java"
        src.write_text(source)
        compiled = cls.root / name
        cls.checked(JAVA / "bin/javac", "--release", "21", "-proc:none", "-encoding", "UTF-8",
                    "-cp", classpath or cls.classpath, "-d", compiled, src)
        jar = destination / f"{name}.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("META-INF/neoforge.mods.toml", METADATA)
            for path in sorted(compiled.rglob("*.class")):
                archive.write(path, path.relative_to(compiled))
        return jar

    def shimcheck(self, jar):
        return self.run_process(sys.executable, ROOT / "tools/shimcheck.py", "--json", jar)

    def test_payload_and_capability_surface_passes_shimcheck(self):
        result = self.shimcheck(self.valid)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"COMPATIBLE"', result.stdout)

    def test_unsupported_and_bridge_internal_methods_fail_shimcheck(self):
        # Resolve the actual unsupported PacketDistributor method during compilation.
        libraries = self.classpath.split(os.pathsep)
        upstream = next(path for path in libraries if path.endswith("-universal.jar"))
        jar = self.pack_mod("Unsupported", UNSUPPORTED, self.root,
                            os.pathsep.join([upstream, *libraries]))
        result = self.shimcheck(jar)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("PacketDistributor#sendToServer(", result.stdout)
        jar = self.pack_mod("Internal", INTERNAL, self.root)
        result = self.shimcheck(jar)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("S4NetworkBridge#send(", result.stdout)

    def test_dimension_codec_uses_exact_keys_and_rejects_unknown_worlds(self):
        source = self.root / "DimensionCheck.java"
        source.write_text(DIMENSION_CODEC)
        self.checked(JAVA / "bin/javac", "--release", "21", "-cp", self.classpath, "-d", self.root, source)
        self.checked(JAVA / "bin/java", "-cp", os.pathsep.join([str(self.root), self.classpath]), "DimensionCheck")

    def test_rust_jni_payloads_capabilities_and_frozen_registry(self):
        output = self.checked(
            "cargo", "test", "--offline", "--locked", "--features", "fabric-probe,neoforge-events",
            "java_compat::tests::s4_jni_fixture", "--", "--exact", "--nocapture", "--test-threads=1",
            extra_env={"MC_SPINE_S4_FIXTURE": str(self.root)},
        )
        self.assertIn("S4 targeted and broadcast payloads round-tripped", output)
        self.assertIn("S4 invalid, oversized and depth-five payloads rejected", output)
        self.assertIn("S4 capability results and conditional mutations verified", output)
        self.assertIn("S4 deterministic Registry Data packet bodies verified", output)
        self.assertIn("1 passed", output)


if __name__ == "__main__":
    unittest.main(verbosity=2)
