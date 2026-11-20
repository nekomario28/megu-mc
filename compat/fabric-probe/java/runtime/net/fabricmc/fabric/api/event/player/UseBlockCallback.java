package net.fabricmc.fabric.api.event.player;

import java.util.Objects;
import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.entity.player.PlayerEntity;
import net.minecraft.util.ActionResult;
import net.minecraft.util.Hand;
import net.minecraft.util.hit.BlockHitResult;
import net.minecraft.world.World;

@FunctionalInterface
public interface UseBlockCallback {
    Event<UseBlockCallback> EVENT = EventFactory.createArrayBacked(
        UseBlockCallback.class, listeners -> (player, world, hand, hit) -> {
            for (UseBlockCallback listener : listeners) {
                ActionResult result = Objects.requireNonNull(listener.interact(player, world, hand, hit));
                if (result != ActionResult.PASS) return result;
            }
            return ActionResult.PASS;
        });

    ActionResult interact(PlayerEntity player, World world, Hand hand, BlockHitResult hit);
}
