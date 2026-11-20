package net.minecraft.core;

import net.mcspine.probe.RegistryStore;
import net.minecraft.resources.ResourceLocation;

public interface Registry<T> {
    T get(ResourceLocation id);

    static <V, T extends V> T register(Registry<V> registry, ResourceLocation id, T value) {
        return ((RegistryStore) registry).add(id.toString(), value);
    }

    static <V, T extends V> T register(Registry<V> registry, String id, T value) {
        return register(registry, ResourceLocation.parse(id), value);
    }
}
