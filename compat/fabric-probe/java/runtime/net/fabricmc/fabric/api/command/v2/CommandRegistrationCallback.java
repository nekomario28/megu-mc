package net.fabricmc.fabric.api.command.v2;

import com.mojang.brigadier.CommandDispatcher;
import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.command.CommandRegistryAccess;
import net.minecraft.server.command.CommandManager;
import net.minecraft.server.command.ServerCommandSource;

@FunctionalInterface
public interface CommandRegistrationCallback {
    Event<CommandRegistrationCallback> EVENT = EventFactory.createArrayBacked(
        CommandRegistrationCallback.class, listeners -> (dispatcher, access, environment) -> {
            for (CommandRegistrationCallback listener : listeners) {
                listener.register(dispatcher, access, environment);
            }
        });

    void register(CommandDispatcher<ServerCommandSource> dispatcher, CommandRegistryAccess access,
                  CommandManager.RegistrationEnvironment environment);
}
