package com.mojang.brigadier;

import com.mojang.brigadier.builder.LiteralArgumentBuilder;
import com.mojang.brigadier.tree.LiteralCommandNode;
import net.mcspine.neoforge.MutationBuffer;
import java.util.LinkedHashMap;
import java.util.Map;

public class CommandDispatcher<S> {
    private static final Map<String, Command<?>> COMMANDS = new LinkedHashMap<>();

    public static synchronized void clearRegisteredCommands() { COMMANDS.clear(); }

    public static synchronized Command<?> getRegisteredCommand(String literal) {
        return COMMANDS.get(literal);
    }

    public LiteralCommandNode<S> register(LiteralArgumentBuilder<S> command) {
        String literal = command.getLiteral();
        MutationBuffer.append(new MutationBuffer.RegisterCommand(literal));
        if (command.getCommand() != null) {
            synchronized (CommandDispatcher.class) { COMMANDS.put(literal, command.getCommand()); }
        }
        return new LiteralCommandNode<>(literal);
    }
}
