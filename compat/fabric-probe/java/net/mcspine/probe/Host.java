package net.mcspine.probe;

/** Scoped world ABI. Rust owns storage; normal callbacks may commit only their active landing. */
public final class Host {
    private Host() { }
    public static native boolean setBlock(int x, int y, int z, int state);
    public static native int getBlock(int x, int y, int z);
}
