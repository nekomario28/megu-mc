package net.mcspine.neoforge;
import java.util.*;
import net.neoforged.bus.api.*;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.entity.Entity;
import net.minecraft.core.BlockPos;
/** Isolated bus contract checks. Normal server never calls this class. */
public final class EventChecks {
    static FarmlandTrampleEvent event() {
        return new FarmlandTrampleEvent(new Level(),new BlockPos(8,70,8),new BlockState(),7,new Entity());
    }
    public static void main(String[] args) throws Exception {
        Bootstrap.initialize(args[0]);
        if (Bootstrap.instance == null) throw new AssertionError("Missing original mod");
        if (!NeoForge.EVENT_BUS.post(event()).isCanceled()) throw new AssertionError("Mod did not cancel");
        NeoForge.EVENT_BUS.unregister(Bootstrap.instance);
        if (NeoForge.EVENT_BUS.post(event()).isCanceled()) throw new AssertionError("Unregister discriminator failed");
        var bus = BusBuilder.builder().startShutdown().build();
        var trace = new ArrayList<String>();
        bus.addListener(EventPriority.HIGH, false, FarmlandTrampleEvent.class, e -> { trace.add("high"); e.setCanceled(true); });
        bus.addListener(EventPriority.NORMAL, false, FarmlandTrampleEvent.class, e -> trace.add("normal"));
        bus.addListener(EventPriority.LOW, true, FarmlandTrampleEvent.class, e -> trace.add("low-canceled"));
        if (bus.post(event()).isCanceled() || !trace.isEmpty()) throw new AssertionError("Shutdown bus dispatched");
        bus.start();
        if (!bus.post(event()).isCanceled() || !trace.equals(List.of("high","low-canceled"))) throw new AssertionError("Priority/cancellation contract: " + trace);
        // Registration order must not matter: all five phases run HIGHEST to LOWEST.
        var ordered = BusBuilder.builder().startShutdown().build();
        var phases = new ArrayList<String>();
        ordered.addListener(EventPriority.LOW, FarmlandTrampleEvent.class, e -> phases.add("low"));
        ordered.addListener(EventPriority.LOWEST, FarmlandTrampleEvent.class, e -> phases.add("lowest"));
        ordered.addListener(EventPriority.HIGH, FarmlandTrampleEvent.class, e -> phases.add("high"));
        ordered.addListener(EventPriority.HIGHEST, FarmlandTrampleEvent.class, e -> phases.add("highest"));
        ordered.addListener(EventPriority.NORMAL, FarmlandTrampleEvent.class, e -> phases.add("normal"));
        ordered.start();
        ordered.post(event());
        if (!phases.equals(List.of("highest", "high", "normal", "low", "lowest")))
            throw new AssertionError("Five-phase listener ordering: " + phases);
        // A canceling HIGHEST listener short-circuits lower phases unless they opt in.
        var shortCircuit = BusBuilder.builder().startShutdown().build();
        var skipped = new ArrayList<String>();
        shortCircuit.addListener(EventPriority.HIGHEST, FarmlandTrampleEvent.class, e -> { skipped.add("highest"); e.setCanceled(true); });
        shortCircuit.addListener(EventPriority.NORMAL, FarmlandTrampleEvent.class, e -> skipped.add("normal"));
        shortCircuit.addListener(EventPriority.LOWEST, true, FarmlandTrampleEvent.class, e -> skipped.add("lowest-canceled"));
        shortCircuit.start();
        if (!shortCircuit.post(event()).isCanceled()
                || !skipped.equals(List.of("highest", "lowest-canceled")))
            throw new AssertionError("Cancellation short-circuit: " + skipped);
        try {
            NeoForge.EVENT_BUS.addListener(EventPriority.NORMAL, InvalidModBusEvent.class, event -> {});
            throw new AssertionError("Game bus accepted mod lifecycle event");
        } catch (IllegalArgumentException expected) { }
        System.out.println("PASS original mod registration, cancellation, unregister, priority, receiveCanceled, shutdown/start, mod-bus rejection");
    }
    public static class InvalidModBusEvent extends Event implements net.neoforged.fml.event.IModBusEvent { }
}
