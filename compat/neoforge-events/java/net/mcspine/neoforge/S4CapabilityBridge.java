package net.mcspine.neoforge;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.world.Container;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.neoforge.capabilities.BlockCapability;
import net.neoforged.neoforge.capabilities.Capabilities;
import net.neoforged.neoforge.capabilities.IBlockCapabilityProvider;

/** Resolves providers on the owner from a copied container and returns proposed slot replacements. */
public final class S4CapabilityBridge {
    private static final int MAX_SLOTS = 256;
    private static final int MAX_OPERATIONS = 256;
    private static final int FIRST_MOD_ITEM = 1333;
    private static final Direction[] DIRECTIONS = Direction.values();

    private static final Map<Integer, Block> BLOCKS = new HashMap<>();
    private static final Map<Integer, Item> ITEMS = new HashMap<>();
    private static final Map<Item, Integer> ITEM_IDS = new IdentityHashMap<>();
    private static final Map<Block, List<IBlockCapabilityProvider<?, ?>>> PROVIDERS = new IdentityHashMap<>();
    private static final ThreadLocal<Snapshot> ACTIVE = new ThreadLocal<>();
    private static volatile Thread owner;
    private static boolean registering;
    private static RuntimeException registrationFailure;

    private S4CapabilityBridge() { }

    static void bindBlock(int id, Block block) { BLOCKS.put(id, block); }
    static void bindItem(int id, Item item) {
        // S2 allows aliases; a stack cannot represent their distinct numeric IDs.
        ITEM_IDS.merge(item, id, (first, second) -> -1);
        ITEMS.put(id, item);
    }

    static void beginRegistration() {
        owner = Thread.currentThread();
        registering = true;
    }

    static void freezeRegistration() {
        registering = false;
        if (registrationFailure != null) throw new IllegalStateException("S4 capability registration failed", registrationFailure);
    }

    static void clearRegistration() {
        owner = null;
        registering = false;
        registrationFailure = null;
        BLOCKS.clear();
        ITEMS.clear();
        ITEM_IDS.clear();
        PROVIDERS.clear();
    }

    public static <T, C> void register(BlockCapability<T, C> capability,
            IBlockCapabilityProvider<T, C> provider, Block[] blocks) {
        checkRegistration(capability);
        try {
            Objects.requireNonNull(provider);
            for (Block block : blocks) {
                if (BLOCKS.values().stream().noneMatch(registered -> registered == block)) throw fail("S4 provider block is absent from the S2 registry");
                PROVIDERS.computeIfAbsent(block, ignored -> new ArrayList<>()).add(provider);
            }
        } catch (RuntimeException error) {
            registrationFailure = error;
            throw error;
        }
    }

    public static boolean isRegistered(BlockCapability<?, ?> capability, Block block) {
        checkRegistration(capability);
        return PROVIDERS.containsKey(block);
    }

    private static void checkRegistration(BlockCapability<?, ?> capability) {
        if (Thread.currentThread() != owner || !registering || registrationFailure != null) {
            throw fail("S4 capability registration is closed");
        }
        if (capability != Capabilities.ItemHandler.BLOCK) throw fail("Unsupported S4 block capability");
    }

    static void requireOutsideQuery() {
        if (ACTIVE.get() != null) throw fail("Legacy native world access is unavailable during S4 capability queries");
    }

    private static IllegalStateException fail(String message) {
        var snapshot = ACTIVE.get();
        if (snapshot != null) snapshot.failed = true;
        return new IllegalStateException(message);
    }

    @SuppressWarnings("unchecked")
    public static <T, C> T lookup(BlockCapability<T, C> capability, Level level, BlockPos pos,
            BlockState state, BlockEntity blockEntity, C context) {
        var snapshot = ACTIVE.get();
        if (snapshot == null || snapshot.level != level) throw fail("S4 capability lookup requires an active level snapshot");
        snapshot.check();
        if (capability != Capabilities.ItemHandler.BLOCK) throw fail("Unsupported S4 block capability");
        if (!snapshot.pos.equals(pos)) return null;
        if (state != null && !snapshot.state.equals(state) || blockEntity != null && blockEntity != snapshot) {
            throw fail("S4 capability lookup has inconsistent snapshot input");
        }
        var providers = PROVIDERS.get(BLOCKS.get(snapshot.state.stateId()));
        if (providers == null) return null;
        if (snapshot.depth == 4) throw fail("S4 capability dispatch depth exceeds 4");
        snapshot.depth++;
        try {
            for (var provider : providers) {
                T result = ((IBlockCapabilityProvider<T, C>) provider)
                    .getCapability(level, pos, snapshot.state, snapshot, context);
                if (result != null) return result;
            }
            return null;
        } catch (RuntimeException | Error error) {
            snapshot.failed = true;
            throw error;
        } finally {
            snapshot.depth--;
        }
    }

    private static ItemStack stack(int id, int count) {
        if (count < 0 || count > 99 || (id == 0) != (count == 0)) throw fail("Invalid S4 item stack");
        if (id == 0) return ItemStack.EMPTY;
        var item = ITEMS.get(id);
        if (item == null && id > 0 && id < FIRST_MOD_ITEM) {
            // Keep the identity of opaque vanilla IDs within the bounded 1.21.1 item table.
            item = new Item(new Item.Properties());
            bindItem(id, item);
        }
        if (item == null || ITEM_IDS.get(item) != id) throw fail("Unknown or ambiguous S4 item ID");
        return new ItemStack(item, count);
    }

