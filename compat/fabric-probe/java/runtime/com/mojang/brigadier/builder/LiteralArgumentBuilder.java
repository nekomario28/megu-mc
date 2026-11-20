package com.mojang.brigadier.builder;

public final class LiteralArgumentBuilder<S> {
    private final String literal;

    private LiteralArgumentBuilder(String literal) {
        if (literal.isEmpty() || literal.chars().anyMatch(Character::isWhitespace)) {
            throw new IllegalArgumentException("Invalid command literal: " + literal);
        }
        this.literal = literal;
    }

    public static <S> LiteralArgumentBuilder<S> literal(String name) {
        return new LiteralArgumentBuilder<>(name);
    }

    public String getLiteral() { return literal; }
}
