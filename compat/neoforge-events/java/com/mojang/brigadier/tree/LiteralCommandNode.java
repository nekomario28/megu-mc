package com.mojang.brigadier.tree;

public record LiteralCommandNode<S>(String literal) {
    public String getLiteral() { return literal; }
}
