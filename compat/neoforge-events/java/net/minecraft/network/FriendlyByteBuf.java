package net.minecraft.network;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.StandardCharsets;

/** Bounded, single-use payload codec view. */
public class FriendlyByteBuf {
    public static final int MAX_PAYLOAD = 32 * 1024;
    private final Thread owner = Thread.currentThread();
    private final ByteBuffer input;
    private final ByteArrayOutputStream output;
    private boolean failed;
    private boolean closed;

    protected FriendlyByteBuf(byte[] input) {
        if (input != null && input.length > MAX_PAYLOAD) throw new IllegalArgumentException("Oversized S4 payload");
        this.input = input == null ? null : ByteBuffer.wrap(input);
        output = input == null ? new ByteArrayOutputStream() : null;
    }

    private IllegalArgumentException fail(String message) {
        failed = true;
        return new IllegalArgumentException(message);
    }

    private void checkOpen() {
        if (Thread.currentThread() != owner || closed || failed) throw fail("S4 codec buffer unavailable");
    }

    private void requireRead(int length) {
        checkOpen();
        if (input == null || length < 0 || length > input.remaining()) throw fail("Truncated S4 payload");
    }

    private void requireWrite(int length) {
        checkOpen();
        if (output == null || length > MAX_PAYLOAD - output.size()) throw fail("Oversized S4 payload");
    }

    public int readVarInt() {
        int value = 0;
        for (int shift = 0; shift < 32; shift += 7) {
            requireRead(1);
            int next = Byte.toUnsignedInt(input.get());
            if (shift == 28 && next > 15) throw fail("Invalid S4 VarInt");
            value |= (next & 127) << shift;
            if (next < 128) return value;
        }
        throw fail("Invalid S4 VarInt");
    }

    public FriendlyByteBuf writeVarInt(int value) {
        while ((value & ~127) != 0) {
            requireWrite(1);
            output.write((value & 127) | 128);
            value >>>= 7;
        }
        requireWrite(1);
        output.write(value);
        return this;
    }

    public int readInt() {
        requireRead(4);
        return input.getInt();
    }

    public boolean readBoolean() {
        requireRead(1);
        return input.get() != 0;
    }

    public String readUtf() { return readUtf(32767); }

    public String readUtf(int maxLength) {
        int length = readVarInt();
        requireRead(length);
        if (maxLength < 0 || length > 3L * maxLength) throw fail("Oversized S4 string");
        try {
            var slice = input.slice(input.position(), length);
            String value = StandardCharsets.UTF_8.newDecoder().decode(slice).toString();
            if (value.length() > maxLength) throw fail("Oversized S4 string");
            input.position(input.position() + length);
            return value;
        } catch (CharacterCodingException error) {
            throw fail("Invalid S4 UTF-8");
        }
    }

    public FriendlyByteBuf writeUtf(String value) { return writeUtf(value, 32767); }

    public FriendlyByteBuf writeUtf(String value, int maxLength) {
        if (value.length() > maxLength || value.length() > MAX_PAYLOAD) throw fail("Oversized S4 string");
        try {
            ByteBuffer encoded = StandardCharsets.UTF_8.newEncoder().encode(CharBuffer.wrap(value));
            int length = encoded.remaining();
            writeVarInt(length);
            requireWrite(length);
            output.write(encoded.array(), encoded.arrayOffset() + encoded.position(), length);
            return this;
        } catch (CharacterCodingException error) {
            throw fail("Invalid S4 UTF-8");
        }
    }

    public byte[] readByteArray() { return readByteArray(MAX_PAYLOAD); }

    public byte[] readByteArray(int maxLength) {
        int length = readVarInt();
        if (length > maxLength) throw fail("Oversized S4 byte array");
        requireRead(length);
        byte[] bytes = new byte[length];
        input.get(bytes);
        return bytes;
    }

    public FriendlyByteBuf writeByteArray(byte[] bytes) {
        writeVarInt(bytes.length);
        requireWrite(bytes.length);
        output.writeBytes(bytes);
        return this;
    }

    public final byte[] finish() {
        checkOpen();
        if (input != null && input.hasRemaining()) throw fail("Trailing S4 payload bytes");
        return output == null ? null : output.toByteArray();
    }

    public final void closeView() { closed = true; }
}
