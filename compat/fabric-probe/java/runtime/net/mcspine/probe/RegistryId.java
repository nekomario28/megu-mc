package net.mcspine.probe;

import java.util.regex.Pattern;

public record RegistryId(String namespace, String path) {
    private static final Pattern NAMESPACE = Pattern.compile("[a-z0-9_.-]+");
    private static final Pattern PATH = Pattern.compile("[a-z0-9_./-]+");

    public RegistryId {
        if (!NAMESPACE.matcher(namespace).matches() || !PATH.matcher(path).matches()) {
            throw new IllegalArgumentException("Invalid registry identifier: " + namespace + ":" + path);
        }
    }

    public static RegistryId parse(String id) {
        int colon = id.indexOf(':');
        return colon == -1 ? new RegistryId("minecraft", id)
            : new RegistryId(id.substring(0, colon), id.substring(colon + 1));
    }

    @Override public String toString() { return namespace + ":" + path; }
}
