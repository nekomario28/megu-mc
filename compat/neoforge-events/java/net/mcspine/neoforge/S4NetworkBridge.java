package net.mcspine.neoforge;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.function.Function;
import java.util.function.Supplier;
import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.PacketFlow;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.server.level.ServerPlayer;
import net.neoforged.neoforge.network.PacketDistributor;
import net.neoforged.neoforge.network.handling.IPayloadContext;
import net.neoforged.neoforge.network.handling.IPayloadHandler;

/** Owner-thread payload registration and synchronous dispatch/drain. */
public final class S4NetworkBridge {
    private static final int MAGIC = 0x00013453; // "S4", version 1, reserved zero
    private static final int MAX_CHANNEL = 256;
    private static final int MAX_RECORDS = 64;
    private static final int CAPACITY = 256 * 1024;
    private static final int MAX_DEPTH = 4;

    private enum Phase { PLAY, CONFIGURATION }
    private record Channel(Phase phase, ResourceLocation id) { }

    private record Registration<T extends CustomPacketPayload, B extends FriendlyByteBuf>(
            CustomPacketPayload.Type<T> type, String version, StreamCodec<? super B, T> codec,
            Function<byte[], B> bufferFactory, IPayloadHandler<T> handler, boolean inbound, boolean outbound) {
        T decode(byte[] bytes) {
            var buffer = bufferFactory.apply(bytes);
            try {
                T value = codec.decode(buffer);
                buffer.finish();
                if (value == null || !type.equals(value.type())) throw fail("S4 decoded payload type mismatch");
                return value;
            } finally {
                buffer.closeView();
            }
        }

        @SuppressWarnings("unchecked")
        byte[] encode(CustomPacketPayload payload) {
            var buffer = bufferFactory.apply(null);
            try {
                codec.encode(buffer, (T) payload);
                return buffer.finish();
            } finally {
                buffer.closeView();
            }
        }

        @SuppressWarnings("unchecked")
        void handle(CustomPacketPayload payload, IPayloadContext context) {
            handler.handle((T) payload, context);
        }
    }

    private static final Map<Channel, Registration<?, ?>> CHANNELS = new HashMap<>();
    private static volatile Thread owner;
    private static boolean registering;
    private static RuntimeException registrationFailure;
    private static final ThreadLocal<Dispatch> ACTIVE = new ThreadLocal<>();

    private S4NetworkBridge() { }

    static void beginRegistration() {
        owner = Thread.currentThread();
        registering = true;
    }

    public static <T extends CustomPacketPayload> void register(CustomPacketPayload.Type<T> type, String version,
            StreamCodec<? super RegistryFriendlyByteBuf, T> codec, IPayloadHandler<T> handler,
            boolean inbound, boolean outbound) {
        register(Phase.PLAY, type, version, codec, RegistryFriendlyByteBuf::new, handler, inbound, outbound);
    }

    public static <T extends CustomPacketPayload> void registerConfiguration(CustomPacketPayload.Type<T> type,
            String version, StreamCodec<? super FriendlyByteBuf, T> codec, IPayloadHandler<T> handler,
            boolean inbound, boolean outbound) {
        register(Phase.CONFIGURATION, type, version, codec, FriendlyByteBuf::new, handler, inbound, outbound);
    }

    private static <T extends CustomPacketPayload, B extends FriendlyByteBuf> void register(
            Phase phase, CustomPacketPayload.Type<T> type, String version, StreamCodec<? super B, T> codec,
            Function<byte[], B> bufferFactory, IPayloadHandler<T> handler, boolean inbound, boolean outbound) {
        if (Thread.currentThread() != owner || !registering) throw fail("S4 payload registration is closed");
        try {
            var id = type.id();
            if (id.toString().length() > MAX_CHANNEL || id.getNamespace().equals("minecraft")) {
                throw fail("Invalid S4 channel " + id);
            }
            var entry = new Registration<>(type, version, Objects.requireNonNull(codec), bufferFactory,
                Objects.requireNonNull(handler), inbound, outbound);
            if (CHANNELS.putIfAbsent(new Channel(phase, id), entry) != null) {
                throw fail("Duplicate S4 channel " + id + " in " + phase + " (version " + version + ")");
            }
        } catch (RuntimeException error) {
            registrationFailure = error;
            throw error;
        }
    }

    static void freezeRegistration() {
        registering = false;
        if (registrationFailure != null) throw new IllegalStateException("S4 payload registration failed", registrationFailure);
    }

    static void clearRegistration() {
        owner = null;
        registering = false;
        registrationFailure = null;
        CHANNELS.clear();
    }

    static void requireOutsideDispatch() {
        if (ACTIVE.get() != null) throw fail("Legacy native world access is unavailable during S4 dispatch");
    }

    private static IllegalStateException fail(String message) {
        var dispatch = ACTIVE.get();
        if (dispatch != null) dispatch.failed = true;
        return new IllegalStateException(message);
    }

    private static Registration<?, ?> registration(Phase phase, ResourceLocation id, boolean inbound) {
        var registration = CHANNELS.get(new Channel(phase, id));
        if (registration == null || !(inbound ? registration.inbound : registration.outbound)) {
            throw fail("Unregistered S4 " + (inbound ? "C2S" : "S2C") + " channel " + id + " in " + phase);
        }
        return registration;
    }

