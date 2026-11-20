package net.mcspine.integrated;

import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.Channels;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.channels.ReadableByteChannel;
import java.nio.channels.WritableByteChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.zip.CRC32;
import java.util.concurrent.TimeUnit;

/** Owns a single local mc-spine process and verifies protocol-767 readiness. */
public final class IntegratedServerBridge implements AutoCloseable {
    private static final String LOOPBACK = "127.0.0.1";
    private static IntegratedServerBridge configuredBridge;
    private final Process server;
    private final FileChannel lockChannel;
    private final FileLock worldLock;
    private final Thread shutdownHook;
    private final int port;
    private boolean closed;

    private IntegratedServerBridge(Process server, FileChannel channel, FileLock lock, int port) {
        this.server = server;
        this.lockChannel = channel;
        this.worldLock = lock;
        this.port = port;
        this.shutdownHook = new Thread(this::close, "mc-spine-integrated-shutdown");
        Runtime.getRuntime().addShutdownHook(shutdownHook);
    }

    public int port() {
        return port;
    }

    public long pid() {
        return server.pid();
    }

    public boolean isAlive() {
        return server.isAlive();
    }

    /**
     * Start in gameDir/saves/worldName. A requested port of zero selects a free
     * ephemeral port. The native server accepts seed, bind and port via env vars.
     */
    public static IntegratedServerBridge start(Path binary, Path gameDir, String worldName,
            String seed, String bind, int requestedPort, Duration timeout) throws IOException {
        if (!LOOPBACK.equals(bind) || requestedPort != 0) {
            throw new IllegalArgumentException("only 127.0.0.1 and ephemeral port 0 are permitted");
        }
        if (worldName == null || !worldName.matches("[a-zA-Z0-9 _-]{1,64}")
                || worldName.equals(".") || worldName.equals("..")) {
            throw new IllegalArgumentException("world must be a safe single directory name");
        }
        if (seed == null || !seed.matches("-?[0-9]{1,20}")) {
            throw new IllegalArgumentException("seed must be an integer bit pattern");
        }
        try {
            if (seed.startsWith("-")) {
                Long.parseLong(seed);
            } else {
                Long.parseUnsignedLong(seed);
            }
        } catch (NumberFormatException error) {
            throw new IllegalArgumentException("seed exceeds 64 bits", error);
        }
        if (timeout == null || timeout.toMillis() < 100 || timeout.toMillis() > 120_000) {
            throw new IllegalArgumentException("timeout must be between 100 and 120000 ms");
        }
        if (binary == null || !Files.isRegularFile(binary) || !Files.isExecutable(binary)) {
            throw new IOException("server binary is missing or not executable");
        }
        Path executable = binary.toRealPath();
        if (gameDir == null || !Files.isDirectory(gameDir)) {
            throw new IOException("game directory must exist");
        }
        Path base = gameDir.toRealPath().resolve("saves");
        Files.createDirectories(base);
        base = base.toRealPath();
        Path world = base.resolve(worldName);
        Files.createDirectories(world);
        world = world.toRealPath();
        if (!world.getParent().equals(base)) {
            throw new IOException("world path escapes the saves directory");
        }
        installRuntimeData(world, executable, gameDir.toRealPath());

        FileChannel channel = FileChannel.open(world.resolve(".mc-spine-integrated.lock"),
                StandardOpenOption.CREATE, StandardOpenOption.WRITE);
        FileLock lock = null;
        Process process = null;
        try {
            try {
                lock = channel.tryLock();
            } catch (java.nio.channels.OverlappingFileLockException error) {
                throw new IOException("integrated world is already in use", error);
            }
            if (lock == null) {
                throw new IOException("integrated world is already in use");
            }
            int port;
            try (ServerSocket reserved = new ServerSocket(0, 1, InetAddress.getByName(LOOPBACK))) {
                port = reserved.getLocalPort();
            }
            ProcessBuilder builder = new ProcessBuilder(executable.toString());
            builder.directory(world.toFile());
            builder.redirectInput(ProcessBuilder.Redirect.PIPE);
            builder.redirectOutput(ProcessBuilder.Redirect.DISCARD);
            builder.redirectError(ProcessBuilder.Redirect.DISCARD);
            Map<String, String> env = builder.environment();
            env.put("MC_SPINE_BIND_ADDR", LOOPBACK);
            env.put("MC_SPINE_PORT", Integer.toString(port));
            env.put("MC_SPINE_WORLD_DIR", world.toString());
            env.put("MC_SPINE_SEED", seed);
            env.put("MC_SPINE_RESOURCE_PACK_URL", "");
            env.put("MC_SPINE_MAX_CONN_PER_IP", "1");
            env.remove("MC_SPINE_ALLOW_PUBLIC_BIND");
            env.remove("MC_SPINE_RCON_PORT");
            env.remove("MC_SPINE_RCON_PASS");
            process = builder.start();
            process.getOutputStream().close();
            long deadline = System.nanoTime() + timeout.toNanos();
            while (System.nanoTime() < deadline) {
                if (!process.isAlive()) {
                    throw new IOException("mc-spine exited during startup (" + process.exitValue() + ")");
                }
                if (statusReady(port)) {
                    if (!process.isAlive()) {
                        throw new IOException("mc-spine exited after status handshake");
                    }
                    return new IntegratedServerBridge(process, channel, lock, port);
                }
                try {
                    Thread.sleep(50);
                } catch (InterruptedException error) {
                    Thread.currentThread().interrupt();
                    throw new IOException("interrupted during startup", error);
                }
            }
            throw new IOException("mc-spine protocol 767 startup timeout on " + LOOPBACK + ":" + port);
        } catch (IOException | RuntimeException | Error error) {
            if (process != null) {
                stopProcess(process);
            }
            if (lock != null) {
                lock.release();
            }
            channel.close();
            throw error;
        }
    }

