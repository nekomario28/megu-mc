package net.megu.neoforge;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.CharBuffer;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.entity.BlockEntityTicker;
import net.minecraft.world.level.block.state.BlockState;

/** Java endpoint of the S3 owner-thread tick batch. See the adapter README for the ABI. */
public final class DirectBatchBridge {
    private static final int CAPACITY = 256 * 1024;
    private static final int MAX_RECORDS = 4096;
    private static final int MAX_MESSAGE = 4096;
    private static final int HEADER_SIZE = 16;
    private static final int REQUEST_SIZE = 28;
    private static final int REQUEST_HEADER = 0x00013353;
    private static final int RESPONSE_HEADER = 0x01013353;

    public enum Activation { ENTITY_TICK, BLOCK_ENTITY_TICK, CONTAINER, SCHEDULED_BLOCK_TICK }
    private static final Activation[] ACTIVATIONS = Activation.values();

    @FunctionalInterface
    public interface Callback {
        void tick(TickContext context);
    }

    private record Registry(Thread owner, Map<Integer, List<Callback>> callbacks) { }
    private static final Map<Integer, List<Callback>> registrations = new HashMap<>();
    private static Thread registering;
    private static volatile Registry registry;
    private static final ThreadLocal<Batch> ACTIVE = new ThreadLocal<>();

    private DirectBatchBridge() { }

    static synchronized void beginRegistration() {
        if (registering != null || registry != null) {
            throw new IllegalStateException("S3 registration already initialized");
        }
        registering = Thread.currentThread();
    }

    /** Register a callback for one activation and unsigned 16-bit kind during bootstrap. */
    public static synchronized void register(Activation activation, int kind, Callback callback) {
        if (registering != Thread.currentThread()) {
            throw new IllegalStateException("S3 callback registration requires the bootstrap owner");
        }
        if (kind < 0 || kind > 0xffff) throw new IllegalArgumentException("Invalid S3 kind");
        registrations.computeIfAbsent(key(activation, kind), ignored -> new ArrayList<>())
            .add(Objects.requireNonNull(callback));
    }

    /** Register a ticker for the bridge's block entity view of an S3 kind. */
    public static void registerBlockEntityTicker(int kind, BlockEntityTicker<? super BlockEntityHandle> ticker) {
        Objects.requireNonNull(ticker);
        register(Activation.BLOCK_ENTITY_TICK, kind, context -> {
            Level level = context.level();
            BlockPos pos = new BlockPos(context.x(), context.y(), context.z());
            ticker.tick(level, pos, level.getBlockState(pos), context.blockEntity());
        });
    }

    static synchronized void freezeRegistration() {
        registrations.replaceAll((key, callbacks) -> List.copyOf(callbacks));
        registry = new Registry(registering, Map.copyOf(registrations));
        registering = null;
        registrations.clear();
    }

    static synchronized void clearRegistration() {
        registry = null;
        registering = null;
        registrations.clear();
    }

    /** Frozen callback count; an unfrozen registry saturates so the host keeps dispatching. */
    static synchronized int callbackCount() {
        Registry frozen = registry;
        if (frozen == null) return Integer.MAX_VALUE;
        long count = 0;
        for (List<Callback> callbacks : frozen.callbacks.values()) count += callbacks.size();
        return (int) Math.min(count, Integer.MAX_VALUE);
    }

    static void requireOutsideBatch() {
        Batch active = ACTIVE.get();
        if (active != null) throw fail(active, "Legacy native world access is unavailable during S3 dispatch");
    }

    static void requireOwner() {
        Registry activeRegistry = registry;
        if (activeRegistry == null || activeRegistry.owner != Thread.currentThread()) {
            throw new IllegalStateException("NeoForge dispatch requires the bootstrap owner");
        }
    }

    private static int key(Activation activation, int kind) {
        return (activation.ordinal() << 16) | kind;
    }

    private record Input(Activation activation, int dim, int kind, int id, int generation,
                         int x, int y, int z, int state) { }

    public static final class EntityHandle {
        private final TickContext context;
        private EntityHandle(TickContext context) { this.context = context; }
        public int dim() { return context.checked().dim; }
        public int kind() { return context.checked().kind; }
        public int id() { return context.checked().id; }
        public int generation() { return context.checked().generation; }
    }

    public static final class BlockEntityHandle extends BlockEntity {
        private final TickContext context;
        private BlockEntityHandle(TickContext context) { this.context = context; }
        @Override protected void checkActivation() { context.checked(); }
        @Override public Level getLevel() { return context.level(); }
        public boolean dirty() { return isDirty(); }
        public int dim() { return context.checked().dim; }
        public int kind() { return context.checked().kind; }
        public int id() { return context.checked().id; }
        public int generation() { return context.checked().generation; }
    }

