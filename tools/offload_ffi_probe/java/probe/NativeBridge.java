package probe;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.zip.CRC32;

final class NativeBridge {
    static {
        System.loadLibrary("offload_ffi_probe");
    }

    static native int transfer(ByteBuffer frame, int length);
    static native long create(int dimension, int x, int z);
    static native int enqueueDelta(long handle, int kind, int source, ByteBuffer frame, int length);
    private static native long stepInto(long handle, long tick, ByteBuffer output);
    static native int destroy(long handle);

    static native long createSettled();
    static native long createFactory(int dormant, int active);
    static native int factoryPhase(long handle, int phase);
    static native long settledKernelNanos(long handle, long firstTick, int ticks);

    static final int CAPACITY = 64 * 1024;
    static final int HEADER = 24;
    static final int S5_CAPACITY = 256 * 1024;
    static final long BACKLOG = 1L << 32;
    private static final int MAGIC = 0x3546464f;
    private static final Frame[] RING = {new Frame(), new Frame(), new Frame(), new Frame()};
    private static int nextSlot;

    // This single-threaded harness owns each buffer until the synchronous JNI
    // call returns. Native addresses start at byte zero of the supplied view.
    static ByteBuffer direct(int bytes) {
        return ByteBuffer.allocateDirect(bytes).order(ByteOrder.LITTLE_ENDIAN);
    }

    // The low 32 bits are the written prefix length. BACKLOG requests a retry at
    // the same absolute tick after draining output. Errors are negative.
    static long step(long handle, long tick, ByteBuffer output) {
        if (output != null && output.isReadOnly()) {
            return -10;
        }
        return stepInto(handle, tick, output);
    }

    static int transfer(ByteBuffer frame) {
        return transfer(frame, frame == null ? 0 : frame.limit());
    }

    static ByteBuffer topology(int first, int count, long generation) {
        return writeTopology(first, count, generation).finish();
    }

    static Frame writeTopology(int first, int count, long generation) {
        Frame frame = nextFrame();
        ByteBuffer b = frame.buffer;
        for (int i = first; i < first + count; i++) {
            b.putShort((short) i).put((byte) (i % 4)).put((byte) 1).putFloat((i % 31) * 0.125f);
        }
        return frame.header(1, generation, count);
    }

    static ByteBuffer posesAndPaletteDiffs(long tick) {
        return writePosesAndPaletteDiffs(tick).finish();
    }

    static Frame writePosesAndPaletteDiffs(long tick) {
        Frame frame = nextFrame();
        ByteBuffer b = frame.buffer;
        for (int i = 0; i < 4; i++) {
            b.putInt(i).putInt((int) tick);
            b.putDouble(i * 16.0 + tick * 0.05).putDouble(70.0).putDouble(i * -12.0);
            b.putFloat((float) (tick % 360)).putFloat(0.0f);
            b.putFloat(0.2f).putFloat(0.0f).putFloat(-0.1f);
        }
        for (int c = 0; c < 4; c++) {
            for (int d = 0; d < 8; d++) {
                b.putShort((short) (c * 256 + d)).putShort((short) ((c * 17 + d) % 64)).putShort((short) 1);
            }
        }
        return frame.header(2, tick, 4);
    }

    static ByteBuffer logicOutput(long generation) {
        Frame frame = nextFrame();
        frame.buffer.putFloat(1.25f).putFloat(420.0f).putInt(6)
                .putFloat(12.0f).putFloat(70.0f).putFloat(-3.0f);
        return frame.header(3, generation, 1).finish();
    }

    private static Frame nextFrame() {
        Frame frame = RING[nextSlot++ & (RING.length - 1)];
        frame.buffer.clear().position(HEADER);
        return frame;
    }

    static final class Frame {
        private final ByteBuffer buffer = direct(CAPACITY);
        private final ByteBuffer crcView = buffer.duplicate();
        private final CRC32 crc = new CRC32();

        private Frame header(int scenario, long generation, int count) {
            buffer.putInt(0, MAGIC).put(4, (byte) 1).put(5, (byte) scenario).putShort(6, (short) count)
                    .putLong(8, generation).putInt(16, buffer.position() - HEADER);
            return this;
        }

        ByteBuffer finish() {
            int end = buffer.position();
            crcView.limit(end).position(HEADER);
            crc.reset();
            crc.update(crcView);
            return buffer.putInt(20, (int) crc.getValue()).flip();
        }
    }
}
