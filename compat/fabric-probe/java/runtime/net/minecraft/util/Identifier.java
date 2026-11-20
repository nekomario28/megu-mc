package net.minecraft.util;

import net.mcspine.probe.RegistryId;

public final class Identifier {
    private final RegistryId id;

    private Identifier(RegistryId id) { this.id = id; }

    public static Identifier of(String namespace, String path) {
        return new Identifier(new RegistryId(namespace, path));
    }

    public static Identifier of(String id) {
        return new Identifier(RegistryId.parse(id));
    }

    public static Identifier ofVanilla(String path) {
        return of("minecraft", path);
    }

    public String getNamespace() { return id.namespace(); }
    public String getPath() { return id.path(); }

    @Override public boolean equals(Object other) {
        return other instanceof Identifier location && id.equals(location.id);
    }

    @Override public int hashCode() { return id.hashCode(); }
    @Override public String toString() { return id.toString(); }
}