    private static void installRuntimeData(Path world, Path binary, Path gameDir) throws IOException {
        String configured = System.getenv("MC_SPINE_SERVER_HOME");
        List<Path> candidates = new ArrayList<>();
        if (configured != null && !configured.isBlank()) {
            candidates.add(Path.of(configured));
        } else {
            candidates.add(gameDir);
            candidates.add(binary.getParent());
            candidates.add(Path.of("").toAbsolutePath());
        }
        for (Path candidate : candidates) {
            if (!Files.isRegularFile(candidate.resolve("config_packets.bin"))
                    || !Files.isDirectory(candidate.resolve("assets"))) {
                continue;
            }
            for (String name : List.of("config_packets.bin", "assets", "datapacks/vanilla")) {
                Path source = candidate.resolve(name);
                if (!Files.exists(source)) {
                    continue;
                }
                source = source.toRealPath();
                Path dest = world.resolve(name);
                if (!Files.exists(dest, LinkOption.NOFOLLOW_LINKS)) {
                    Files.createDirectories(dest.getParent());
                    Files.createSymbolicLink(dest, source);
                }
            }
            return;
        }
        if (configured != null && !configured.isBlank()) {
            throw new IOException("MC_SPINE_SERVER_HOME must contain config_packets.bin and assets");
        }
    }

