package net.neoforged.neoforge.items.wrapper;

import java.util.Objects;
import net.minecraft.world.Container;
import net.minecraft.world.item.ItemStack;
import net.neoforged.neoforge.items.IItemHandler;

/** Item handler for a component-free snapshot container. */
public class InvWrapper implements IItemHandler {
    private final Container container;

    public InvWrapper(Container container) { this.container = Objects.requireNonNull(container); }

    public int getSlots() { return container.getContainerSize(); }
    public ItemStack getStackInSlot(int slot) { return container.getItem(slot); }
    public int getSlotLimit(int slot) { return container.getMaxStackSize(); }
    public boolean isItemValid(int slot, ItemStack stack) { return container.canPlaceItem(slot, stack); }

    public ItemStack insertItem(int slot, ItemStack stack, boolean simulate) {
        if (stack.isEmpty() || !isItemValid(slot, stack)) return stack;
        ItemStack current = getStackInSlot(slot);
        if (!current.isEmpty() && !ItemStack.isSameItemSameComponents(current, stack)) return stack;
        int inserted = Math.min(stack.getCount(), Math.max(0, getSlotLimit(slot) - current.getCount()));
        if (!simulate && inserted > 0) container.setItem(slot, stack.copyWithCount(current.getCount() + inserted));
        return stack.copyWithCount(stack.getCount() - inserted);
    }

    public ItemStack extractItem(int slot, int amount, boolean simulate) {
        if (amount <= 0) return ItemStack.EMPTY;
        ItemStack current = getStackInSlot(slot);
        int extracted = Math.min(amount, current.getCount());
        if (!simulate && extracted > 0) container.setItem(slot, current.copyWithCount(current.getCount() - extracted));
        return current.copyWithCount(extracted);
    }
}
