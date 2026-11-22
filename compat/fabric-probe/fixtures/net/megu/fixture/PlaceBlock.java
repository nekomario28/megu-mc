package net.megu.fixture;

import net.fabricmc.api.ModInitializer;
import net.fabricmc.loader.api.FabricLoader;
import net.megu.probe.Host;

/** Self-authored diagnostic mod, compiled once and loaded without runtime JAR edits. */
public final class PlaceBlock implements ModInitializer {
    public void onInitialize() {
        if (!FabricLoader.getInstance().isModLoaded("place_block")) {
            throw new IllegalStateException("Loader metadata was bypassed");
        }
        int original = Host.getBlock(8, 70, 8);
        // A rejected JNI input must neither mutate the world nor escape as a Rust unwind.
        try {
            Host.setBlock(8, 70, 8, -1);
            throw new IllegalStateException("Invalid state was accepted");
        } catch (IllegalArgumentException expected) { }
        if (Host.getBlock(8, 70, 8) != original) { throw new AssertionError("Invalid state changed the world"); }
        var workerFailure = new java.util.concurrent.atomic.AtomicReference<Throwable>();
        var worker = new Thread(() -> {
            try {
                try {
                    Host.setBlock(8, 70, 8, 2);
                    throw new AssertionError("Worker mutation was accepted");
                } catch (IllegalStateException expected) { }
                try {
                    Host.getBlock(8, 70, 8);
                    throw new AssertionError("Worker read was accepted");
                } catch (IllegalStateException expected) { }
            } catch (Throwable failure) { workerFailure.set(failure); }
        });
        worker.start();
        try { worker.join(); }
        catch (InterruptedException interrupted) { Thread.currentThread().interrupt(); throw new IllegalStateException(interrupted); }
        if (workerFailure.get() != null) { throw new IllegalStateException(workerFailure.get()); }
        if (Host.getBlock(8, 70, 8) != original) { throw new AssertionError("Rejected worker changed the world"); }
        if (!Host.setBlock(8, 70, 8, 1) || Host.getBlock(8, 70, 8) != 1) {
            throw new IllegalStateException("JNI write/read did not reach Rust");
        }
    }
}