    private static boolean statusReady(int port) {
        try (Socket socket = new Socket()) {
            socket.connect(new InetSocketAddress(LOOPBACK, port), 250);
            socket.setSoTimeout(500);
            OutputStream out = socket.getOutputStream();
            byte[] host = LOOPBACK.getBytes(StandardCharsets.UTF_8);
            ByteArrayOutputStream handshake = new ByteArrayOutputStream();
            writeVarInt(handshake, 0);
            writeVarInt(handshake, 767);
            writeVarInt(handshake, host.length);
            handshake.writeBytes(host);
            handshake.write((port >> 8) & 0xff);
            handshake.write(port & 0xff);
            writeVarInt(handshake, 1);
            writeFrame(out, handshake.toByteArray());
            writeFrame(out, new byte[]{0});
            InputStream in = socket.getInputStream();
            int length = readVarInt(in);
            if (length < 3 || length > 16_384) {
                return false;
            }
            byte[] frame = in.readNBytes(length);
            if (frame.length != length) {
                return false;
            }
            java.io.ByteArrayInputStream payload = new java.io.ByteArrayInputStream(frame);
            if (readVarInt(payload) != 0) {
                return false;
            }
            int jsonLength = readVarInt(payload);
            if (jsonLength != payload.available() || jsonLength < 1) {
                return false;
            }
            String json = new String(payload.readNBytes(jsonLength), StandardCharsets.UTF_8);
            return json.matches("(?s).*\\\"protocol\\\"\\s*:\\s*767\\b.*")
                    && json.matches("(?s).*\\\"text\\\"\\s*:\\s*\\\"mc-spine\\\".*");
        } catch (IOException | RuntimeException error) {
            return false;
        }
    }

    private static void writeFrame(OutputStream out, byte[] body) throws IOException {
        ByteArrayOutputStream packet = new ByteArrayOutputStream();
        writeVarInt(packet, body.length);
        packet.writeBytes(body);
        out.write(packet.toByteArray());
        out.flush();
    }

    private static void writeVarInt(OutputStream out, int value) throws IOException {
        do {
            int next = value & 0x7f;
            value >>>= 7;
            out.write(value == 0 ? next : next | 0x80);
        } while (value != 0);
    }

    private static int readVarInt(InputStream in) throws IOException {
        int value = 0;
        for (int i = 0; i < 5; i++) {
            int next = in.read();
            if (next < 0) {
                throw new EOFException("short VarInt");
            }
            value |= (next & 0x7f) << (i * 7);
            if ((next & 0x80) == 0) {
                return value;
            }
        }
        throw new IOException("oversized VarInt");
    }

    // ---- CF-3b: direct ByteBuffer MCSI v1 frame bridge ------------------

    /** Little-endian "MCSI" magic. */
    public static final int DIRECT_FRAME_MAGIC = 0x4D435349;
    public static final int DIRECT_FRAME_VERSION = 1;
    public static final int KIND_PROTOCOL767_PACKET = 1;
    public static final int KIND_PALETTED_SECTION_SNAPSHOT = 2;
    public static final int KIND_TICK_HEARTBEAT = 3;
    /** magic(4) | version(2) | kind(2) | payloadLength(4) | crc32(4). */
    public static final int DIRECT_FRAME_HEADER_SIZE = 16;
    public static final int DIRECT_FRAME_MAX_PAYLOAD = 262_144;

    /** Immutable decoded CF-3b frame; payload is defensively copied. */
    public record DirectFrame(int kind, byte[] payload, long crc32) {
        public DirectFrame {
            if (payload == null) {
                throw new IllegalArgumentException("payload must not be null");
            }
            payload = payload.clone();
        }

        public int frameSize() {
            return DIRECT_FRAME_HEADER_SIZE + payload.length;
        }

        @Override
        public byte[] payload() {
            return payload.clone();
        }
    }

    private static void requireDirectFrameBuffer(ByteBuffer buf, String name) {
        if (buf == null) {
            throw new IllegalArgumentException(name + " must not be null");
        }
        if (!buf.isDirect()) {
            throw new IllegalArgumentException(name + " must be a direct ByteBuffer");
        }
        if (buf.order() != ByteOrder.LITTLE_ENDIAN) {
            throw new IllegalArgumentException(name + " must use ByteOrder.LITTLE_ENDIAN");
        }
    }

    private static void validateDirectFrameKind(int kind) {
        if (kind != KIND_PROTOCOL767_PACKET && kind != KIND_PALETTED_SECTION_SNAPSHOT
                && kind != KIND_TICK_HEARTBEAT) {
            throw new IllegalArgumentException("unsupported MCSI frame kind: " + kind);
        }
    }

