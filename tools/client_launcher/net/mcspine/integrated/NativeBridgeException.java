package net.mcspine.integrated;

/** Fail-closed native bridge error. {@link #status()} is the C status or -1 for bridge-level failures. */
public final class NativeBridgeException extends RuntimeException {
    private final int status;

    public NativeBridgeException(String message) {
        this(message, -1, null);
    }

    public NativeBridgeException(String message, Throwable cause) {
        this(message, -1, cause);
    }

    public NativeBridgeException(String message, int status) {
        this(message, status, null);
    }

    private NativeBridgeException(String message, int status, Throwable cause) {
        super(message, cause);
        this.status = status;
    }

    public int status() {
        return status;
    }
}
