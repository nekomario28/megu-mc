package net.neoforged.neoforge.registries;

import net.minecraft.resources.ResourceKey;
import net.minecraft.world.item.Item;

public class DeferredItem<T extends Item> extends DeferredHolder<Item, T> implements net.minecraft.world.level.ItemLike {
    DeferredItem(ResourceKey<Item> key) { super(key); }
    @Override public Item asItem() { return get(); }
}
