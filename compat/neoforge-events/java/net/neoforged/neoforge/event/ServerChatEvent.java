package net.neoforged.neoforge.event;

import java.util.Objects;
import net.minecraft.network.chat.Component;
import net.minecraft.server.level.ServerPlayer;
import net.neoforged.bus.api.Event;
import net.neoforged.bus.api.ICancellableEvent;

public class ServerChatEvent extends Event implements ICancellableEvent {
    private final ServerPlayer player;
    private final String username;
    private final String rawText;
    private Component message;

    public ServerChatEvent(ServerPlayer player, String rawText, Component message) {
        this.player = Objects.requireNonNull(player);
        this.username = player.getName().getString();
        this.rawText = Objects.requireNonNull(rawText);
        this.message = Objects.requireNonNull(message);
    }

    public ServerPlayer getPlayer() { return player; }
    public String getUsername() { return username; }
    public String getRawText() { return rawText; }
    public Component getMessage() { return message; }
    public void setMessage(Component message) { this.message = Objects.requireNonNull(message); }
}
