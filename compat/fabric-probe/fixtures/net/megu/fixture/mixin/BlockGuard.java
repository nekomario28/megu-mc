package net.megu.fixture.mixin;

import net.megu.probe.ProbeGame;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = ProbeGame.class, remap = false)
public abstract class BlockGuard {
    @Inject(method = "replaceBlock", at = @At("HEAD"), cancellable = true, require = 1)
    private void cancelReplace(int state, CallbackInfoReturnable<Boolean> result) {
        result.setReturnValue(false);
    }
}
