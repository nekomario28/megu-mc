"""S3 acceptance tests using real Bootstrap mod loading and the Rust JNI bridge."""

import os
from pathlib import Path
import struct
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
CAPACITY = 256 * 1024
SEQUENCE = 0x8000_0000_0100_0002
GENERATION = 0x8000_0003

MOD = """
package fixture;
import net.megu.neoforge.DirectBatchBridge;
import net.megu.neoforge.DirectBatchBridge.BlockEntityHandle;
import net.megu.neoforge.DirectBatchBridge.EntityHandle;
import net.megu.neoforge.DirectBatchBridge.TickContext;
import static net.megu.neoforge.DirectBatchBridge.Activation.*;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.LevelAccessor;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.entity.BlockEntityTicker;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("nf4_fixture")
public final class S3Mod {
    private BlockEntityHandle block;
    private BlockEntity previousTick;
    private EntityHandle entity;
    private int depth;

    public S3Mod(IEventBus bus) {
        var blocks = DeferredRegister.createBlocks("nf4_fixture");
        var scheduledBlock = blocks.registerSimpleBlock("scheduled");
        blocks.register(bus);
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 21, context -> {
            var level = context.level();
            var pos = new BlockPos(context.x(), context.y(), context.z());
            if (level.dimension() != Level.END || level.getBlockState(pos).stateId() != 26684) {
                throw new AssertionError("incorrect S3 level snapshot");
            }
            LevelAccessor scheduler = level;
            scheduler.scheduleTick(pos, scheduledBlock.get(), 12);
            if (pos.getX() == -3) {
                requireScheduleRejected(() -> scheduler.scheduleTick(new BlockPos(0, 70, 18), scheduledBlock.get(), 2));
            } else if (pos.getX() == -2) {
                requireScheduleRejected(() -> level.scheduleTick(pos, new Block(BlockBehaviour.Properties.of()), 2));
            }
        });
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 7, context -> {
            var handle = context.blockEntity();
            checkIdentity(context, handle.kind(), handle.id(), handle.generation(), handle.dim());
            context.setBlockIf(context.x(), context.y(), context.z(), context.state() + 1, context.state());
        });
        DirectBatchBridge.register(SCHEDULED_BLOCK_TICK, 7, context ->
            context.scheduleTick(context.x(), context.y(), context.z(), context.x() * 4, context.x() % 7 - 3));
        DirectBatchBridge.register(CONTAINER, 7, context -> {
            var handle = context.blockEntity();
            checkIdentity(context, handle.kind(), handle.id(), handle.generation(), handle.dim());
            context.containerSlotDelta(context.x(), context.y(), context.z(),
                context.x(), 1400 + context.x(), context.x() % 99 + 1);
        });
        DirectBatchBridge.register(ENTITY_TICK, 7, context -> {
            var handle = context.entity();
            checkIdentity(context, handle.kind(), handle.id(), handle.generation(), handle.dim());
            context.sendSystemMessage(handle.id(), "更新 📦 " + context.x());
        });

        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 11, context -> {
            if (context.x() != 0) {
                requireExpired(block::generation);
                return;
            }
            block = context.blockEntity();
            context.scheduleTick(context.x(), context.y(), context.z(), 0, -3);
        });
        DirectBatchBridge.register(ENTITY_TICK, 11, context -> {
            if (context.x() != 0) {
                requireExpired(entity::id);
                return;
            }
            entity = context.entity();
            context.sendSystemMessage(entity.id(), "handle was live");
        });
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 12, this::reenter);
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 13, context -> {
            String message = "ü".repeat(2048);
            for (int i = 0; i < context.x(); i++) context.sendSystemMessage(0, message);
        });
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 14, context -> {
            for (int i = 0; i < context.x(); i++) context.scheduleTick(i, 0, 0, i, 3);
        });
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 15, context -> context.scheduleTick(0, 0, 0, -1, 0));
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 16, context -> context.containerSlotDelta(0, 0, 0, 0, 0, 1));
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 17, context ->
            context.sendSystemMessage(0, String.valueOf((char) 0xd800)));
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 18, context ->
            net.megu.neoforge.ServerHooks.land(0, 0, 0, 0.0f));
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 20, context -> {
            throw new AssertionError("malformed input must fail before any callback runs");
        });
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 22, context -> {
            if (context.blockEntity().dirty()) throw new AssertionError("dirty flag leaked between activations");
        });
        BlockEntityTicker<BlockEntity> ticker = (level, pos, state, be) -> {
            if (be.getLevel() != level || level.dimension() != Level.END
                    || level.getBlockState(pos) != state || state.stateId() != 26684
                    || pos.getY() != 70 || pos.getZ() != 18) {
                throw new AssertionError("incorrect ticker inputs");
            }
            be.setChanged();
            if (pos.getX() == -3) {
                try {
                    previousTick.setChanged();
                    throw new AssertionError("stale block entity accepted setChanged");
                } catch (IllegalStateException expected) {
                    if (!expected.getMessage().contains("handle expired")) throw expected;
                }
            }
            previousTick = be;
        };
        DirectBatchBridge.registerBlockEntityTicker(22, ticker);
        DirectBatchBridge.register(BLOCK_ENTITY_TICK, 22, context -> {
            if (previousTick != context.blockEntity() || !context.blockEntity().dirty()) {
                throw new AssertionError("ticker did not mark the active view");
            }
            context.scheduleTick(context.x(), context.y(), context.z(), 12, 0);
        });
    }

    private static void requireScheduleRejected(Runnable schedule) {
        try {
            schedule.run();
            throw new AssertionError("out-of-snapshot schedule accepted");
        } catch (IllegalStateException expected) {
            if (!expected.getMessage().contains("current cell and its registered block")) throw expected;
        }
    }

    private static void checkIdentity(TickContext context, int kind, int id, int generation, int dim) {
        int x = context.x();
        if (kind != 7 || id != x - 512 || generation != 0x80000000 + x || dim != x % 3) {
            throw new AssertionError("S3 handle identity mismatch");
        }
    }

    private static void requireExpired(java.util.function.IntSupplier access) {
        try {
            access.getAsInt();
            throw new AssertionError("expired S3 handle was accepted");
        } catch (IllegalStateException expected) {
            if (!expected.getMessage().contains("handle expired")) throw expected;
            // The bridge must invalidate the batch even if a callback catches this exception.
        }
    }

    private void reenter(TickContext context) {
        depth++;
        try {
            context.scheduleTick(context.x(), context.y(), context.z(), depth, 0);
            if (depth < context.x()) {
                try {
                    context.redispatch();
                } catch (IllegalStateException expected) {
                    if (!expected.getMessage().contains("depth exceeds 4")) throw expected;
                    if (depth != 4) throw new AssertionError("wrong depth limit");
                }
            }
        } finally {
            depth--;
        }
    }
}
"""

