package net.mcspine.probe;

/** Probe ABI only. Rust owns the world; Java has no duplicate world model. */
public final class Host {
    private Host() { }
    public static native boolean setBlock(int x, int y, int z, int state);
    public static native int getBlock(int x, int y, int z);
}
