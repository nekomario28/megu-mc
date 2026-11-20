package com.mojang.brigadier.builder;

import java.util.Objects;

public final class LiteralArgumentBuilder<S> {
    private final String literal;
    private LiteralArgumentBuilder(String literal) { this.literal = Objects.requireNonNull(literal); }
    public static <S> LiteralArgumentBuilder<S> literal(String literal) {
        return new LiteralArgumentBuilder<>(literal);
    }
    public String getLiteral() { return literal; }
}
