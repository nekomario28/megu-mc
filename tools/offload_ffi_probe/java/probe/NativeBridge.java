package probe;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public final class NativeBridge {
    static { System.loadLibrary("offload_ffi_probe"); }
    static native int transfer(ByteBuffer frame);
    static final int CAPACITY = 64 * 1024, HEADER = 24, MAGIC = 0x3546464f;
    static final ByteBuffer[] RING = new ByteBuffer[4];
    static int nextSlot;
    static { for(int i=0;i<RING.length;i++) RING[i]=ByteBuffer.allocateDirect(CAPACITY).order(ByteOrder.LITTLE_ENDIAN); }

    static ByteBuffer frame(int scenario, long generation, byte[] payload, int count) {
        ByteBuffer b = RING[nextSlot++ & 3].clear().order(ByteOrder.LITTLE_ENDIAN);
        b.putInt(MAGIC).put((byte)1).put((byte)scenario).putShort((short)count).putLong(generation).putInt(payload.length).putInt(crc(payload));
        b.put(payload).flip();
        return b;
    }
    static byte[] topology(int n) {
        ByteBuffer b = ByteBuffer.allocate(n * 8).order(ByteOrder.LITTLE_ENDIAN);
        for (int i=0;i<n;i++) b.putShort((short)i).put((byte)(i % 4)).put((byte)1).putFloat((i % 31) * 0.125f);
        return b.array();
    }
    static byte[] poses(long tick) {
        ByteBuffer b = ByteBuffer.allocate(4 * 52).order(ByteOrder.LITTLE_ENDIAN);
        for (int i=0;i<4;i++) {
            b.putInt(i).putInt((int)tick);
            b.putDouble(i * 16.0 + tick * 0.05).putDouble(70.0).putDouble(i * -12.0);
            b.putFloat((float)(tick % 360)).putFloat(0.0f);
            b.putFloat(0.2f).putFloat(0.0f).putFloat(-0.1f);
        }
        return b.array();
    }
    static byte[] posesAndPaletteDiffs(long tick) {
        byte[] poses=poses(tick);
        ByteBuffer b=ByteBuffer.allocate(poses.length+4*8*6).order(ByteOrder.LITTLE_ENDIAN).put(poses);
        for(int c=0;c<4;c++) for(int d=0;d<8;d++) b.putShort((short)(c*256+d)).putShort((short)((c*17+d)%64)).putShort((short)1);
        return b.array();
    }
    static byte[] logicOutput() {
        return ByteBuffer.allocate(24).order(ByteOrder.LITTLE_ENDIAN).putFloat(1.25f).putFloat(420.0f).putInt(6)
                .putFloat(12.0f).putFloat(70.0f).putFloat(-3.0f).array();
    }
    static int call(int scenario, long gen, byte[] payload, int count) { return transfer(frame(scenario, gen, payload, count)); }
    static int crc(byte[] bytes) {
        int c=~0;
        for(byte v:bytes){ c^=v&255; for(int j=0;j<8;j++) c=(c>>>1)^(0xedb88320 & -(c&1)); }
        return ~c;
    }
    static void require(boolean ok, String message) { if(!ok) throw new AssertionError(message); }
}
