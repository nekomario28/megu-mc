package net.megu.integrated;

import java.lang.invoke.MethodHandle;
import java.lang.ref.Reference;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Path;

/**
 * Zero-copy bridge to {@code libmegu_client_mesh.so} (C ABI v1).
 *
 * <p>Section block/light data and quad/vertex output stay in caller-owned native-order
 * direct {@link ByteBuffer}s; only the 88-byte {@code megu_mesh_input} descriptor is
 * built internally. Heap buffers are rejected. Calls are serialized per bridge and close
 * is idempotent. Optional halo borders are not supplied (missing-cell policy applies).
 */
public final class NativeMeshBridge implements AutoCloseable {
    public static final String LIBRARY_NAME = "libmegu_client_mesh.so";
    public static final int ABI_VERSION = 1;
    public static final int SECTION_CELLS = 4096;
    public static final int BLOCK_BYTES = 16;
    public static final int LIGHT_BYTES = 2048;
    public static final int QUAD_BYTES = 84;
    public static final int VERTEX_BYTES = 16;
    public static final int MISSING_AIR = 0;
    public static final int MISSING_OPAQUE = 1;
    private static final int INPUT_BYTES = 88;

    private final NativeLibrary library;
    private final MethodHandle section;
    private final MethodHandle pack;
    private final ByteBuffer input = ByteBuffer.allocateDirect(INPUT_BYTES + 8).order(ByteOrder.nativeOrder());
    private final ByteBuffer count = ByteBuffer.allocateDirect(16).order(ByteOrder.nativeOrder());
    private final long inputAddress;
    private final long countAddress;
    private boolean closed;

    private NativeMeshBridge(NativeLibrary library) {
        this.library = library;
        try {
            section = library.bind("megu_mesh_section", int.class,
                    long.class, long.class, long.class, long.class);
            pack = library.bind("megu_client_mesh_pack_vertices", int.class,
                    long.class, long.class, long.class, long.class, long.class);
            inputAddress = NativeLibrary.require(input, "input", INPUT_BYTES, 8);
            countAddress = NativeLibrary.require(count, "count", 8, 8);
        } catch (RuntimeException error) {
            library.close();
            throw error;
        }
    }

    /** Load {@code libmegu_client_mesh.so} from an absolute path; fails closed on any mismatch. */
    public static NativeMeshBridge open(Path library) {
        return new NativeMeshBridge(NativeLibrary.open(library, LIBRARY_NAME,
                "megu_mesh_abi_version", ABI_VERSION));
    }

    /**
     * Mesh one section. {@code blocks} is 4096 16-byte megu_mesh_block cells, the lights
     * are 2048 bytes each. Pass a null {@code quads} with capacity 0 to query the quad count.
     * Returns the number of quads (written when {@code quads} is non-null).
     */
    public synchronized int meshSection(ByteBuffer blocks, ByteBuffer blockLight, ByteBuffer skyLight,
            int missing, ByteBuffer quads, int capacity) {
        ensureOpen();
        long blocksAddress = NativeLibrary.require(blocks, "blocks", SECTION_CELLS * BLOCK_BYTES, 4);
        long blockLightAddress = NativeLibrary.require(blockLight, "block light", LIGHT_BYTES, 1);
        long skyLightAddress = NativeLibrary.require(skyLight, "sky light", LIGHT_BYTES, 1);
        long quadsAddress = outputAddress(quads, capacity, QUAD_BYTES, 4, "quads");
        input.clear();
        for (int offset = 0; offset < input.capacity(); offset += 8) {
            input.putLong(offset, 0);
        }
        input.putInt(0, ABI_VERSION).putInt(4, missing);
        input.putLong(8, blocksAddress).putLong(16, blockLightAddress).putLong(24, skyLightAddress);
        try {
            return run(section, "mesh_section", inputAddress, quadsAddress, (long) capacity, countAddress);
        } finally {
            Reference.reachabilityFence(blocks);
            Reference.reachabilityFence(blockLight);
            Reference.reachabilityFence(skyLight);
            Reference.reachabilityFence(quads);
        }
    }

    /**
     * Pack {@code quadCount} 84-byte quads into 16-byte GPU vertices (four per quad).
     * Pass a null {@code vertices} with capacity 0 to query the vertex count.
     */
    public synchronized int packVertices(ByteBuffer quads, int quadCount, ByteBuffer vertices, int capacity) {
        ensureOpen();
        if (quadCount < 0 || (long) quadCount * QUAD_BYTES > Integer.MAX_VALUE) {
            throw new NativeBridgeException("invalid quad count");
        }
        long quadsAddress = NativeLibrary.require(quads, "quads", quadCount * QUAD_BYTES, 4);
        long verticesAddress = outputAddress(vertices, capacity, VERTEX_BYTES, 4, "vertices");
        try {
            return run(pack, "client_mesh_pack_vertices", quadsAddress, (long) quadCount,
                    verticesAddress, (long) capacity, countAddress);
        } finally {
            Reference.reachabilityFence(quads);
            Reference.reachabilityFence(vertices);
        }
    }

    public synchronized boolean isOpen() {
        return !closed;
    }

    @Override
    public synchronized void close() {
        if (!closed) {
            closed = true;
            library.close();
        }
    }

    private void ensureOpen() {
        if (closed) {
            throw new NativeBridgeException("mesh bridge is closed");
        }
    }

    private static long outputAddress(ByteBuffer buffer, int capacity, int unit, int alignment, String name) {
        if (capacity < 0) {
            throw new NativeBridgeException(name + " capacity must not be negative");
        }
        if (buffer == null) {
            if (capacity != 0) {
                throw new NativeBridgeException(name + " capacity requires an output buffer");
            }
            return 0;
        }
        if ((long) capacity * unit > Integer.MAX_VALUE) {
            throw new NativeBridgeException(name + " capacity is too large");
        }
        return NativeLibrary.require(buffer, name, capacity * unit, alignment);
    }

    private int run(MethodHandle handle, String name, Object... arguments) {
        count.putLong(0, 0);
        int status;
        try {
            status = (int) handle.invokeWithArguments(arguments);
        } catch (RuntimeException | Error error) {
            throw error;
        } catch (Throwable error) {
            throw new NativeBridgeException("native call failed", error);
        }
        long produced = count.getLong(0);
        if (status != 0) {
            throw new NativeBridgeException("megu_" + name + " failed with status " + status
                    + (status == 2 ? " (required " + produced + ")" : ""), status);
        }
        if (produced < 0 || produced > Integer.MAX_VALUE) {
            throw new NativeBridgeException("native count out of range");
        }
        return (int) produced;
    }
}
