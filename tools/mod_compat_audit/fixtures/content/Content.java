package auditfixture.content;

import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredRegister;
import net.neoforged.neoforge.registries.RegisterEvent;

@Mod("audit_content")
public final class Content {
    public Content(IEventBus bus) {
        var blocks = DeferredRegister.createBlocks("audit_content");
        var ore = blocks.registerSimpleBlock("sample_ore", BlockBehaviour.Properties.of());
        var items = DeferredRegister.createItems("audit_content");
        items.registerSimpleBlockItem("sample_ore", ore);
        blocks.register(bus);
        items.register(bus);
        bus.addListener(Content::register);
    }

    @SubscribeEvent
    public static void register(RegisterEvent event) {
        event.register(Registries.BLOCK, ResourceLocation.fromNamespaceAndPath("audit_content", "ore_storage"),
            () -> new Block(BlockBehaviour.Properties.of()));
    }
}
