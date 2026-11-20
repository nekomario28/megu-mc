package net.minecraft.network.protocol.common.custom;

import java.util.Objects;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.codec.StreamDecoder;
import net.minecraft.network.codec.StreamMemberEncoder;
import net.minecraft.resources.ResourceLocation;

public interface CustomPacketPayload {
    Type<? extends CustomPacketPayload> type();

    static <B, T extends CustomPacketPayload> StreamCodec<B, T> codec(
            StreamMemberEncoder<B, T> encoder, StreamDecoder<B, T> decoder) {
        return StreamCodec.ofMember(encoder, decoder);
    }

    record Type<T extends CustomPacketPayload>(ResourceLocation id) {
        public Type { Objects.requireNonNull(id); }
    }
}
