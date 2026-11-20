package auditfixture.network;

import net.minecraft.core.registries.Registries;
import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.Level;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.network.event.RegisterPayloadHandlersEvent;

@Mod("audit_network")
public final class Network {
    public Network(IEventBus modBus) {
        modBus.addListener(Network::register);
    }

    public record DimensionConfig(ResourceKey<Level> dimension) implements CustomPacketPayload {
        public static final Type<DimensionConfig> TYPE =
            new Type<>(ResourceLocation.fromNamespaceAndPath("audit_network", "dimension_config"));
        public static final StreamCodec<FriendlyByteBuf, DimensionConfig> CODEC = CustomPacketPayload.codec(
            (payload, buffer) -> buffer.writeResourceLocation(payload.dimension().location()),
            buffer -> new DimensionConfig(buffer.readResourceKey(Registries.DIMENSION)));

        @Override
        public Type<DimensionConfig> type() {
            return TYPE;
        }
    }

    public static void register(RegisterPayloadHandlersEvent event) {
        var registrar = event.registrar("1");
        registrar.configurationToClient(DimensionConfig.TYPE, DimensionConfig.CODEC, (payload, context) -> {});
        registrar.playToServer(DimensionConfig.TYPE, DimensionConfig.CODEC, (payload, context) -> {});
    }

    public static ResourceKey<Level> currentDimension(Level level) {
        return level.dimension();
    }
}
