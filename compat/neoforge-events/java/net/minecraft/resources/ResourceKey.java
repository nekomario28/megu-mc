package net.minecraft.resources;

import java.util.concurrent.ConcurrentHashMap;
import net.minecraft.core.Registry;

public final class ResourceKey<T> {
    private static final ResourceLocation ROOT = ResourceLocation.withDefaultNamespace("root");
    private static final ConcurrentHashMap<Identity, ResourceKey<?>> KEYS = new ConcurrentHashMap<>();
    private final Identity identity;

    private record Identity(ResourceLocation registry, ResourceLocation location) { }

    private ResourceKey(Identity identity) { this.identity = identity; }

    public static <T> ResourceKey<T> create(ResourceKey<? extends Registry<T>> registry, ResourceLocation location) {
        return intern(registry.location(), location);
    }

    public static <T> ResourceKey<Registry<T>> createRegistryKey(ResourceLocation location) {
        return intern(ROOT, location);
    }

    @SuppressWarnings("unchecked")
    private static <T> ResourceKey<T> intern(ResourceLocation registry, ResourceLocation location) {
        // The pair defines key identity. The type parameter is erased, as in the registry API.
        var identity = new Identity(java.util.Objects.requireNonNull(registry),
                                    java.util.Objects.requireNonNull(location));
        return (ResourceKey<T>) KEYS.computeIfAbsent(identity, ResourceKey::new);
    }

    public ResourceLocation location() { return identity.location(); }
    public ResourceLocation registry() { return identity.registry(); }

    @Override public String toString() { return "ResourceKey[" + registry() + " / " + location() + "]"; }
}