    /**
     * Writes one MCSI v1 frame at {@code buf.position()} directly into the
     * direct buffer (no intermediate heap frame) and advances the position.
     */
    public static void writeDirectFrame(ByteBuffer buf, int kind, byte[] payload) {
        requireDirectFrameBuffer(buf, "buf");
        validateDirectFrameKind(kind);
        if (payload == null) {
            throw new IllegalArgumentException("payload must not be null");
        }
        if (payload.length > DIRECT_FRAME_MAX_PAYLOAD) {
            throw new IllegalArgumentException("MCSI frame payload exceeds "
                    + DIRECT_FRAME_MAX_PAYLOAD + " bytes");
        }
        int base = buf.position();
        int frameSize = DIRECT_FRAME_HEADER_SIZE + payload.length;
        if (frameSize > buf.remaining()) {
            throw new IllegalArgumentException("direct buffer too small for MCSI frame: need "
                    + frameSize + ", remaining " + buf.remaining());
        }
        CRC32 crc = new CRC32();
        crc.update(payload);
        buf.putInt(base, DIRECT_FRAME_MAGIC);
        buf.putShort(base + 4, (short) DIRECT_FRAME_VERSION);
        buf.putShort(base + 6, (short) kind);
        buf.putInt(base + 8, payload.length);
        buf.putInt(base + 12, (int) crc.getValue());
        for (int i = 0; i < payload.length; i++) {
            buf.put(base + DIRECT_FRAME_HEADER_SIZE + i, payload[i]);
        }
        buf.position(base + frameSize);
    }

    private static int parseAndValidateHeader(ByteBuffer buf, int base) {
        int magic = buf.getInt(base);
        if (magic != DIRECT_FRAME_MAGIC) {
            throw new IllegalArgumentException("bad MCSI frame magic: 0x"
                    + Integer.toHexString(magic));
        }
        int version = Short.toUnsignedInt(buf.getShort(base + 4));
        if (version != DIRECT_FRAME_VERSION) {
            throw new IllegalArgumentException("unsupported MCSI frame version: " + version);
        }
        validateDirectFrameKind(Short.toUnsignedInt(buf.getShort(base + 6)));
        int length = buf.getInt(base + 8);
        if (length < 0 || length > DIRECT_FRAME_MAX_PAYLOAD) {
            throw new IllegalArgumentException("invalid MCSI frame payload length: " + length);
        }
        return length;
    }

    private static DirectFrame decodeFrameAt(ByteBuffer buf, int base) {
        if (buf.limit() - base < DIRECT_FRAME_HEADER_SIZE) {
            throw new IllegalArgumentException("truncated MCSI frame header");
        }
        int length = parseAndValidateHeader(buf, base);
        if (buf.limit() - base < DIRECT_FRAME_HEADER_SIZE + length) {
            throw new IllegalArgumentException("truncated MCSI frame payload");
        }
        long expectedCrc = Integer.toUnsignedLong(buf.getInt(base + 12));
        byte[] payload = new byte[length];
        for (int i = 0; i < length; i++) {
            payload[i] = buf.get(base + DIRECT_FRAME_HEADER_SIZE + i);
        }
        CRC32 crc = new CRC32();
        crc.update(payload);
        if (crc.getValue() != expectedCrc) {
            throw new IllegalArgumentException("MCSI frame CRC-32 mismatch");
        }
        return new DirectFrame(Short.toUnsignedInt(buf.getShort(base + 6)), payload, expectedCrc);
    }

    /** Decodes one frame at {@code buf.position()} and advances past it. */
    public static DirectFrame readDirectFrame(ByteBuffer buf) {
        requireDirectFrameBuffer(buf, "buf");
        int base = buf.position();
        DirectFrame frame = decodeFrameAt(buf, base);
        buf.position(base + frame.frameSize());
        return frame;
    }

