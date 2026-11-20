package net.fabricmc.fabric.api.event.player;

import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.block.BlockState;
import net.minecraft.block.entity.BlockEntity;
import net.minecraft.entity.player.PlayerEntity;
import net.minecraft.util.math.BlockPos;
import net.minecraft.world.World;

public final class PlayerBlockBreakEvents {
    private PlayerBlockBreakEvents() {}

    public static final Event<Before> BEFORE = EventFactory.createArrayBacked(
        Before.class, listeners -> (world, player, pos, state, entity) -> {
            for (Before listener : listeners) {
                if (!listener.beforeBlockBreak(world, player, pos, state, entity)) return false;
            }
            return true;
        });
    public static final Event<After> AFTER = EventFactory.createArrayBacked(
        After.class, listeners -> (world, player, pos, state, entity) -> {
            for (After listener : listeners) listener.afterBlockBreak(world, player, pos, state, entity);
        });

    @FunctionalInterface
    public interface Before {
        boolean beforeBlockBreak(World world, PlayerEntity player, BlockPos pos, BlockState state,
                                 BlockEntity blockEntity);
    }

    @FunctionalInterface
    public interface After {
        void afterBlockBreak(World world, PlayerEntity player, BlockPos pos, BlockState state,
                             BlockEntity blockEntity);
    }
}
