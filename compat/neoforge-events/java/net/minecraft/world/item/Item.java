package net.minecraft.world.item;

import java.util.Objects;

public class Item implements net.minecraft.world.level.ItemLike {
    public Item(Properties properties) { Objects.requireNonNull(properties); }

    @Override public Item asItem() { return this; }

    public static class Properties { }
}
