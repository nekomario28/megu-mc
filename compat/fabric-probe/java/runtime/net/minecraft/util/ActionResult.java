package net.minecraft.util;

public enum ActionResult {
    SUCCESS, SUCCESS_NO_ITEM_USED, CONSUME, CONSUME_PARTIAL, PASS, FAIL;

    public boolean isAccepted() {
        return this != PASS && this != FAIL;
    }
}