BATCH_RUNNER = """
import java.lang.reflect.InvocationTargetException;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import net.megu.neoforge.Bootstrap;
import net.megu.neoforge.DirectBatchBridge;

public class BatchCheck {
    public static void main(String[] args) throws Exception {
        Bootstrap.initialize(args[0]);
        var dispatch = DirectBatchBridge.class.getDeclaredMethod("dispatch", ByteBuffer.class, int.class, ByteBuffer.class);
        dispatch.setAccessible(true);
        var request = ByteBuffer.allocateDirect(256 * 1024);
        var response = ByteBuffer.allocateDirect(256 * 1024);
        byte[] wire = Files.readAllBytes(Path.of(args[1]));
        request.put(wire);
        try {
            int length = (int) dispatch.invoke(null, request, wire.length, response);
            byte[] output = new byte[length];
            response.get(output);
            Files.write(Path.of(args[2]), output);
        } catch (InvocationTargetException error) {
            if (response.getInt(0) != 0) throw new AssertionError("partial S3 response published", error);
            if (error.getCause() instanceof IllegalStateException cause && cause.getMessage().equals("S3 batch failed")) {
                System.out.println("S3 batch rolled back");
            } else {
                throw error;
            }
        } finally {
            Bootstrap.close();
        }
    }
}
"""

DECODER = """
use landing_types::{decode_s3_response_batch, S3Activation, S3TickRequest};

fn main() {
    let input = S3TickRequest {
        activation: S3Activation::BlockEntityTick,
        dim: 2,
        kind: 21,
        id: -20,
        generation: 0x8000_0003,
        pos: (-4, 70, 18),
        state: 26684,
    };
    let path = std::env::args_os().nth(1).unwrap();
    let wire = std::fs::read(path).unwrap();
    match decode_s3_response_batch(&wire, 0x8000_0000_0100_0002, &[input]) {
        Ok(ops) => println!("accepted {}", ops.count()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
}
"""


def frame(*records, count=None, sequence=SEQUENCE, header=b"S3\x01\x01"):
    return struct.pack("<4sIQ", header, len(records) if count is None else count, sequence) + b"".join(records)


def op(tag, payload, *, source=0, generation=GENERATION):
    return struct.pack("<BII", tag, source, generation) + payload


