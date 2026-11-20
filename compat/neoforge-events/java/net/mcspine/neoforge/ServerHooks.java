package net.mcspine.neoforge;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.entity.Entity;
import net.minecraft.core.BlockPos;
import net.mcspine.probe.Host;
import java.util.List;
import java.util.function.Consumer;
import com.mojang.brigadier.CommandDispatcher;
import net.minecraft.commands.CommandBuildContext;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;
import net.minecraft.network.chat.Component;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.entity.player.Player;
import net.neoforged.bus.api.Event;
import net.neoforged.bus.api.ICancellableEvent;
import net.neoforged.neoforge.common.util.BlockSnapshot;
import net.neoforged.neoforge.event.RegisterCommandsEvent;
import net.neoforged.neoforge.event.ServerChatEvent;
import net.neoforged.neoforge.event.entity.player.PlayerEvent;
import net.neoforged.neoforge.event.level.BlockEvent;
/** Only this original mod's cancellation contract; arguments are opaque API shapes. */
public final class ServerHooks {
    public static void land(int x, int y, int z, float distance) {
        var event = new FarmlandTrampleEvent(new Level(), new BlockPos(x,y,z),
            new BlockState(), distance, new Entity());
        NeoForge.EVENT_BUS.post(event);
        if (!event.isCanceled()) Host.setBlock(x,y,z,10);
    }

    /** Mutations must be applied atomically by the host, honoring each SetBlockIf CAS guard. */
    public record DispatchResult(boolean canceled, List<MutationBuffer.Mutation> mutations) { }

    private static <E extends Event> DispatchResult dispatch(E event, Consumer<E> afterPost) {
        MutationBuffer.beginEvent();
        try {
            NeoForge.EVENT_BUS.post(event);
            boolean canceled = event instanceof ICancellableEvent cancellable && cancellable.isCanceled();
            if (canceled) MutationBuffer.cancelCurrent();
            else afterPost.accept(event);
            MutationBuffer.endEvent();
            // Nested dispatches accumulate into their enclosing scope.
            return new DispatchResult(canceled,
                MutationBuffer.depth() == 0 ? MutationBuffer.commitBatch() : List.of());
        } catch (RuntimeException | Error error) {
            MutationBuffer.discardAll();
            throw error;
        }
    }

    public static DispatchResult onBlockBreak(Level level, BlockPos pos, BlockState state,
                                               Player player, int expToDrop) {
        level.snapshotBlock(pos, state);
        BlockEvent.BreakEvent event = new BlockEvent.BreakEvent(level, pos, state, player);
        event.setExpToDrop(expToDrop);
        return dispatch(event, posted -> {
            if (posted.getExpToDrop() != expToDrop) {
                MutationBuffer.append(new MutationBuffer.BreakExp(posted.getExpToDrop()));
            }
        });
    }

    public static DispatchResult onBlockPlace(Level level, BlockPos pos, BlockState replaced,
                                               BlockState placed, BlockState against, Entity entity) {
        level.snapshotBlock(pos, replaced);
        BlockSnapshot snapshot = new BlockSnapshot(level, pos, replaced, placed);
        return dispatch(new BlockEvent.EntityPlaceEvent(snapshot, against, entity), event -> { });
    }

    public static DispatchResult onPlayerLoggedIn(Player player) {
        return dispatch(new PlayerEvent.PlayerLoggedInEvent(player), event -> { });
    }

    public static DispatchResult onPlayerLoggedOut(Player player) {
        return dispatch(new PlayerEvent.PlayerLoggedOutEvent(player), event -> { });
    }

    public static DispatchResult onServerChat(ServerPlayer player, String rawText) {
        ServerChatEvent event = new ServerChatEvent(player, rawText, Component.literal(rawText));
        return dispatch(event, posted -> {
            if (!rawText.equals(posted.getMessage().getString())) {
                MutationBuffer.append(new MutationBuffer.ChatMessage(posted.getMessage().getString()));
            }
        });
    }

    public static DispatchResult onRegisterCommands() {
        return dispatch(new RegisterCommandsEvent(new CommandDispatcher<CommandSourceStack>(),
            Commands.CommandSelection.DEDICATED, new CommandBuildContext()), event -> { });
    }
}
