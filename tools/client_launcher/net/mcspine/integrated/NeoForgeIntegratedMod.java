package net.mcspine.integrated;

import net.neoforged.fml.common.Mod;

/** NeoForge loader adapter, activated only with mc.spine.integrated.enabled. */
@Mod("mc_spine_integrated")
public final class NeoForgeIntegratedMod {
    public NeoForgeIntegratedMod() {
        IntegratedServerBridge.startConfigured();
    }
}
