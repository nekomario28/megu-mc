package net.mcspine.integrated;

import java.lang.invoke.MethodHandle;
import java.lang.ref.Reference;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Path;
import java.util.IdentityHashMap;
import java.util.Set;
import java.util.Collections;

/**
 * Zero-copy bridge to {@code libmc_spine_kernel.so} (C ABI v1).
 *
 * <p>Caller-supplied buffers must be native-order direct {@link ByteBuffer}s whose
 * address is aligned for the C type; their memory is handed to C without copying.
 * Heap buffers are rejected. Calls are serialized per bridge. Closing the bridge frees
 * every live {@link Palette} and unloads the library; close is idempotent.
 */
public final class NativeKernelBridge implements AutoCloseable {
    public static final String LIBRARY_NAME = "libmc_spine_kernel.so";
    public static final int ABI_VERSION = 1;
    public static final int SECTION_CELLS = 4096;
    public static final int STATUS_OK = 0;
    public static final int AABB_BYTES = 48;
    public static final int DELTA_BYTES = 24;

    /** Collision result: resolved delta and MC_SPINE_KERNEL_COLLISION_* flags. */
    public record Collision(double dx, double dy, double dz, int flags) {}

    private final NativeLibrary library;
    private final MethodHandle paletteNew;
    private final MethodHandle paletteFree;
    private final MethodHandle paletteGet;
    private final MethodHandle paletteSet;
    private final MethodHandle paletteNonAir;
    private final MethodHandle paletteWire;
    private final MethodHandle bytesFree;
    private final MethodHandle nbtValidate;
    private final MethodHandle collide;
    private final Set<Palette> palettes = Collections.newSetFromMap(new IdentityHashMap<>());
    private final ByteBuffer scratch = ByteBuffer.allocateDirect(64).order(ByteOrder.nativeOrder());
    private final long scratchAddress;
    private boolean closed;

    private NativeKernelBridge(NativeLibrary library) {
        this.library = library;
        try {
            paletteNew = library.bind("mc_spine_kernel_palette_new_single", int.class, int.class, long.class);
            paletteFree = library.bind("mc_spine_kernel_palette_free", int.class, long.class);
            paletteGet = library.bind("mc_spine_kernel_palette_get", int.class, long.class, int.class, long.class);
            paletteSet = library.bind("mc_spine_kernel_palette_set", int.class, long.class, int.class, int.class);
            paletteNonAir = library.bind("mc_spine_kernel_palette_non_air", int.class, long.class, long.class);
            paletteWire = library.bind("mc_spine_kernel_palette_write_block_states_wire", int.class,
                    long.class, long.class);
            bytesFree = library.bind("mc_spine_kernel_bytes_free", int.class, long.class, long.class);
            nbtValidate = library.bind("mc_spine_kernel_nbt_read_named_validate", int.class,
                    long.class, long.class);
            collide = library.bind("mc_spine_kernel_collide", int.class, long.class, long.class,
                    byte.class, float.class, long.class, long.class, long.class);
            scratchAddress = NativeLibrary.require(scratch, "scratch", 64, 8);
        } catch (RuntimeException error) {
            library.close();
            throw error;
        }
    }

    /** Load {@code libmc_spine_kernel.so} from an absolute path; fails closed on any mismatch. */
    public static NativeKernelBridge open(Path library) {
        return new NativeKernelBridge(NativeLibrary.open(library, LIBRARY_NAME,
                "mc_spine_kernel_abi_version", ABI_VERSION));
    }

    public synchronized Palette newPalette(int state) {
        ensureOpen();
        scratch.putLong(0, 0);
        status(paletteNew, "palette_new_single", state, scratchAddress);
        long handle = scratch.getLong(0);
        if (handle == 0) {
            throw new NativeBridgeException("kernel returned a null palette");
        }
        Palette palette = new Palette(handle);
        palettes.add(palette);
        return palette;
    }

    /** Validate a named NBT tree in place. Returns the C status (0 valid, 5 invalid NBT, ...). */
    public synchronized int nbtValidateNamed(ByteBuffer input) {
        ensureOpen();
        long address = NativeLibrary.require(input, "nbt input", input == null ? 0 : input.remaining(), 1);
        try {
            return (int) invoke(nbtValidate, address, (long) input.remaining());
        } finally {
            Reference.reachabilityFence(input);
        }
    }

