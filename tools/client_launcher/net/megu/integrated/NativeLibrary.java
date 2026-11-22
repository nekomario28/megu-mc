package net.megu.integrated;

import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermission;
import java.util.Optional;
import java.util.Set;

/**
 * Validated, ABI-checked handle to one megu C library.
 *
 * <p>The project compiles with {@code --release 21}, where {@code java.lang.foreign} is a
 * preview API (final in 22). The Foreign Function and Memory API is therefore reached
 * reflectively so the same bytecode loads on JDK 22+ and refuses to load (fail-closed)
 * where the API is unavailable. Run with {@code --enable-native-access=ALL-UNNAMED}.
 *
 * <p>All C pointers are declared as {@code long} in downcall descriptors. That is
 * ABI-identical on LP64 targets (checked at load) and keeps FFM types out of
 * the bytecode. The two-word {@code {ptr,len}} by-value structs are likewise passed as two
 * integer-class words, which the SysV x86-64 and AAPCS64 ABIs treat identically.
 */
final class NativeLibrary implements AutoCloseable {
    private static final Class<?> SEGMENT = type("java.lang.foreign.MemorySegment");
    private static final Class<?> LAYOUT = type("java.lang.foreign.MemoryLayout");
    private static final Class<?> VALUE_LAYOUT = type("java.lang.foreign.ValueLayout");
    private static final Class<?> DESCRIPTOR = type("java.lang.foreign.FunctionDescriptor");
    private static final Class<?> LINKER = type("java.lang.foreign.Linker");
    private static final Class<?> OPTION = type("java.lang.foreign.Linker$Option");
    private static final Class<?> LOOKUP = type("java.lang.foreign.SymbolLookup");
    private static final Class<?> ARENA = type("java.lang.foreign.Arena");

    private final Path path;
    private final Object arena;
    private final Object lookup;
    private final Object linker;
    private boolean closed;

    private NativeLibrary(Path path, Object arena, Object lookup, Object linker) {
        this.path = path;
        this.arena = arena;
        this.lookup = lookup;
        this.linker = linker;
    }

    /** Validate the path, load the library and require {@code abiSymbol()==expectedAbi}. */
    static NativeLibrary open(Path requested, String expectedName, String abiSymbol, int expectedAbi) {
        if (requested == null || !requested.isAbsolute() || !requested.equals(requested.normalize())) {
            throw new NativeBridgeException("native library path must be absolute and normalized");
        }
        if (requested.getFileName() == null || !requested.getFileName().toString().equals(expectedName)) {
            throw new NativeBridgeException("native library must be named " + expectedName);
        }
        Path real;
        try {
            if (!Files.isRegularFile(requested)) {
                throw new NativeBridgeException("native library is missing or not a regular file");
            }
            real = requested.toRealPath();
            Set<PosixFilePermission> mode = Files.getPosixFilePermissions(real);
            if (mode.contains(PosixFilePermission.GROUP_WRITE) || mode.contains(PosixFilePermission.OTHERS_WRITE)) {
                throw new NativeBridgeException("native library must not be group or world writable");
            }
        } catch (IOException | UnsupportedOperationException error) {
            throw new NativeBridgeException("cannot validate native library", error);
        }
        if (ARENA == null || LINKER == null || SEGMENT == null) {
            throw new NativeBridgeException("java.lang.foreign is unavailable; JDK 22+ is required");
        }
        Object arena = null;
        try {
            Object address = VALUE_LAYOUT.getField("ADDRESS").get(null);
            if ((long) call(LAYOUT.getMethod("byteSize"), address) != Long.BYTES) {
                throw new NativeBridgeException("native bridge requires an LP64 platform");
            }
            arena = call(ARENA.getMethod("ofShared"), null);
            Object lookup = call(LOOKUP.getMethod("libraryLookup", Path.class, ARENA), null, real, arena);
            Object linker = call(LINKER.getMethod("nativeLinker"), null);
            NativeLibrary library = new NativeLibrary(real, arena, lookup, linker);
            try {
                int found = (int) library.bind(abiSymbol, int.class).invokeWithArguments();
                if (found != expectedAbi) {
                    throw new NativeBridgeException(
                            "unsupported " + expectedName + " ABI " + found + ", expected " + expectedAbi);
                }
            } catch (NativeBridgeException error) {
                library.close();
                throw error;
            } catch (Throwable error) {
                library.close();
                throw new NativeBridgeException("ABI handshake failed for " + expectedName, error);
            }
            return library;
        } catch (NativeBridgeException error) {
            throw error;
        } catch (ReflectiveOperationException | RuntimeException | LinkageError error) {
            if (arena != null) {
                closeQuietly(arena);
            }
            throw new NativeBridgeException(
                    "cannot load " + expectedName + " (requires JDK 22+ java.lang.foreign and "
                            + "--enable-native-access=ALL-UNNAMED): " + error.getMessage(), error);
        }
    }

