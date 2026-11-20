package com.mojang.brigadier.context;

import java.util.Objects;

public final class CommandContext<S> {
    private final S source;

    public CommandContext(S source) { this.source = Objects.requireNonNull(source); }

    public S getSource() { return source; }
}