    private static void readFully(ReadableByteChannel channel, ByteBuffer target, int length)
            throws IOException {
        int end = target.position() + length;
        if (end > target.limit()) {
            throw new IllegalArgumentException("direct buffer too small for MCSI frame read");
        }
        // The channel reads up to remaining(), not to our logical frame end, so
        // temporarily cap the limit or a header read would swallow the payload.
        int savedLimit = target.limit();
        target.limit(end);
        try {
            while (target.position() < end) {
                if (channel.read(target) < 0) {
                    throw new EOFException("short MCSI frame stream");
                }
            }
        } finally {
            target.limit(savedLimit);
        }
    }

    private static void writeFully(WritableByteChannel channel, ByteBuffer source)
            throws IOException {
        while (source.hasRemaining()) {
            if (channel.write(source) <= 0) {
                throw new IOException("no progress writing MCSI frame");
            }
        }
    }

    /**
     * Relays one framed request from {@code requestBuf} to the loopback
     * {@code out} stream, then reads the framed response straight into
     * {@code responseBuf}. The frame is never parsed as a heap stream; both
     * buffers stay direct and little-endian.
     */
    public static DirectFrame exchangeFrameOverLoopback(ByteBuffer requestBuf, InputStream in,
            OutputStream out, ByteBuffer responseBuf) throws IOException {
        requireDirectFrameBuffer(requestBuf, "requestBuf");
        requireDirectFrameBuffer(responseBuf, "responseBuf");
        if (in == null || out == null) {
            throw new IllegalArgumentException("loopback streams must not be null");
        }
        int requestSize = requestBuf.position();
        if (requestSize < DIRECT_FRAME_HEADER_SIZE) {
            throw new IllegalArgumentException("requestBuf has no complete MCSI frame");
        }
        ByteBuffer request = requestBuf.duplicate();
        request.clear();
        request.limit(requestSize);
        request.order(ByteOrder.LITTLE_ENDIAN);
        int requestPayload = parseAndValidateHeader(request, 0);
        if (requestSize != DIRECT_FRAME_HEADER_SIZE + requestPayload) {
            throw new IllegalArgumentException("requestBuf frame size does not match its header");
        }

        writeFully(Channels.newChannel(out), request);
        out.flush();

        responseBuf.clear();
        if (responseBuf.capacity() < DIRECT_FRAME_HEADER_SIZE) {
            throw new IllegalArgumentException("responseBuf cannot hold an MCSI frame header");
        }
        ReadableByteChannel source = Channels.newChannel(in);
        readFully(source, responseBuf, DIRECT_FRAME_HEADER_SIZE);
        int responsePayload = parseAndValidateHeader(responseBuf, 0);
        if (responsePayload > responseBuf.capacity() - DIRECT_FRAME_HEADER_SIZE) {
            throw new IllegalArgumentException("responseBuf too small for MCSI frame payload");
        }
        responseBuf.limit(DIRECT_FRAME_HEADER_SIZE + responsePayload);
        readFully(source, responseBuf, responsePayload);
        responseBuf.position(0);
        return decodeFrameAt(responseBuf, 0);
    }

    private static void stopProcess(Process process) {
        List<ProcessHandle> children = process.descendants().toList();
        for (ProcessHandle child : children) {
            child.destroy();
        }
        process.destroy();
        try {
            if (!process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
                process.waitFor(2, TimeUnit.SECONDS);
            }
            for (ProcessHandle child : children) {
                if (child.isAlive()) {
                    child.destroyForcibly();
                }
            }
        } catch (InterruptedException error) {
            process.destroyForcibly();
            Thread.currentThread().interrupt();
        }
    }

