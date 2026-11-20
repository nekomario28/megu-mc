package auditfixture.gameplay;

import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.event.RegisterCommandsEvent;
import net.neoforged.neoforge.event.ServerChatEvent;
import net.neoforged.neoforge.event.entity.living.LivingDamageEvent;
import net.neoforged.neoforge.event.level.BlockEvent;

@Mod("audit_gameplay")
@EventBusSubscriber(modid = "audit_gameplay")
public final class Gameplay {
    @SubscribeEvent
    public static void protect(BlockEvent.BreakEvent event) {
        if (event.getPos().getY() < 0) {
            event.setCanceled(true);
        }
    }

    @SubscribeEvent
    public static void limitDamage(LivingDamageEvent.Pre event) {
        event.setNewDamage(Math.min(event.getNewDamage(), 8));
    }

    @SubscribeEvent
    public static void moderate(ServerChatEvent event) {
        if (event.getRawText().isBlank()) {
            event.setCanceled(true);
        }
    }

    @SubscribeEvent
    public static void register(RegisterCommandsEvent event) {
        event.getDispatcher();
    }
}
