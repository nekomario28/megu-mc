package net.mcspine.probe;

public final class ProbeGame {
    public boolean replaceBlock(int state) {
        return Host.setBlock(8, 70, 8, state);
    }
}
