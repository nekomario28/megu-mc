package net.neoforged.neoforge.network.registration;

import net.megu.neoforge.S4NetworkBridge;
import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.neoforged.neoforge.network.handling.IPayloadHandler;

public class PayloadRegistrar {
    private final String version;

    public PayloadRegistrar(String version) {
        if (version == null || version.isBlank() || version.length() > 128) {
            throw new IllegalArgumentException("Invalid S4 channel version");
        }
        this.version = version;
    }

    public PayloadRegistrar versioned(String version) { return new PayloadRegistrar(version); }

    public <T extends CustomPacketPayload> PayloadRegistrar playToServer(CustomPacketPayload.Type<T> type,
            StreamCodec<? super RegistryFriendlyByteBuf, T> codec, IPayloadHandler<T> handler) {
        S4NetworkBridge.register(type, version, codec, handler, true, false);
        return this;
    }

    public <T extends CustomPacketPayload> PayloadRegistrar playToClient(CustomPacketPayload.Type<T> type,
            StreamCodec<? super RegistryFriendlyByteBuf, T> codec, IPayloadHandler<T> handler) {
        S4NetworkBridge.register(type, version, codec, handler, false, true);
        return this;
    }

    public <T extends CustomPacketPayload> PayloadRegistrar playBidirectional(CustomPacketPayload.Type<T> type,
            StreamCodec<? super RegistryFriendlyByteBuf, T> codec, IPayloadHandler<T> handler) {
        S4NetworkBridge.register(type, version, codec, handler, true, true);
        return this;
    }

    public <T extends CustomPacketPayload> PayloadRegistrar configurationToClient(CustomPacketPayload.Type<T> type,
            StreamCodec<? super FriendlyByteBuf, T> codec, IPayloadHandler<T> handler) {
        S4NetworkBridge.registerConfiguration(type, version, codec, handler, false, true);
        return this;
    }

    public <T extends CustomPacketPayload> PayloadRegistrar configurationToServer(CustomPacketPayload.Type<T> type,
            StreamCodec<? super FriendlyByteBuf, T> codec, IPayloadHandler<T> handler) {
        S4NetworkBridge.registerConfiguration(type, version, codec, handler, true, false);
        return this;
    }
}
