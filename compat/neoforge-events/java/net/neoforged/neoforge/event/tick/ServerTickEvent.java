package net.neoforged.neoforge.event.tick;

import net.neoforged.bus.api.Event;

/** A tick phase and time budget explicitly supplied by the server owner. */
public abstract class ServerTickEvent extends Event {
    private final boolean hasTime;

    private ServerTickEvent(boolean hasTime) { this.hasTime = hasTime; }

    public boolean hasTime() { return hasTime; }

    public static class Pre extends ServerTickEvent {
        public Pre(boolean hasTime) { super(hasTime); }
    }

    public static class Post extends ServerTickEvent {
        public Post(boolean hasTime) { super(hasTime); }
    }
}
