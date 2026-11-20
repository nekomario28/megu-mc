package com.mojang.brigadier.tree;

public final class LiteralCommandNode<S> {
    private final String literal;

    public LiteralCommandNode(String literal) { this.literal = literal; }

    public String getLiteral() { return literal; }
    public String getName() { return literal; }
}
