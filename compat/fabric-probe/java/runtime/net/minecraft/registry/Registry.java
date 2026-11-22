package net.minecraft.registry;

import net.megu.probe.RegistryStore;
import net.minecraft.util.Identifier;

public interface Registry<T> {
    T get(Identifier id);

    static <V, T extends V> T register(Registry<V> registry, Identifier id, T value) {
        return ((RegistryStore) registry).add(id.toString(), value);
    }

    static <V, T extends V> T register(Registry<V> registry, String id, T value) {
        return register(registry, Identifier.of(id), value);
    }
}
