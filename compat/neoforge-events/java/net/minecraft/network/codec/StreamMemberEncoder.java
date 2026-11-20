package net.minecraft.network.codec;

@FunctionalInterface
public interface StreamMemberEncoder<B, V> {
    void encode(V value, B buffer);
}
