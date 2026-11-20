package auditfixture.transforms;

import net.minecraft.resources.ResourceKey;
import net.minecraft.world.level.Level;
import net.neoforged.fml.common.Mod;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.ModifyVariable;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mod("audit_transforms")
public final class Transforms {}

@Mixin(Level.class)
abstract class WorldMixin {
    @Inject(method = "setBlock", at = @At("HEAD"))
    private void beforeSetBlock(CallbackInfoReturnable<Boolean> callback) {}

    @Inject(method = "setBlock", at = @At("TAIL"))
    private void afterSetBlock(CallbackInfoReturnable<Boolean> callback) {}

    @Redirect(method = "setBlock", at = @At(value = "INVOKE",
        target = "Lnet/minecraft/world/level/Level;isOutsideBuildHeight(Lnet/minecraft/core/BlockPos;)Z"))
    private boolean withinBuildHeight(Level level, net.minecraft.core.BlockPos pos) {
        return false;
    }

    @ModifyVariable(method = "setBlock", at = @At("STORE"), ordinal = 0)
    private boolean changeFlag(boolean flag) {
        return flag;
    }

    @Overwrite
    public ResourceKey<Level> dimension() {
        return Level.OVERWORLD;
    }
}

@Mixin(Level.class)
abstract class HooksOnlyMixin {
    @Inject(method = "close", at = @At("HEAD"))
    private void beforeClose(CallbackInfo callback) {}

    @Inject(method = "close", at = @At("TAIL"))
    private void afterClose(CallbackInfo callback) {}
}