    public static final class TickContext {
        private final Batch batch;
        private final Input input;
        private final int index;
        private Level level;
        private BlockEntityHandle blockEntity;

        private TickContext(Batch batch, Input input, int index) {
            this.batch = batch;
            this.input = input;
            this.index = index;
        }

        private Input checked() {
            if (Thread.currentThread() != batch.owner || batch.current != this
                    || batch.generation != input.generation || batch.failed) {
                throw fail(batch, "S3 activation or handle expired");
            }
            return input;
        }

        public Activation activation() { return checked().activation; }
        public int x() { return checked().x; }
        public int y() { return checked().y; }
        public int z() { return checked().z; }
        public int state() { return checked().state; }

        public Level level() {
            Input activation = checked();
            if (level == null) {
                level = new Level(activation.dim, this);
                level.snapshotBlock(new BlockPos(activation.x, activation.y, activation.z),
                    new BlockState(activation.state));
            }
            return level;
        }

        public EntityHandle entity() {
            if (checked().activation != Activation.ENTITY_TICK) {
                throw fail(batch, "S3 activation has no entity handle");
            }
            return new EntityHandle(this);
        }

        public BlockEntityHandle blockEntity() {
            Activation activation = checked().activation;
            if (activation != Activation.BLOCK_ENTITY_TICK && activation != Activation.CONTAINER) {
                throw fail(batch, "S3 activation has no block entity handle");
            }
            if (blockEntity == null) blockEntity = new BlockEntityHandle(this);
            return blockEntity;
        }

        /** Invoke the current activation's callbacks again within the same batch. */
        public void redispatch() {
            batch.invoke(this);
        }

        public void setBlockIf(int x, int y, int z, int newState, int expectedState) {
            beginPositionedOp(1, 21, x, y, z).putInt(newState).putInt(expectedState);
        }

        public void scheduleTick(int x, int y, int z, int delay, int priority) {
            checked();
            if (delay < 0 || priority < -3 || priority > 3) {
                throw fail(batch, "Invalid S3 tick delay or priority");
            }
            beginPositionedOp(2, 18, x, y, z).putInt(delay).put((byte) priority);
        }

        public void containerSlotDelta(int x, int y, int z, int slot, int itemId, int count) {
            checked();
            if (slot < 0 || slot > 0xffff || count < 0 || count > 99 || (itemId == 0) != (count == 0)) {
                throw fail(batch, "Invalid S3 container slot or stack");
            }
            beginPositionedOp(3, 20, x, y, z).putShort((short) slot).putInt(itemId).put((byte) count);
        }

        public void sendSystemMessage(int playerEid, String message) {
            checked();
            if (message == null || message.length() > MAX_MESSAGE) {
                throw fail(batch, "Invalid S3 message length");
            }
            ByteBuffer out = batch.beginOp(this, 4, 6).putInt(playerEid);
            int lengthOffset = out.position();
            out.putShort((short) 0);
            int start = out.position();
            // Encode UTF-8 in place. The cap stays in bytes even for multibyte text.
            int limit = out.limit();
            out.limit(Math.min(limit, start + MAX_MESSAGE));
            try {
                var encoder = StandardCharsets.UTF_8.newEncoder();
                if (!encoder.encode(CharBuffer.wrap(message), out, true).isUnderflow()
                        || !encoder.flush(out).isUnderflow()) {
                    throw fail(batch, "Invalid or oversized S3 UTF-8 message");
                }
            } finally {
                out.limit(limit);
            }
            out.putShort(lengthOffset, (short) (out.position() - start));
        }

        private ByteBuffer beginPositionedOp(int tag, int payload, int x, int y, int z) {
            return batch.beginOp(this, tag, payload)
                .put((byte) input.dim).putInt(x).putInt(y).putInt(z);
        }
    }

    /** The v1 wire implies the block identity from the source activation's cell. */
    public static void scheduleBlockTick(TickContext context, Level level, BlockPos pos, Block block, int delay) {
        if (context == null) {
            String message = "scheduleTick requires an active S3 level snapshot";
            Batch active = ACTIVE.get();
            throw active == null ? new IllegalStateException(message) : fail(active, message);
        }
        Input input = context.checked();
        if (context.level != level || pos == null || pos.x() != input.x || pos.y() != input.y || pos.z() != input.z
                || block == null || S4CapabilityBridge.blockForState(input.state) != block) {
            throw fail(context.batch, "scheduleTick requires the current cell and its registered block");
        }
        context.scheduleTick(input.x, input.y, input.z, delay, 0);
    }

