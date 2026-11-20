package net.minecraft.world;

import net.minecraft.world.item.ItemStack;

public interface Container {
    int getContainerSize();
    int getMaxStackSize();
    ItemStack getItem(int slot);
    void setItem(int slot, ItemStack stack);
    boolean canPlaceItem(int slot, ItemStack stack);
}