    /**
     * Resolve one movement against {@code shapeCount} 48-byte AABBs. All buffers are read in
     * place from their current position: {@code aabb} 48 bytes, {@code delta} 24 bytes
     * (three doubles), {@code shapes} 48 * shapeCount bytes (may be null when the count is 0).
     */
    public synchronized Collision collide(ByteBuffer aabb, ByteBuffer delta, boolean onGround,
            float maxUpStep, ByteBuffer shapes, int shapeCount) {
        ensureOpen();
        long aabbAddress = NativeLibrary.require(aabb, "aabb", AABB_BYTES, 8);
        long deltaAddress = NativeLibrary.require(delta, "delta", DELTA_BYTES, 8);
        long shapesAddress = 0;
        if (shapeCount < 0) {
            throw new NativeBridgeException("shape count must not be negative");
        }
        if (shapeCount > 0) {
            if ((long) shapeCount * AABB_BYTES > Integer.MAX_VALUE) {
                throw new NativeBridgeException("shape table is too large");
            }
            shapesAddress = NativeLibrary.require(shapes, "shapes", shapeCount * AABB_BYTES, 8);
        }
        try {
            status(collide, "collide", aabbAddress, deltaAddress, (byte) (onGround ? 1 : 0), maxUpStep,
                    shapesAddress, (long) shapeCount, scratchAddress + 32);
        } finally {
            Reference.reachabilityFence(aabb);
            Reference.reachabilityFence(delta);
            Reference.reachabilityFence(shapes);
        }
        return new Collision(scratch.getDouble(32), scratch.getDouble(40), scratch.getDouble(48),
                scratch.get(56) & 0xff);
    }

    public synchronized boolean isOpen() {
        return !closed;
    }

    @Override
    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        try {
            for (Palette palette : palettes.toArray(new Palette[0])) {
                palette.release();
            }
        } finally {
            palettes.clear();
            library.close();
        }
    }

    private void ensureOpen() {
        if (closed) {
            throw new NativeBridgeException("kernel bridge is closed");
        }
    }

    private static Object invoke(MethodHandle handle, Object... arguments) {
        try {
            return handle.invokeWithArguments(arguments);
        } catch (RuntimeException | Error error) {
            throw error;
        } catch (Throwable error) {
            throw new NativeBridgeException("native call failed", error);
        }
    }

    private static void status(MethodHandle handle, String name, Object... arguments) {
        int status = (int) invoke(handle, arguments);
        if (status != STATUS_OK) {
            throw new NativeBridgeException("mc_spine_kernel_" + name + " failed with status " + status, status);
        }
    }

    /** Owned native block-state palette for one 16x16x16 section; close is idempotent. */
    public final class Palette implements AutoCloseable {
        private long handle;

        private Palette(long handle) {
            this.handle = handle;
        }

        public int get(int index) {
            synchronized (NativeKernelBridge.this) {
                long live = live();
                status(paletteGet, "palette_get", live, index, scratchAddress);
                return scratch.getInt(0);
            }
        }

        public void set(int index, int state) {
            synchronized (NativeKernelBridge.this) {
                status(paletteSet, "palette_set", live(), index, state);
            }
        }

        public int nonAir() {
            synchronized (NativeKernelBridge.this) {
                status(paletteNonAir, "palette_non_air", live(), scratchAddress);
                return scratch.getShort(0) & 0xffff;
            }
        }

        /** Serialize the protocol-767 block-state paletted container; the native copy is freed. */
        public byte[] writeWire() {
            synchronized (NativeKernelBridge.this) {
                long live = live();
                scratch.putLong(0, 0).putLong(8, 0);
                status(paletteWire, "palette_write_block_states_wire", live, scratchAddress);
                long data = scratch.getLong(0);
                long length = scratch.getLong(8);
                try {
                    return NativeLibrary.copyOut(data, length);
                } finally {
                    status(bytesFree, "bytes_free", data, length);
                }
            }
        }

        @Override
        public void close() {
            synchronized (NativeKernelBridge.this) {
                if (handle != 0 && !closed) {
                    palettes.remove(this);
                    release();
                }
                handle = 0;
            }
        }

        private long live() {
            ensureOpen();
            if (handle == 0) {
                throw new NativeBridgeException("palette is closed");
            }
            return handle;
        }

        private void release() {
            long freed = handle;
            handle = 0;
            if (freed != 0) {
                status(paletteFree, "palette_free", freed);
            }
        }
    }
}
