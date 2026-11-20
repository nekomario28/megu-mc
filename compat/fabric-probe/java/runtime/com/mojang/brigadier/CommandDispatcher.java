package com.mojang.brigadier;

import com.mojang.brigadier.builder.LiteralArgumentBuilder;
import com.mojang.brigadier.tree.LiteralCommandNode;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public final class CommandDispatcher<S> {
    private final Map<String, LiteralCommandNode<S>> commands = new LinkedHashMap<>();
    private boolean frozen;

    public synchronized LiteralCommandNode<S> register(LiteralArgumentBuilder<S> command) {
        if (frozen) throw new IllegalStateException("Command registration is closed");
        return commands.computeIfAbsent(command.getLiteral(), LiteralCommandNode::new);
    }

    public synchronized List<String> freeze() {
        frozen = true;
        return List.copyOf(commands.keySet());
    }
}