class NeoForgeS3BatchTests(unittest.TestCase):
    @classmethod
    def run_process(cls, *args, extra_env=None):
        return subprocess.run(
            list(map(str, args)), cwd=ROOT, env=cls.env | (extra_env or {}),
            capture_output=True, text=True, timeout=240,
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
        workspace = tempfile.TemporaryDirectory(prefix="neoforge-nf4-", dir=tmp)
        cls.addClassCleanup(workspace.cleanup)
        cls.root = Path(workspace.name)
        classpath_file = ADAPTER / ".build/classpath.txt"
        if not classpath_file.is_file():
            cls.checked(sys.executable, ADAPTER / "build.py", "--java-home", JAVA)
        classpath = classpath_file.read_text()
        (cls.root / "classpath.txt").write_text(classpath)
        (cls.root / "mods").mkdir()
        cls.pack_mod("S3Mod", MOD)
        cls.codec = cls.root / "decode"
        core_rlib = cls.root / "libmegu_core.rlib"
        cls.checked("rustc", "--edition=2021", "--crate-type", "rlib",
                    "--crate-name", "megu_core", ROOT / "core/src/lib.rs",
                    "-o", core_rlib)
        codec = cls.root / "decode.rs"
        codec.write_text(f'#![allow(dead_code)]\n#[path = "{ROOT / "src/landing_types.rs"}"]\n'
                         f'mod landing_types;\n{DECODER}')
        cls.checked("rustc", "--edition=2021", "--cfg", 'feature="neoforge-events"', codec,
                    "--extern", f"megu_core={core_rlib}", "-o", cls.codec)
        source = cls.root / "BatchCheck.java"
        source.write_text(BATCH_RUNNER)
        cls.checked(tool_path(JAVA, "javac"), "--release", "21", "-cp", classpath.strip(), "-d", cls.root, source)

    @classmethod
    def pack_mod(cls, name, source):
        src = cls.root / f"{name}.java"
        src.write_text(source)
        compiled = cls.root / name
        cls.checked(tool_path(JAVA, "javac"), "--release", "21", "-proc:none", "-encoding", "UTF-8",
                    "-cp", (cls.root / "classpath.txt").read_text().strip(), "-d", compiled, src)
        metadata = ('modLoader="javafml"\nloaderVersion="[1,)"\nlicense="MIT"\n'
                    '[[mods]]\nmodId="nf4_fixture"\nversion="1.0.0"\ndisplayName="S3 fixture"\n')
        jar = cls.root / "mods" / f"{name}.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("META-INF/neoforge.mods.toml", metadata)
            for path in compiled.rglob("*.class"):
                archive.write(path, path.relative_to(compiled))
        return jar

    def run_jni(self, mode, marker):
        stdout = self.checked(
            "cargo", "test", "--offline", "--locked", "--features", "fabric-probe,neoforge-events",
            "java_compat::tests::s3_direct_jni_fixture", "--", "--exact", "--nocapture", "--test-threads=1",
            extra_env={"MEGU_S3_FIXTURE": str(self.root), "MEGU_S3_MODE": mode},
        )
        self.assertIn(marker, stdout)
        self.assertIn("1 passed", stdout)

    def test_1024_activations_round_trip_in_one_crossing(self):
        self.run_jni("stress", "validated=1024 jni_crossings=1")

    def test_generation_checked_entity_and_block_entity_invalidation(self):
        self.run_jni("lifetime", "expired entity and block entity handles rolled back")

    def test_depth_five_rolls_back_every_pending_mutation(self):
        self.run_jni("reentrancy", "depth 4 committed and depth 5 rolled back")

    def test_rust_rejects_corrupt_truncated_and_oversized_responses(self):
        pos = struct.pack("<Biii", 2, -4, 70, 18)
        tick = op(2, pos + struct.pack("<ib", 2**31 - 1, -3))
        message = op(4, struct.pack("<iH", -20, 4) + "📦".encode())
        valid = frame(tick, message)
        path = self.root / "response"
        path.write_bytes(valid)
        self.assertEqual(self.checked(self.codec, path).strip(), "accepted 2")
        for length in range(len(valid)):
            with self.subTest(truncated_at=length):
                path.write_bytes(valid[:length])
                result = self.run_process(self.codec, path)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("truncated S3 response", result.stderr)

        bad_cases = {
            "version": (frame(tick, header=b"S3\x02\x01"), "invalid S3 response header"),
            "direction": (frame(tick, header=b"S3\x01\x00"), "invalid S3 response header"),
            "sequence": (frame(tick, sequence=SEQUENCE + 1), "sequence mismatch"),
            "count": (frame(tick, count=4097), "record limit"),
            "short count": (frame(tick, message, count=1), "trailing bytes"),
            "trailing byte": (valid + b"\x00", "trailing bytes"),
            "capacity": (valid.ljust(CAPACITY + 1, b"\0"), "exceeds capacity"),
            "opcode": (frame(tick, op(0xff, b"")), "unknown S3 mutation opcode"),
            "source": (frame(op(2, tick[9:], source=1)), "unknown S3 mutation source"),
            "generation": (frame(op(2, tick[9:], generation=GENERATION + 1)), "generation mismatch"),
            "closed generation": (frame(op(2, tick[9:], generation=0)), "generation mismatch"),
            "dimension": (frame(op(2, bytes([3]) + tick[10:])), "invalid S3 mutation dimension"),
            "other dimension": (frame(op(2, bytes([1]) + tick[10:])), "invalid S3 mutation dimension"),
            "delay": (frame(op(2, pos + struct.pack("<ib", -1, 0))), "tick delay or priority"),
            "priority": (frame(op(2, pos + struct.pack("<ib", 0, 4))), "tick delay or priority"),
            "stack": (frame(op(3, pos + struct.pack("<HIB", 0, 1, 100))), "invalid S3 container stack"),
            "empty stack": (frame(op(3, pos + struct.pack("<HIB", 0, 1, 0))), "invalid S3 container stack"),
            "message length": (frame(op(4, struct.pack("<iH", 0, 4097))), "message exceeds capacity"),
            "invalid UTF-8": (frame(op(4, struct.pack("<iH", 0, 1) + b"\xff")), "invalid S3 message UTF-8"),
        }
        for case, (wire, error) in bad_cases.items():
            with self.subTest(case=case):
                path.write_bytes(wire)
                result = self.run_process(self.codec, path)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn(error, result.stderr)

    def test_java_rejects_malformed_requests_and_rolls_back_at_output_limits(self):
        self.run_jni("bounds", "malformed input and invalid or oversized output rejected")

    def dispatch_batch(self, *records):
        classpath = (self.root / "classpath.txt").read_text().strip()
        request = self.root / "batch-request"
        response = self.root / "batch-response"
        response.unlink(missing_ok=True)
        request.write_bytes(frame(*records, header=b"S3\x01\x00"))
        stdout = self.checked(tool_path(JAVA, "java"), "-cp", os.pathsep.join([str(self.root), classpath]),
                              "BatchCheck", self.root, request, response)
        return stdout, response

    @staticmethod
    def block_entity_record(kind, x, activation=1):
        return struct.pack("<BBHiIiiiI", activation, 2, kind, -20, GENERATION, x, 70, 18, 26684)

    def test_level_schedule_tick_emits_s3_op_and_rolls_back_invalid_calls(self):
        for x in (-4, -3, -2):
            with self.subTest(x=x):
                result, response = self.dispatch_batch(self.block_entity_record(21, x))
                if x == -4:
                    payload = struct.pack("<Biiiib", 2, x, 70, 18, 12, 0)
                    self.assertEqual(response.read_bytes(), frame(op(2, payload)))
                    self.assertEqual(self.checked(self.codec, response).strip(), "accepted 1")
                else:
                    self.assertIn("S3 batch rolled back", result)
                    self.assertFalse(response.exists())

    def test_block_entity_ticker_marks_only_the_active_view(self):
        record = self.block_entity_record(22, -4)
        _, response = self.dispatch_batch(record, record, self.block_entity_record(22, -4, activation=0))
        payload = struct.pack("<Biiiib", 2, -4, 70, 18, 12, 0)
        self.assertEqual(response.read_bytes(), frame(op(2, payload), op(2, payload, source=1)))

        stdout, response = self.dispatch_batch(record, self.block_entity_record(22, -3))
        self.assertIn("S3 batch rolled back", stdout)
        self.assertFalse(response.exists())

    def test_shimcheck_rejects_direct_calls_to_the_legacy_native_host(self):
        jar = self.pack_mod("HostCaller", """
package fixture;
import net.neoforged.fml.common.Mod;
@Mod("nf4_fixture")
public final class HostCaller {
    public HostCaller() { net.megu.probe.Host.getBlock(0, 0, 0); }
}
""")
        self.addCleanup(jar.unlink)
        result = self.run_process(sys.executable, ROOT / "tools/shimcheck.py", "--json", jar)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("net/megu/probe/Host#getBlock(III)I", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
