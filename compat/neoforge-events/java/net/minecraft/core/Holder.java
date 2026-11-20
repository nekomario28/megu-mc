package net.minecraft.core;

import java.util.Optional;
import net.minecraft.resources.ResourceKey;

public interface Holder<T> {
    T value();
    boolean isBound();
    Optional<ResourceKey<T>> unwrapKey();
}
