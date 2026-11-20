package net.mcspine.neoforge;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/** Thread-confined event mutation batch; Rust owns CAS validation and application. */
public final class MutationBuffer {
    public static final int MAX_REENTRANCY_DEPTH = 4;

    public sealed interface Mutation permits SetBlockIf, SendSystemMessage, RegisterCommand, BreakExp, ChatMessage { }
    public record SetBlockIf(int dim, int x, int y, int z, int newState, int expectedState) implements Mutation { }
    public record SendSystemMessage(int playerEid, String text) implements Mutation {
        public SendSystemMessage { Objects.requireNonNull(text); }
    }
    public record RegisterCommand(String literal) implements Mutation {
        public RegisterCommand { Objects.requireNonNull(literal); }
    }
    public record BreakExp(int exp) implements Mutation { }
    public record ChatMessage(String text) implements Mutation {
        public ChatMessage { Objects.requireNonNull(text); }
    }

    private static final class State {
        final ArrayList<Mutation> pending = new ArrayList<>();
        final ArrayDeque<Integer> scopeStarts = new ArrayDeque<>();
        boolean failed;
    }

    private static final ThreadLocal<State> STATE = ThreadLocal.withInitial(State::new);

    private MutationBuffer() { }

    public static void beginEvent() {
        State state = STATE.get();
        if (state.failed) throw new IllegalStateException("MutationBuffer batch already failed");
        if (state.scopeStarts.size() >= MAX_REENTRANCY_DEPTH) {
            state.pending.clear();
            state.failed = true;
            throw new IllegalStateException("NeoForge event reentrancy depth exceeds " + MAX_REENTRANCY_DEPTH);
        }
        state.scopeStarts.push(state.pending.size());
    }

    public static void append(Mutation mutation) {
        State state = STATE.get();
        if (state.failed || state.scopeStarts.isEmpty()) {
            throw new IllegalStateException("MutationBuffer requires an active, valid event");
        }
        state.pending.add(Objects.requireNonNull(mutation));
    }

    /** Drop this scope's mutations, including successfully completed nested scopes. */
    public static void cancelCurrent() {
        State state = STATE.get();
        if (state.failed || state.scopeStarts.isEmpty()) {
            throw new IllegalStateException("MutationBuffer has no active, valid event to cancel");
        }
        state.pending.subList(state.scopeStarts.peek(), state.pending.size()).clear();
    }

    public static void endEvent() {
        State state = STATE.get();
        if (state.scopeStarts.isEmpty()) throw new IllegalStateException("MutationBuffer event scope underflow");
        state.scopeStarts.pop();
    }

    public static int depth() { return STATE.get().scopeStarts.size(); }

    /** Return an immutable batch exactly once, after the outermost scope has exited. */
    public static List<Mutation> commitBatch() {
        State state = STATE.get();
        if (state.failed || !state.scopeStarts.isEmpty()) {
            throw new IllegalStateException("MutationBuffer batch is failed or event scopes are still active");
        }
        List<Mutation> batch = List.copyOf(state.pending);
        STATE.remove();
        return batch;
    }

    public static void discardAll() { STATE.remove(); }
}
