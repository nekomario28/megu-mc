package net.neoforged.neoforge.network;

import net.megu.neoforge.S4NetworkBridge;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.server.level.ServerPlayer;

public final class PacketDistributor {
    private PacketDistributor() { }

    public static void sendToPlayer(ServerPlayer player, CustomPacketPayload payload, CustomPacketPayload... payloads) {
        S4NetworkBridge.send(player.getEid(), payload, payloads);
    }

    public static void sendToAllPlayers(CustomPacketPayload payload, CustomPacketPayload... payloads) {
        S4NetworkBridge.send(null, payload, payloads);
    }
}
