package net.minecraft.network.codec;

public interface StreamMemberEncoder<B, T> {
    void encode(T value, B buffer);
}
