package net.minecraft.world.item;

import java.util.Objects;
import net.minecraft.world.level.ItemLike;

/** Component-free item stack. Counts and registry membership are validated on the bridge. */
public final class ItemStack {
    public static final ItemStack EMPTY = new ItemStack();
    private final Item item;
    private int count;

    private ItemStack() { item = null; }
    public ItemStack(ItemLike item) { this(item, 1); }
    public ItemStack(ItemLike item, int count) {
        this.item = Objects.requireNonNull(item.asItem());
        setCount(count);
    }

    public boolean isEmpty() { return count == 0; }
    public Item getItem() { return item; }
    public int getCount() { return count; }
    public void setCount(int count) {
        if (item == null && count != 0 || count < 0 || count > 99) {
            throw new IllegalArgumentException("Invalid S4 item count");
        }
        this.count = count;
    }
    public ItemStack copy() { return copyWithCount(count); }
    public ItemStack copyWithCount(int count) { return count == 0 ? EMPTY : new ItemStack(item, count); }
    public boolean is(Item type) { return !isEmpty() && item == type; }
    public static boolean isSameItemSameComponents(ItemStack left, ItemStack right) { return left.item == right.item; }
}
