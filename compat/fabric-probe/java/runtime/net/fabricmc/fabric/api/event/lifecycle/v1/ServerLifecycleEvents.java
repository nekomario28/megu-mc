package net.fabricmc.fabric.api.event.lifecycle.v1;

import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.server.MinecraftServer;

public final class ServerLifecycleEvents {
    private ServerLifecycleEvents() {}

    public static final Event<ServerStarting> SERVER_STARTING =
        EventFactory.createArrayBacked(ServerStarting.class, listeners -> server -> {
            for (ServerStarting listener : listeners) listener.onServerStarting(server);
        });
    public static final Event<ServerStarted> SERVER_STARTED =
        EventFactory.createArrayBacked(ServerStarted.class, listeners -> server -> {
            for (ServerStarted listener : listeners) listener.onServerStarted(server);
        });
    public static final Event<ServerStopping> SERVER_STOPPING =
        EventFactory.createArrayBacked(ServerStopping.class, listeners -> server -> {
            for (ServerStopping listener : listeners) listener.onServerStopping(server);
        });

    @FunctionalInterface
    public interface ServerStarting { void onServerStarting(MinecraftServer server); }
    @FunctionalInterface
    public interface ServerStarted { void onServerStarted(MinecraftServer server); }
    @FunctionalInterface
    public interface ServerStopping { void onServerStopping(MinecraftServer server); }
}
