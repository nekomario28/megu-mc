package net.minecraft.network.codec;

public interface StreamCodec<B, V> extends StreamDecoder<B, V>, StreamEncoder<B, V> {
    static <B, V> StreamCodec<B, V> of(StreamEncoder<B, V> encoder, StreamDecoder<B, V> decoder) {
        return new StreamCodec<>() {
            public V decode(B buffer) { return decoder.decode(buffer); }
            public void encode(B buffer, V value) { encoder.encode(buffer, value); }
        };
    }

    static <B, V> StreamCodec<B, V> ofMember(StreamMemberEncoder<B, V> encoder, StreamDecoder<B, V> decoder) {
        return of((buffer, value) -> encoder.encode(value, buffer), decoder);
    }
}
