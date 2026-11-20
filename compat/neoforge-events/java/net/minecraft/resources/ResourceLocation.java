package net.minecraft.resources;

import java.util.regex.Pattern;

public final class ResourceLocation {
    private static final Pattern NAMESPACE = Pattern.compile("[a-z0-9_.-]+");
    private static final Pattern PATH = Pattern.compile("[a-z0-9_./-]+");
    private final String namespace;
    private final String path;

    private ResourceLocation(String namespace, String path) {
        if (!NAMESPACE.matcher(namespace).matches() || !PATH.matcher(path).matches()) {
            throw new IllegalArgumentException("Invalid resource location: " + namespace + ":" + path);
        }
        this.namespace = namespace;
        this.path = path;
    }

    public static ResourceLocation fromNamespaceAndPath(String namespace, String path) {
        return new ResourceLocation(namespace, path);
    }

    public static ResourceLocation parse(String name) {
        int separator = name.indexOf(':');
        return separator < 0 ? withDefaultNamespace(name)
            : fromNamespaceAndPath(name.substring(0, separator), name.substring(separator + 1));
    }

    public static ResourceLocation withDefaultNamespace(String path) {
        return fromNamespaceAndPath("minecraft", path);
    }

    public String getNamespace() { return namespace; }
    public String getPath() { return path; }

    @Override public boolean equals(Object other) {
        return other instanceof ResourceLocation location
            && namespace.equals(location.namespace) && path.equals(location.path);
    }

    @Override public int hashCode() { return 31 * namespace.hashCode() + path.hashCode(); }
    @Override public String toString() { return namespace + ":" + path; }
}
