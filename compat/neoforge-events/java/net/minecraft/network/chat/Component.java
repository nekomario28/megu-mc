package net.minecraft.network.chat;

import java.util.Objects;

public record Component(String text) {
    public Component { Objects.requireNonNull(text); }
    public static Component literal(String text) { return new Component(text); }
    public String getString() { return text; }
}
