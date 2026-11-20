package net.neoforged.neoforge.event.level;

import net.minecraft.core.BlockPos;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.entity.player.Player;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.LevelAccessor;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.bus.api.Event;
import net.neoforged.bus.api.ICancellableEvent;
import net.neoforged.neoforge.common.util.BlockSnapshot;

/** Bounded S1 block events, preserving the original FarmlandTrampleEvent binary API. */
public abstract class BlockEvent extends Event {
    private final LevelAccessor level;
    private final BlockPos pos;
    private final BlockState state;

    public BlockEvent(LevelAccessor level, BlockPos pos, BlockState state) {
        this.level = level;
        this.pos = pos;
        this.state = state;
    }

    public LevelAccessor getLevel() { return level; }
    public BlockPos getPos() { return pos; }
    public BlockState getState() { return state; }

    public static class FarmlandTrampleEvent extends BlockEvent implements ICancellableEvent {
        private final Entity entity;
        private final float fallDistance;

        public FarmlandTrampleEvent(Level level, BlockPos pos, BlockState state, float fallDistance, Entity entity) {
            super(level, pos, state);
            this.entity = entity;
            this.fallDistance = fallDistance;
        }

        public Entity getEntity() { return entity; }
        public float getFallDistance() { return fallDistance; }
    }

    public static class BreakEvent extends BlockEvent implements ICancellableEvent {
        private final Player player;
        private int expToDrop;

        public BreakEvent(Level level, BlockPos pos, BlockState state, Player player) {
            super(level, pos, state);
            this.player = player;
        }

        public Player getPlayer() { return player; }
        public int getExpToDrop() { return expToDrop; }
        public void setExpToDrop(int exp) {
            if (exp < 0) throw new IllegalArgumentException("Negative block break XP");
            expToDrop = exp;
        }
    }

    public static class EntityPlaceEvent extends BlockEvent implements ICancellableEvent {
        private final Entity entity;
        private final BlockSnapshot snapshot;
        private final BlockState placedBlock;
        private final BlockState placedAgainst;

        public EntityPlaceEvent(BlockSnapshot snapshot, BlockState placedAgainst, Entity entity) {
            super(snapshot.getLevel(), snapshot.getPos(), snapshot.getCurrentState());
            this.entity = entity;
            this.snapshot = snapshot;
            this.placedBlock = getState();
            this.placedAgainst = placedAgainst;
        }

        public Entity getEntity() { return entity; }
        public BlockSnapshot getBlockSnapshot() { return snapshot; }
        public BlockState getPlacedBlock() { return placedBlock; }
        public BlockState getPlacedAgainst() { return placedAgainst; }
    }
}
