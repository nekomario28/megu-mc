package net.neoforged.neoforge.capabilities;

import net.minecraft.core.Direction;
import net.neoforged.neoforge.items.IItemHandler;

public final class Capabilities {
    private Capabilities() { }

    public static final class ItemHandler {
        public static final BlockCapability<IItemHandler, Direction> BLOCK = new BlockCapability<>();
        private ItemHandler() { }
    }
}
