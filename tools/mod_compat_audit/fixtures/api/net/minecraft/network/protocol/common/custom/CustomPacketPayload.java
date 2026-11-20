package net.minecraft.network.protocol.common.custom;

import net.minecraft.resources.ResourceLocation;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.codec.StreamDecoder;
import net.minecraft.network.codec.StreamMemberEncoder;
public interface CustomPacketPayload {
    Type<? extends CustomPacketPayload> type();

    static <B, T extends CustomPacketPayload> StreamCodec<B, T> codec(
            StreamMemberEncoder<B, T> encoder, StreamDecoder<B, T> decoder) {
        throw new UnsupportedOperationException();
    }

    record Type<T extends CustomPacketPayload>(ResourceLocation id) {}
}
