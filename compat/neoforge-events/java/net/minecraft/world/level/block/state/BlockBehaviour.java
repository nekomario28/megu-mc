package net.minecraft.world.level.block.state;

import java.util.Objects;

/** S2 models a single state per custom block. */
public abstract class BlockBehaviour {
    protected BlockBehaviour(Properties properties) { Objects.requireNonNull(properties); }

    public static class Properties {
        private Properties() { }
        public static Properties of() { return new Properties(); }
    }
}
