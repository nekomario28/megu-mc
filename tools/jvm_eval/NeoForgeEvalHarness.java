package net.mcspine.jvm_eval;

import net.mcspine.neoforge.Bootstrap;
import net.neoforged.bus.api.EventPriority;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
import net.minecraft.core.BlockPos;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;

/** Isolated timing probe for Bootstrap.initialize and one real EventBus dispatch. */
public final class NeoForgeEvalHarness {
    private NeoForgeEvalHarness() { }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("usage: NeoForgeEvalHarness <empty-mod-directory>");
        var event = new FarmlandTrampleEvent(new Level(), new BlockPos(8, 70, 8), new BlockState(),
            7.0F, new Entity());
        NeoForge.EVENT_BUS.addListener(EventPriority.NORMAL, false, FarmlandTrampleEvent.class, ignored -> { });
        long start = System.nanoTime();
        Bootstrap.initialize(args[0]);
        long bootstrapNanos = System.nanoTime() - start;

        for (int i = 0; i < 20_000; i++) NeoForge.EVENT_BUS.post(event);
        long[] samples = new long[50_000];
        for (int i = 0; i < samples.length; i++) {
            start = System.nanoTime();
            NeoForge.EVENT_BUS.post(event);
            samples[i] = System.nanoTime() - start;
        }
        java.util.Arrays.sort(samples);
        System.out.printf("bootstrap_ns=%d event_ns_p50=%d event_ns_p95=%d event_ns_p99=%d samples=%d%n",
            bootstrapNanos, percentile(samples, 50), percentile(samples, 95), percentile(samples, 99), samples.length);
        Bootstrap.close();
    }

    private static long percentile(long[] sorted, int percentile) {
        return sorted[(sorted.length - 1) * percentile / 100];
    }
}
