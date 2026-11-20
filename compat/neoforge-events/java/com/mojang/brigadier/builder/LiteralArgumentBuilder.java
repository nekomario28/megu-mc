package com.mojang.brigadier.builder;

import java.util.Objects;
import com.mojang.brigadier.Command;

public final class LiteralArgumentBuilder<S> {
    private final String literal;
    private Command<S> command;
    private LiteralArgumentBuilder(String literal) { this.literal = Objects.requireNonNull(literal); }
    public static <S> LiteralArgumentBuilder<S> literal(String literal) {
        return new LiteralArgumentBuilder<>(literal);
    }
    public String getLiteral() { return literal; }
    public LiteralArgumentBuilder<S> executes(Command<S> command) {
        this.command = Objects.requireNonNull(command);
        return this;
    }
    public Command<S> getCommand() { return command; }
}
