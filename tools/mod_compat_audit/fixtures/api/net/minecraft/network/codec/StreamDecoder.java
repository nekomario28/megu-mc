package net.minecraft.network.codec;

public interface StreamDecoder<B, T> {
    T decode(B buffer);
}
