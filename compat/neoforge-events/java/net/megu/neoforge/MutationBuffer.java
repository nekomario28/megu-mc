package net.megu.neoforge;

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

    static int requireDimension(int dim) {
        if (dim < 0 || dim > 2) throw new IllegalArgumentException("Invalid S1 dimension slot " + dim);
        return dim;
    }

    /** Versioned, tab-delimited records shared with Rust's decode_mutation_batch. */
    static String encode(boolean canceled, List<Mutation> batch, Mutation response) {
        if (canceled) {
            if (!batch.isEmpty() || response != null) {
                throw new IllegalStateException("Canceled S1 batch must be empty");
            }
            return "S1\t1\nend";
        }
        StringBuilder wire = new StringBuilder("S1\t0\n");
        for (Mutation mutation : batch) {
            switch (mutation) {
                case SetBlockIf block -> wire.append("block\t").append(requireDimension(block.dim()))
                    .append('\t').append(block.x()).append('\t').append(block.y()).append('\t').append(block.z())
                    .append('\t').append(Integer.toUnsignedString(block.newState()))
                    .append('\t').append(Integer.toUnsignedString(block.expectedState())).append('\n');
                case SendSystemMessage message -> {
                    wire.append("message\t").append(message.playerEid()).append('\t');
                    appendText(wire, message.text());
                }
                case RegisterCommand command -> appendText(wire.append("command\t"), command.literal());
                // Only the outermost event can change the host's XP or chat payload.
                case BreakExp ignored -> { }
                case ChatMessage ignored -> { }
            }
        }
        switch (response) {
            case null -> { }
            case BreakExp exp -> wire.append("exp\t").append(exp.exp()).append('\n');
            case ChatMessage chat -> appendText(wire.append("chat\t"), chat.text());
            default -> throw new IllegalArgumentException("Invalid S1 event response " + response);
        }
        return wire.append("end").toString();
    }

    private static void appendText(StringBuilder wire, String text) {
        for (int offset = 0; offset < text.length();) {
            int ch = text.codePointAt(offset);
            switch (ch) {
                case '\\' -> wire.append("\\\\");
                case '\n' -> wire.append("\\n");
                case '\r' -> wire.append("\\r");
                case '\t' -> wire.append("\\t");
                default -> {
                    if (ch >= Character.MIN_SURROGATE && ch <= Character.MAX_SURROGATE) {
                        throw new IllegalArgumentException("Unpaired surrogate in S1 text");
                    }
                    wire.appendCodePoint(ch);
                }
            }
            offset += Character.charCount(ch);
        }
        wire.append('\n');
    }
}
