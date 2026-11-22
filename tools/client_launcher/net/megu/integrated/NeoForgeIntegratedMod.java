package net.megu.integrated;

import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.client.event.ClientPlayerNetworkEvent;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.GameShuttingDownEvent;

/** NeoForge loader adapter, activated only with megu.integrated.enabled. */
@Mod("megu_integrated")
public final class NeoForgeIntegratedMod {
    public NeoForgeIntegratedMod() {
        IntegratedServerBridge.startConfigured();
        if (Boolean.getBoolean("megu.integrated.enabled")) {
            registerLifecycleHooks();
        }
    }

    /** Close the integrated server on world logout and on game shutdown. */
    static void registerLifecycleHooks() {
        NeoForge.EVENT_BUS.addListener((ClientPlayerNetworkEvent.LoggingOut event) ->
                IntegratedServerBridge.closeConfigured());
        NeoForge.EVENT_BUS.addListener((GameShuttingDownEvent event) ->
                IntegratedServerBridge.closeConfigured());
    }
}
