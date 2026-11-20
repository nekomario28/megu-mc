package net.mcspine.neoforge;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.entity.Entity;
import net.minecraft.core.BlockPos;
import net.mcspine.probe.Host;
import java.util.List;
import java.util.function.Function;
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
/** Dispatches events against Java snapshots and returns mutations for the Rust host. */
public final class ServerHooks {
    public static void land(int x, int y, int z, float distance) {
        DirectBatchBridge.requireOutsideBatch();
        var event = new FarmlandTrampleEvent(new Level(), new BlockPos(x,y,z),
            new BlockState(), distance, new Entity());
        NeoForge.EVENT_BUS.post(event);
        if (!event.isCanceled()) Host.setBlock(x,y,z,10);
    }

    /** The host applies each eligible block write with its own CAS guard. */
    public record DispatchResult(boolean canceled, List<MutationBuffer.Mutation> mutations) { }

    private record PostedEvent(DispatchResult result, MutationBuffer.Mutation response) {
        String encode() {
            return MutationBuffer.encode(result.canceled(), result.mutations(), response);
        }
    }

    private static PostedEvent dispatch(Event event) {
        return dispatch(event, posted -> null);
    }

    private static <E extends Event> PostedEvent dispatch(E event, Function<E, MutationBuffer.Mutation> afterPost) {
        MutationBuffer.beginEvent();
        try {
            NeoForge.EVENT_BUS.post(event);
            boolean canceled = event instanceof ICancellableEvent cancellable && cancellable.isCanceled();
            var response = canceled ? null : afterPost.apply(event);
            if (canceled) MutationBuffer.cancelCurrent();
            else if (response != null) MutationBuffer.append(response);
            MutationBuffer.endEvent();
            // Nested dispatches accumulate into their enclosing scope.
            if (MutationBuffer.depth() != 0) {
                return new PostedEvent(new DispatchResult(canceled, List.of()), null);
            }
            return new PostedEvent(new DispatchResult(canceled, MutationBuffer.commitBatch()), response);
        } catch (RuntimeException | Error error) {
            MutationBuffer.discardAll();
            throw error;
        }
    }

    public static DispatchResult onBlockBreak(Level level, BlockPos pos, BlockState state,
                                               Player player, int expToDrop) {
        return blockBreak(level, pos, state, player, expToDrop).result();
    }

    private static PostedEvent blockBreak(Level level, BlockPos pos, BlockState state,
                                          Player player, int expToDrop) {
        level.snapshotBlock(pos, state);
        BlockEvent.BreakEvent event = new BlockEvent.BreakEvent(level, pos, state, player);
        event.setExpToDrop(expToDrop);
        return dispatch(event, posted -> posted.getExpToDrop() == expToDrop
            ? null : new MutationBuffer.BreakExp(posted.getExpToDrop()));
    }

    public static DispatchResult onBlockPlace(Level level, BlockPos pos, BlockState replaced,
                                               BlockState placed, BlockState against, Entity entity) {
        return blockPlace(level, pos, replaced, placed, against, entity).result();
    }

    private static PostedEvent blockPlace(Level level, BlockPos pos, BlockState replaced,
                                          BlockState placed, BlockState against, Entity entity) {
        level.snapshotBlock(pos, replaced);
        BlockSnapshot snapshot = new BlockSnapshot(level, pos, replaced, placed);
        return dispatch(new BlockEvent.EntityPlaceEvent(snapshot, against, entity));
    }

    public static DispatchResult onPlayerLoggedIn(Player player) {
        return dispatch(new PlayerEvent.PlayerLoggedInEvent(player)).result();
    }

    public static DispatchResult onPlayerLoggedOut(Player player) {
        return dispatch(new PlayerEvent.PlayerLoggedOutEvent(player)).result();
    }

    public static DispatchResult onServerChat(ServerPlayer player, String rawText) {
        return serverChat(player, rawText).result();
    }

    private static PostedEvent serverChat(ServerPlayer player, String rawText) {
        ServerChatEvent event = new ServerChatEvent(player, rawText, Component.literal(rawText));
        return dispatch(event, posted -> {
            String message = posted.getMessage().getString();
            return rawText.equals(message) ? null : new MutationBuffer.ChatMessage(message);
        });
    }

    public static DispatchResult onRegisterCommands() {
        return registerCommands().result();
    }

    private static PostedEvent registerCommands() {
        return dispatch(new RegisterCommandsEvent(new CommandDispatcher<CommandSourceStack>(),
            Commands.CommandSelection.DEDICATED, new CommandBuildContext()));
    }

    public static String dispatchBlockBreakEncoded(int dim, int x, int y, int z, int state,
                                                   int playerEid, String playerName, int expToDrop) {
        return blockBreak(new Level(MutationBuffer.requireDimension(dim)), new BlockPos(x, y, z),
            new BlockState(state), new ServerPlayer(playerEid, playerName), expToDrop).encode();
    }

    public static String dispatchBlockPlaceEncoded(int dim, int x, int y, int z, int replaced,
                                                   int placed, int against, int playerEid, String playerName) {
        return blockPlace(new Level(MutationBuffer.requireDimension(dim)), new BlockPos(x, y, z),
            new BlockState(replaced), new BlockState(placed), new BlockState(against),
            new ServerPlayer(playerEid, playerName)).encode();
    }

    public static String dispatchPlayerLoginEncoded(int playerEid, String playerName) {
        return dispatch(new PlayerEvent.PlayerLoggedInEvent(new ServerPlayer(playerEid, playerName))).encode();
    }

    public static String dispatchPlayerLogoutEncoded(int playerEid, String playerName) {
        return dispatch(new PlayerEvent.PlayerLoggedOutEvent(new ServerPlayer(playerEid, playerName))).encode();
    }

    public static String dispatchServerChatEncoded(int playerEid, String playerName, String rawText) {
        return serverChat(new ServerPlayer(playerEid, playerName), rawText).encode();
    }

    public static String dispatchRegisterCommandsEncoded() {
        return registerCommands().encode();
    }
}
