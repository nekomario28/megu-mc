package net.minecraft.world.level.block.state;

/** Protocol 767 block state identifier; a lightweight immutable facade. */
public record BlockState(int stateId) {
    public BlockState() { this(0); }
}
