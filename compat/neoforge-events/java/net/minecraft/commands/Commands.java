package net.minecraft.commands;

import com.mojang.brigadier.builder.LiteralArgumentBuilder;

public final class Commands {
    private Commands() { }
    public enum CommandSelection { ALL, DEDICATED, INTEGRATED }
    public static LiteralArgumentBuilder<CommandSourceStack> literal(String literal) {
        return LiteralArgumentBuilder.literal(literal);
    }
}