    private static IllegalStateException fail(Batch batch, String message) {
        batch.markFailed();
        // Using a retained handle can also invalidate the batch currently on this thread.
        Batch active = ACTIVE.get();
        if (active != null && active != batch) active.markFailed();
        return new IllegalStateException(message);
    }

    private static final class Batch {
        final Thread owner = Thread.currentThread();
        ByteBuffer output;
        List<Callback> callbacks;
        volatile boolean failed;
        TickContext current;
        int generation;
        int count;
        int depth;

        Batch(ByteBuffer output) { this.output = output; }

        synchronized void markFailed() { failed = true; }

        synchronized int publish(Registry activeRegistry, long sequence) {
            if (failed || registry != activeRegistry) throw new IllegalStateException("S3 batch failed");
            output.putInt(4, count).putLong(8, sequence).putInt(0, RESPONSE_HEADER);
            return output.position();
        }

        ByteBuffer beginOp(TickContext context, int opcode, int payload) {
            context.checked();
            if (count == MAX_RECORDS || output.remaining() < 9 + payload) {
                throw fail(this, "S3 response capacity exceeded");
            }
            count++;
            return output.put((byte) opcode).putInt(context.index).putInt(generation);
        }

        void invoke(TickContext context) {
            context.checked();
            if (depth >= MutationBuffer.MAX_REENTRANCY_DEPTH) {
                throw fail(this, "S3 reentrancy depth exceeds " + MutationBuffer.MAX_REENTRANCY_DEPTH);
            }
            depth++;
            try {
                for (Callback callback : callbacks) {
                    callback.tick(context);
                    if (failed) throw fail(this, "S3 batch failed");
                }
            } catch (RuntimeException | Error error) {
                markFailed();
                throw error;
            } finally {
                depth--;
            }
        }
    }

    /** JNI-only entry point. These views are borrowed for this call and never escape into mod code. */
    private static int dispatch(ByteBuffer request, int requestLength, ByteBuffer response) {
        Batch active = ACTIVE.get();
        if (active != null) throw fail(active, "S3 batch is already active");
        if (!response.isDirect() || response.isReadOnly() || response.capacity() != CAPACITY) {
            throw new IllegalArgumentException("Invalid S3 direct response buffer");
        }
        ByteBuffer out = response.duplicate().clear().order(ByteOrder.LITTLE_ENDIAN);
        out.putInt(0, 0); // No prior or partial response can be consumed after failure.
        Registry callbacks = registry;
        if (callbacks == null || callbacks.owner != Thread.currentThread()) {
            throw new IllegalStateException("S3 dispatch requires the bootstrap owner");
        }
        if (!request.isDirect() || request.capacity() != CAPACITY
                || requestLength < HEADER_SIZE || requestLength > CAPACITY) {
            throw new IllegalArgumentException("Invalid S3 direct request buffer or length");
        }
        ByteBuffer in = request.asReadOnlyBuffer().clear().order(ByteOrder.LITTLE_ENDIAN).limit(requestLength);
        if (in.getInt() != REQUEST_HEADER) throw new IllegalArgumentException("Invalid S3 request header");
        int count = in.getInt();
        long sequence = in.getLong();
        if (sequence == 0 || count < 0 || count > MAX_RECORDS || in.remaining() != count * REQUEST_SIZE) {
            throw new IllegalArgumentException("Invalid S3 request sequence, count or size");
        }
        // Preflight all headers before invoking the first callback.
        for (int offset = HEADER_SIZE; offset < requestLength; offset += REQUEST_SIZE) {
            if (Byte.toUnsignedInt(in.get(offset)) >= ACTIVATIONS.length
                    || Byte.toUnsignedInt(in.get(offset + 1)) > 2 || in.getInt(offset + 8) == 0) {
                throw new IllegalArgumentException("Invalid S3 activation identity");
            }
        }

        out.position(HEADER_SIZE);
        Batch batch = new Batch(out);
        ACTIVE.set(batch);
        try {
            for (int index = 0; index < count; index++) {
                Input input = new Input(ACTIVATIONS[in.get()], in.get(), Short.toUnsignedInt(in.getShort()),
                    in.getInt(), in.getInt(), in.getInt(), in.getInt(), in.getInt(), in.getInt());
                batch.callbacks = callbacks.callbacks.get(key(input.activation, input.kind));
                if (batch.callbacks == null) continue;
                batch.generation = input.generation;
                batch.current = new TickContext(batch, input, index);
                batch.invoke(batch.current);
                batch.current = null;
            }
            return batch.publish(callbacks, sequence);
        } finally {
            batch.current = null;
            batch.callbacks = null;
            batch.output = null; // A retained, invalid context must not keep native memory reachable.
            ACTIVE.remove();
        }
    }
}
