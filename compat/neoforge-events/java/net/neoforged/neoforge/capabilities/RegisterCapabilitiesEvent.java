package net.neoforged.neoforge.capabilities;

import net.megu.neoforge.S4CapabilityBridge;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.entity.BlockEntityType;
import net.neoforged.bus.api.Event;
import net.neoforged.fml.event.IModBusEvent;

public class RegisterCapabilitiesEvent extends Event implements IModBusEvent {
    public <T, C> void registerBlock(BlockCapability<T, C> capability, IBlockCapabilityProvider<T, C> provider,
            Block... blocks) {
        S4CapabilityBridge.register(capability, provider, blocks);
    }

    public boolean isBlockRegistered(BlockCapability<?, ?> capability, Block block) {
        return S4CapabilityBridge.isRegistered(capability, block);
    }

    public <T, C, BE extends BlockEntity> void registerBlockEntity(BlockCapability<T, C> capability,
            BlockEntityType<BE> type, ICapabilityProvider<? super BE, C, T> provider) {
        S4CapabilityBridge.registerBlockEntity(capability, type, provider);
    }
}
