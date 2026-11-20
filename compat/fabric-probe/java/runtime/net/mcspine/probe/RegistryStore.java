package net.mcspine.probe;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.util.Identifier;

/** One bootstrap registry and freeze lock shared by both naming facades. */
public enum RegistryStore implements net.minecraft.registry.DefaultedRegistry<Object>,
                                     net.minecraft.core.DefaultedRegistry<Object> {
    BLOCK("minecraft:block"), ITEM("minecraft:item");

    private static boolean frozen;
    private static Thread owner;
    private final String name;
    private final Map<String, Object> entries = new LinkedHashMap<>();

    RegistryStore(String name) { this.name = name; }

    static synchronized void beginRegistration() {
        owner = Thread.currentThread();
    }

    public <T> T add(String id, T value) {
        synchronized (RegistryStore.class) {
            if (frozen) throw new IllegalStateException("Registry is frozen: " + name + " / " + id);
            if (Thread.currentThread() != owner) throw new IllegalStateException("Registration requires the bootstrap thread");
            if (entries.putIfAbsent(id, Objects.requireNonNull(value)) != null) {
                throw new IllegalArgumentException("Duplicate registry identifier: " + name + " / " + id);
            }
            return value;
        }
    }

    private Object lookup(String id) {
        synchronized (RegistryStore.class) {
            return entries.get(id);
        }
    }

    @Override public Object get(Identifier id) { return lookup(id.toString()); }
    @Override public Object get(ResourceLocation id) { return lookup(id.toString()); }

    static synchronized Map<String, List<String>> freeze() {
        frozen = true;
        owner = null;
        Map<String, List<String>> snapshot = new LinkedHashMap<>();
        for (RegistryStore registry : values()) {
            snapshot.put(registry.name, registry.entries.keySet().stream().sorted().toList());
        }
        return snapshot;
    }
}
