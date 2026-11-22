package net.minecraft.resources;

import net.megu.probe.RegistryId;

public final class ResourceLocation {
    private final RegistryId id;

    private ResourceLocation(RegistryId id) { this.id = id; }

    public static ResourceLocation fromNamespaceAndPath(String namespace, String path) {
        return new ResourceLocation(new RegistryId(namespace, path));
    }

    public static ResourceLocation parse(String id) {
        return new ResourceLocation(RegistryId.parse(id));
    }

    public static ResourceLocation withDefaultNamespace(String path) {
        return fromNamespaceAndPath("minecraft", path);
    }

    public String getNamespace() { return id.namespace(); }
    public String getPath() { return id.path(); }

    @Override public boolean equals(Object other) {
        return other instanceof ResourceLocation location && id.equals(location.id);
    }

    @Override public int hashCode() { return id.hashCode(); }
    @Override public String toString() { return id.toString(); }
}
