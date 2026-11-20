package net.neoforged.neoforge.network.handling;

import java.util.concurrent.CompletableFuture;
import java.util.function.Supplier;
import net.minecraft.network.protocol.PacketFlow;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.world.entity.player.Player;

public interface IPayloadContext {
    Player player();
    void reply(CustomPacketPayload payload);
    void handle(CustomPacketPayload payload);
    PacketFlow flow();
    CompletableFuture<Void> enqueueWork(Runnable task);
    <T> CompletableFuture<T> enqueueWork(Supplier<T> task);
}
