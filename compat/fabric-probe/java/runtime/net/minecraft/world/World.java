package net.minecraft.world;

import net.minecraft.registry.RegistryKey;

public final class World {
    private static final RegistryKey<World> KEY = new RegistryKey<>();

    public RegistryKey<World> getRegistryKey() {
        return KEY;
    }
}
