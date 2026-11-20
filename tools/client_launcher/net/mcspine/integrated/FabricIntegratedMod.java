package net.mcspine.integrated;

import net.fabricmc.api.ClientModInitializer;

/** Fabric client-side loader adapter. */
public final class FabricIntegratedMod implements ClientModInitializer {
    @Override
    public void onInitializeClient() {
        IntegratedServerBridge.startConfigured();
    }
}