    private static int id(ItemStack stack) {
        if (stack.isEmpty()) return 0;
        var id = ITEM_IDS.get(stack.getItem());
        if (id == null || id < 0) throw fail("Unregistered or ambiguous S4 item");
        return id;
    }

    private static final class Snapshot extends BlockEntity implements Container {
        final Level level;
        final BlockPos pos;
        final BlockState state;
        final int limit;
        final ItemStack[] slots;
        boolean writable;
        boolean failed;
        int depth;

        Snapshot(int dim, BlockPos pos, BlockState state, int limit, ItemStack[] slots) {
            this.level = new Level(dim);
            this.pos = pos;
            this.state = state;
            this.limit = limit;
            this.slots = slots;
            level.snapshotBlock(pos, state);
        }

        void check() {
            if (ACTIVE.get() != this || failed) throw fail("S4 container snapshot expired");
        }

        void checkSlot(int slot) {
            check();
            if (slot < 0 || slot >= slots.length) throw fail("Invalid S4 snapshot slot");
        }

        public int getContainerSize() { check(); return slots.length; }
        public int getMaxStackSize() { check(); return limit; }
        public ItemStack getItem(int slot) { checkSlot(slot); return slots[slot].copy(); }
        public boolean canPlaceItem(int slot, ItemStack stack) { checkSlot(slot); return true; }

        public void setItem(int slot, ItemStack stack) {
            checkSlot(slot);
            if (!writable || stack.getCount() > limit) throw fail("Invalid S4 snapshot mutation");
            id(stack);
            slots[slot] = stack.copy();
        }
    }

    private static int[] query(int[] frame) {
        if (Thread.currentThread() != owner || registering || ACTIVE.get() != null) {
            throw fail("S4 capability query requires the frozen JVM owner");
        }
        // dim, xyz, state, side (-1 for unsided), limit, slot count, operation count.
        if (frame.length < 9) throw fail("Truncated S4 capability frame");
        int dim = frame[0], side = frame[5], limit = frame[6], count = frame[7], operations = frame[8];
        if (dim < 0 || dim > 2 || side < -1 || side >= DIRECTIONS.length || limit < 1 || limit > 99
                || count < 0 || count > MAX_SLOTS || operations < 0 || operations > MAX_OPERATIONS
                || frame.length != 9 + 2 * count + 4 * operations) {
            throw fail("Invalid S4 capability frame");
        }
        var slots = new ItemStack[count];
        for (int slot = 0; slot < count; slot++) {
            int offset = 9 + 2 * slot;
            if (frame[offset + 1] > limit) throw fail("S4 stack exceeds snapshot slot limit");
            slots[slot] = stack(frame[offset], frame[offset + 1]);
        }
        var snapshot = new Snapshot(dim, new BlockPos(frame[1], frame[2], frame[3]), new BlockState(frame[4]), limit, slots);
        ACTIVE.set(snapshot);
        try {
            var handler = snapshot.level.getCapability(Capabilities.ItemHandler.BLOCK, snapshot.pos, snapshot.state,
                snapshot, side == -1 ? null : DIRECTIONS[side]);
            snapshot.check();
            if (handler == null) return new int[0];
            int handlerSlots = handler.getSlots();
            if (handlerSlots < 0 || handlerSlots > MAX_SLOTS) throw fail("Invalid S4 handler slot count");
            var result = new int[1 + 3 * operations + 2 * count];
            result[0] = handlerSlots;
            for (int op = 0; op < operations; op++) {
                int offset = 9 + 2 * count + 4 * op;
                int tag = frame[offset], slot = frame[offset + 1], item = frame[offset + 2], amount = frame[offset + 3];
                if (slot < 0 || slot >= handlerSlots) throw fail("Invalid S4 handler slot");
                int slotLimit = handler.getSlotLimit(slot);
                if (slotLimit < 0 || slotLimit > 99) throw fail("Invalid S4 handler slot limit");
                snapshot.writable = tag == 1 || tag == 3;
                ItemStack value;
                try {
                    value = switch (tag) {
                        case 0 -> handler.getStackInSlot(slot);
                        case 1, 2 -> handler.insertItem(slot, stack(item, amount), tag == 2);
                        case 3, 4 -> {
                            if (amount < 0 || item != 0) throw fail("Invalid S4 extraction");
                            yield handler.extractItem(slot, amount, tag == 4);
                        }
                        default -> throw fail("Unknown S4 capability operation");
                    };
                } finally {
                    snapshot.writable = false;
                }
                result[1 + 3 * op] = slotLimit;
                result[2 + 3 * op] = id(value);
                result[3 + 3 * op] = value.getCount();
            }
            for (int slot = 0; slot < count; slot++) {
                result[1 + 3 * operations + 2 * slot] = id(slots[slot]);
                result[2 + 3 * operations + 2 * slot] = slots[slot].getCount();
            }
            snapshot.check();
            return result;
        } finally {
            ACTIVE.remove();
        }
    }
}
