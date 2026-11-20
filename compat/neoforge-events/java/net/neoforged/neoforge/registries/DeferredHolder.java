package net.neoforged.neoforge.registries;

import java.util.Optional;
import java.util.function.Supplier;
import net.minecraft.core.Holder;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;

public class DeferredHolder<R, T extends R> implements Holder<R>, Supplier<T> {
    private final ResourceKey<R> key;
    private T value;

    DeferredHolder(ResourceKey<R> key) { this.key = key; }

    final void bind(T value) { this.value = value; }

    @Override public T get() { return value(); }
    @Override public T value() {
        if (!isBound()) throw new IllegalStateException("Unbound registry holder: " + key);
        return value;
    }

    @Override public boolean isBound() { return value != null; }
    @Override public Optional<ResourceKey<R>> unwrapKey() { return Optional.of(key); }
    public Optional<T> asOptional() { return Optional.ofNullable(value); }
    public ResourceLocation getId() { return key.location(); }
    public ResourceKey<R> getKey() { return key; }
}