    Path path() {
        return path;
    }

    /** Bind a symbol. Parameters and result may be int, long, byte, short, float or double. */
    MethodHandle bind(String symbol, Class<?> result, Class<?>... parameters) {
        try {
            Optional<?> address = (Optional<?>) call(LOOKUP.getMethod("find", String.class), lookup, symbol);
            if (address.isEmpty()) {
                throw new NativeBridgeException("native library lacks symbol " + symbol);
            }
            Object layouts = Array.newInstance(LAYOUT, parameters.length);
            for (int i = 0; i < parameters.length; i++) {
                Array.set(layouts, i, layout(parameters[i]));
            }
            Object descriptor = call(DESCRIPTOR.getMethod("of", LAYOUT, layouts.getClass()), null,
                    layout(result), layouts);
            Method downcall = LINKER.getMethod("downcallHandle", SEGMENT, DESCRIPTOR,
                    Array.newInstance(OPTION, 0).getClass());
            return (MethodHandle) call(downcall, linker, address.get(), descriptor,
                    Array.newInstance(OPTION, 0));
        } catch (ReflectiveOperationException error) {
            throw new NativeBridgeException("cannot bind " + symbol, error);
        }
    }

    /** Address of the first remaining byte of a direct buffer. */
    static long address(ByteBuffer buffer) {
        try {
            Object segment = call(SEGMENT.getMethod("ofBuffer", java.nio.Buffer.class), null, buffer);
            return (long) call(SEGMENT.getMethod("address"), segment);
        } catch (ReflectiveOperationException error) {
            throw new NativeBridgeException("cannot resolve buffer address", error);
        }
    }

    /** Copy {@code length} bytes from a native address into a new heap array. */
    static byte[] copyOut(long address, long length) {
        if (address == 0 || length < 0 || length > Integer.MAX_VALUE - 16) {
            throw new NativeBridgeException("invalid native byte range");
        }
        try {
            Object segment = call(SEGMENT.getMethod("ofAddress", long.class), null, address);
            segment = call(SEGMENT.getMethod("reinterpret", long.class), segment, length);
            ByteBuffer view = (ByteBuffer) call(SEGMENT.getMethod("asByteBuffer"), segment);
            byte[] out = new byte[(int) length];
            view.get(out);
            return out;
        } catch (ReflectiveOperationException error) {
            throw new NativeBridgeException("cannot read native bytes", error);
        }
    }

    /** Require a native-order direct buffer with at least {@code bytes} remaining and aligned address. */
    static long require(ByteBuffer buffer, String name, int bytes, int alignment) {
        if (buffer == null || !buffer.isDirect() || buffer.isReadOnly()) {
            throw new NativeBridgeException(name + " must be a writable direct ByteBuffer");
        }
        if (buffer.order() != ByteOrder.nativeOrder()) {
            throw new NativeBridgeException(name + " must use native byte order");
        }
        if (bytes < 0 || buffer.remaining() < bytes) {
            throw new NativeBridgeException(name + " is too small: need " + bytes + " bytes");
        }
        long address = address(buffer);
        if (address == 0 || address % alignment != 0) {
            throw new NativeBridgeException(name + " is not " + alignment + "-byte aligned");
        }
        return address;
    }

    synchronized boolean isOpen() {
        return !closed;
    }

    @Override
    public synchronized void close() {
        if (!closed) {
            closed = true;
            closeQuietly(arena);
        }
    }

    private static void closeQuietly(Object arena) {
        try {
            ((AutoCloseable) arena).close();
        } catch (Exception ignored) {
            // The library stays mapped; nothing further can be released.
        }
    }

    private static Object layout(Class<?> type) throws ReflectiveOperationException {
        String field = type == int.class ? "JAVA_INT" : type == long.class ? "JAVA_LONG"
                : type == byte.class ? "JAVA_BYTE" : type == short.class ? "JAVA_SHORT"
                : type == float.class ? "JAVA_FLOAT" : type == double.class ? "JAVA_DOUBLE" : null;
        if (field == null) {
            throw new IllegalArgumentException("unsupported native type " + type);
        }
        return VALUE_LAYOUT.getField(field).get(null);
    }

    private static Object call(Method method, Object target, Object... arguments)
            throws ReflectiveOperationException {
        try {
            return method.invoke(target, arguments);
        } catch (InvocationTargetException error) {
            Throwable cause = error.getCause();
            if (cause instanceof RuntimeException runtime) {
                throw runtime;
            }
            if (cause instanceof Error fatal) {
                throw fatal;
            }
            throw error;
        }
    }

    private static Class<?> type(String name) {
        try {
            return Class.forName(name);
        } catch (ClassNotFoundException error) {
            return null;
        }
    }
}