    @Override
    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        stopProcess(server);
        try {
            worldLock.release();
            lockChannel.close();
        } catch (IOException error) {
            System.err.println("mc-spine integrated: world lock cleanup: " + error.getMessage());
        }
        if (Thread.currentThread() != shutdownHook) {
            try {
                Runtime.getRuntime().removeShutdownHook(shutdownHook);
            } catch (IllegalStateException ignored) {
                // JVM shutdown already started.
            }
        }
    }

    /** Optional mod loader entry point; activated by an explicit Java property. */
    public static synchronized void startConfigured() {
        if (!Boolean.getBoolean("mc.spine.integrated.enabled")) {
            return;
        }
        if (configuredBridge != null) {
            return;
        }
        String binary = System.getProperty("mc.spine.integrated.serverBin",
                System.getenv("MC_SPINE_SERVER_BIN"));
        if (binary == null || binary.isBlank()) {
            throw new IllegalStateException("mc.spine.integrated.serverBin is required");
        }
        try {
            IntegratedServerBridge bridge = start(Path.of(binary),
                    Path.of(System.getProperty("mc.spine.integrated.gameDir", ".")),
                    System.getProperty("mc.spine.integrated.world", "mc-spine"),
                    System.getProperty("mc.spine.integrated.seed", "0"), LOOPBACK, 0,
                    Duration.ofSeconds(30));
            configuredBridge = bridge;
            Thread monitor = new Thread(() -> {
                try {
                    bridge.server.waitFor();
                } catch (InterruptedException error) {
                    Thread.currentThread().interrupt();
                }
                synchronized (IntegratedServerBridge.class) {
                    bridge.close();
                    if (configuredBridge == bridge) {
                        configuredBridge = null;
                    }
                }
            }, "mc-spine-integrated-monitor");
            monitor.setDaemon(true);
            monitor.start();
        } catch (IOException error) {
            throw new IllegalStateException("cannot start integrated server", error);
        }
    }

    /** Stop the mod-managed instance when the game/world lifecycle ends. */
    public static synchronized void closeConfigured() {
        if (configuredBridge != null) {
            configuredBridge.close();
            configuredBridge = null;
        }
    }

    /** Standalone launcher entry point, usable via java -cp JAR ...Bridge. */
    public static void main(String[] args) {
        try {
            Path binary = null;
            Path gameDir = null;
            String world = "mc-spine";
            String seed = "0";
            String bind = LOOPBACK;
            int port = 0;
            long timeout = 30_000;
            for (int i = 0; i < args.length; i++) {
                String key = args[i];
                if (i + 1 >= args.length) {
                    throw new IllegalArgumentException("missing value for " + key);
                }
                String value = args[++i];
                switch (key) {
                    case "--server-bin" -> binary = Path.of(value);
                    case "--game-dir" -> gameDir = Path.of(value);
                    case "--world" -> world = value;
                    case "--seed" -> seed = value;
                    case "--bind" -> bind = value;
                    case "--port" -> port = Integer.parseInt(value);
                    case "--timeout-ms" -> timeout = Long.parseLong(value);
                    default -> throw new IllegalArgumentException("unknown option " + key);
                }
            }
            try (IntegratedServerBridge bridge = start(binary, gameDir, world, seed, bind,
                    port, Duration.ofMillis(timeout))) {
                System.out.println("READY " + LOOPBACK + ":" + bridge.port() + " pid=" + bridge.pid());
                System.out.flush();
                CountDownLatch stop = new CountDownLatch(1);
                Thread console = new Thread(() -> {
                    try {
                        while (System.in.read() != -1) {
                            // EOF or any input requests clean shutdown.
                            break;
                        }
                    } catch (IOException ignored) {
                        // Broken parent pipe is equivalent to EOF.
                    } finally {
                        stop.countDown();
                    }
                }, "mc-spine-integrated-console");
                console.setDaemon(true);
                console.start();
                while (bridge.isAlive() && !stop.await(100, TimeUnit.MILLISECONDS)) {
                    // Monitor process death while accepting parent stdin shutdown.
                }
                if (!bridge.isAlive() && stop.getCount() != 0) {
                    throw new IOException("mc-spine exited after startup");
                }
            }
        } catch (Exception error) {
            System.err.println("mc-spine integrated: " + error.getMessage());
            System.exit(2);
        }
    }
}
