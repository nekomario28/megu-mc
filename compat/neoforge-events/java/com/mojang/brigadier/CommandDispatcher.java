package com.mojang.brigadier;

import com.mojang.brigadier.builder.LiteralArgumentBuilder;
import com.mojang.brigadier.tree.LiteralCommandNode;
import net.mcspine.neoforge.MutationBuffer;

public class CommandDispatcher<S> {
    public LiteralCommandNode<S> register(LiteralArgumentBuilder<S> command) {
        String literal = command.getLiteral();
        MutationBuffer.append(new MutationBuffer.RegisterCommand(literal));
        return new LiteralCommandNode<>(literal);
    }
}
