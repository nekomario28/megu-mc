package auditfixture.fabric;

import net.fabricmc.api.DedicatedServerModInitializer;
import net.fabricmc.api.EnvType;
import net.fabricmc.api.Environment;
import net.minecraft.class_1937;
import net.minecraft.class_310;
import net.minecraft.class_5321;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Environment(EnvType.SERVER)
public final class FabricServer implements DedicatedServerModInitializer {
    @Override
    public void onInitializeServer() {}

    public static class_5321<class_1937> dimension(class_1937 world) {
        return world.method_27983();
    }
}

@Environment(EnvType.CLIENT)
final class FabricClient {
    private class_310 client;
}

@Mixin(class_1937.class)
abstract class FabricMixin {
    @Inject(method = "close", at = @At("HEAD"))
    private void beforeClose(CallbackInfo callback) {}
}
