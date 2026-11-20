package net.fabricmc.fabric.api.event;

import java.lang.reflect.Array;
import java.util.Arrays;
import java.util.Objects;
import java.util.function.Function;

public final class EventFactory {
    private EventFactory() {}

    public static <T> Event<T> createArrayBacked(Class<? super T> type, Function<T[], T> factory) {
        return new ArrayEvent<>(type, factory);
    }

    public static <T> Event<T> createArrayBacked(Class<T> type, T emptyInvoker,
                                                  Function<T[], T> factory) {
        Objects.requireNonNull(emptyInvoker);
        return createArrayBacked(type, listeners -> switch (listeners.length) {
            case 0 -> emptyInvoker;
            case 1 -> listeners[0];
            default -> factory.apply(listeners);
        });
    }

    private static final class ArrayEvent<T> extends Event<T> {
        private final Function<T[], T> factory;
        private T[] listeners;

        @SuppressWarnings("unchecked")
        ArrayEvent(Class<? super T> type, Function<T[], T> factory) {
            this.factory = Objects.requireNonNull(factory);
            listeners = (T[]) Array.newInstance(type, 0);
            invoker = Objects.requireNonNull(factory.apply(listeners));
        }

        @Override public synchronized void register(T listener) {
            T[] next = Arrays.copyOf(listeners, listeners.length + 1);
            next[listeners.length] = Objects.requireNonNull(listener);
            T nextInvoker = Objects.requireNonNull(factory.apply(next));
            listeners = next;
            invoker = nextInvoker;
        }
    }
}
