package net.minecraft.server.command;

import com.mojang.brigadier.builder.LiteralArgumentBuilder;

public final class CommandManager {
    private CommandManager() {}

    public static LiteralArgumentBuilder<ServerCommandSource> literal(String name) {
        return LiteralArgumentBuilder.literal(name);
    }

    public enum RegistrationEnvironment {
        ALL, DEDICATED, INTEGRATED
    }
}