    public static void send(Integer target, CustomPacketPayload first, CustomPacketPayload[] rest) {
        var dispatch = ACTIVE.get();
        if (dispatch == null) throw fail("S4 send requires an active payload dispatch");
        dispatch.checkContext();
        if (dispatch.encoding || rest.length >= MAX_RECORDS - dispatch.count) {
            throw fail("S4 outbound batch record limit or recursive encoding");
        }
        dispatch.encoding = true;
        try {
            dispatch.append(target, first);
            for (var payload : rest) dispatch.append(target, payload);
        } catch (RuntimeException | Error error) {
            dispatch.failed = true;
            throw error;
        } finally {
            dispatch.encoding = false;
        }
    }

    private static final class Dispatch implements IPayloadContext {
        final Phase phase;
        final ServerPlayer player;
        final ByteBuffer output = ByteBuffer.allocate(CAPACITY).order(ByteOrder.LITTLE_ENDIAN);
        int count;
        int depth;
        boolean encoding;
        boolean failed;

        Dispatch(Phase phase, ServerPlayer player) {
            this.phase = phase;
            this.player = player;
            output.putInt(MAGIC).putShort((short) 0);
        }

        void checkContext() {
            if (ACTIVE.get() != this || failed || depth == 0) throw fail("S4 payload context expired");
        }

        void append(Integer target, CustomPacketPayload payload) {
            var entry = registration(phase, payload.type().id(), false);
            if (!entry.type.equals(payload.type())) throw fail("S4 outgoing payload type mismatch");
            byte[] channel = entry.type.id().toString().getBytes(StandardCharsets.US_ASCII);
            byte[] bytes = entry.encode(payload);
            if (output.remaining() < 2 + channel.length + 1 + (target == null ? 0 : 4) + 4 + bytes.length) {
                throw fail("S4 outbound batch capacity exceeded");
            }
            output.putShort((short) channel.length).put(channel).put((byte) (target == null ? 0 : 1));
            if (target != null) output.putInt(target);
            output.putInt(bytes.length).put(bytes);
            count++;
        }

        void invoke(CustomPacketPayload payload) {
            if (depth == MAX_DEPTH || encoding) throw fail("S4 payload dispatch depth exceeds 4 or codec is active");
            depth++;
            try {
                registration(phase, payload.type().id(), true).handle(payload, this);
            } catch (RuntimeException | Error error) {
                failed = true;
                throw error;
            } finally {
                depth--;
            }
        }

        public ServerPlayer player() { checkContext(); return player; }
        public PacketFlow flow() { checkContext(); return PacketFlow.SERVERBOUND; }

        public void reply(CustomPacketPayload payload) { PacketDistributor.sendToPlayer(player(), payload); }

        public void handle(CustomPacketPayload payload) {
            checkContext();
            invoke(payload);
        }

        public CompletableFuture<Void> enqueueWork(Runnable task) {
            return enqueueWork(() -> { task.run(); return null; });
        }

        public <T> CompletableFuture<T> enqueueWork(Supplier<T> task) {
            checkContext();
            try {
                return CompletableFuture.completedFuture(task.get());
            } catch (RuntimeException | Error error) {
                failed = true;
                return CompletableFuture.failedFuture(error);
            }
        }
    }

    private static byte[] dispatch(byte[] frame, int playerEid, String playerName) {
        return dispatch(Phase.PLAY, frame, playerEid, playerName);
    }

    // The host selects the protocol phase. Payload frames carry no phase selector.
    private static byte[] dispatchConfiguration(byte[] frame, int playerEid, String playerName) {
        return dispatch(Phase.CONFIGURATION, frame, playerEid, playerName);
    }

    private static byte[] dispatch(Phase phase, byte[] frame, int playerEid, String playerName) {
        if (Thread.currentThread() != owner || registering || ACTIVE.get() != null) {
            throw fail("S4 dispatch requires the frozen JVM owner outside another dispatch");
        }
        if (frame.length > 13 + MAX_CHANNEL + FriendlyByteBuf.MAX_PAYLOAD || frame.length < 13) {
            throw fail("Invalid S4 inbound frame size");
        }
        var input = ByteBuffer.wrap(frame).order(ByteOrder.LITTLE_ENDIAN);
        if (input.getInt() != MAGIC || input.getShort() != 1) throw fail("Invalid S4 inbound frame header");
        int channelLength = Short.toUnsignedInt(input.getShort());
        if (channelLength == 0 || channelLength > MAX_CHANNEL || input.remaining() < channelLength + 5) {
            throw fail("Invalid or truncated S4 channel");
        }
        byte[] channel = new byte[channelLength];
        input.get(channel);
        String name = new String(channel, StandardCharsets.US_ASCII);
        var id = ResourceLocation.parse(name);
        if (!id.toString().equals(name) || input.get() != 0) throw fail("Invalid S4 C2S channel or target");
        int length = input.getInt();
        if (length < 0 || length > FriendlyByteBuf.MAX_PAYLOAD || length != input.remaining()) {
            throw fail("Truncated or oversized S4 payload frame");
        }
        var entry = registration(phase, id, true);
        byte[] bytes = new byte[length];
        input.get(bytes);
        var dispatch = new Dispatch(phase, new ServerPlayer(playerEid, playerName));
        ACTIVE.set(dispatch);
        try {
            dispatch.invoke(entry.decode(bytes));
            if (dispatch.failed) throw fail("S4 payload dispatch failed");
            dispatch.output.putShort(4, (short) dispatch.count);
            return java.util.Arrays.copyOf(dispatch.output.array(), dispatch.output.position());
        } finally {
            ACTIVE.remove();
        }
    }
}
