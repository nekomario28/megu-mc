package net.minecraft.commands;

import java.util.Objects;
import net.minecraft.network.chat.Component;
import net.minecraft.core.BlockPos;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.level.Level;

public class CommandSourceStack {
    private final Level level;
    private final ServerPlayer player;
    private final BlockPos position;

    public CommandSourceStack(Level level, ServerPlayer player) {
        this(level, player, new BlockPos(0, 0, 0));
    }

    public CommandSourceStack(Level level, ServerPlayer player, BlockPos position) {
        this.level = Objects.requireNonNull(level);
        this.player = Objects.requireNonNull(player);
        this.position = Objects.requireNonNull(position);
    }

    public Level getLevel() { return level; }
    public ServerPlayer getPlayer() { return player; }
    public BlockPos getPosition() { return position; }
    public void sendSuccess(Component message, boolean broadcastToOperators) {
        player.sendSystemMessage(message);
    }
}
