package net.minecraft.network.protocol.common.custom;

import java.util.Objects;
import net.minecraft.resources.ResourceLocation;

public interface CustomPacketPayload {
    Type<? extends CustomPacketPayload> type();

    record Type<T extends CustomPacketPayload>(ResourceLocation id) {
        public Type { Objects.requireNonNull(id); }
    }
}
