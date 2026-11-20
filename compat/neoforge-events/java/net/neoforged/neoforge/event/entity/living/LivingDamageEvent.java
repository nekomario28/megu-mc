package net.neoforged.neoforge.event.entity.living;

import net.minecraft.world.entity.LivingEntity;
import net.neoforged.neoforge.common.damagesource.DamageContainer;

/** Host-driven NeoForge damage event at the pre-absorption stage. */
public abstract class LivingDamageEvent extends LivingEvent {
    private LivingDamageEvent(LivingEntity entity) { super(entity); }

    public static class Pre extends LivingDamageEvent {
        private final DamageContainer container;

        public Pre(LivingEntity entity, DamageContainer container) {
            super(entity);
            this.container = container;
        }

        public DamageContainer getContainer() { return container; }
        public float getOriginalDamage() { return container.getOriginalDamage(); }
        public float getNewDamage() { return container.getNewDamage(); }
        public void setNewDamage(float damage) { container.setNewDamage(damage); }
    }
}
