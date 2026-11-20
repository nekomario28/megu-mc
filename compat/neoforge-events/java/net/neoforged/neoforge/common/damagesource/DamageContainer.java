package net.neoforged.neoforge.common.damagesource;

/** Damage amounts supplied by the host at the pre-absorption stage. */
public final class DamageContainer {
    private final Thread owner = Thread.currentThread();
    private final float originalDamage;
    private float newDamage;

    public DamageContainer(float originalDamage, float newDamage) {
        this.originalDamage = originalDamage;
        this.newDamage = newDamage;
    }

    public float getOriginalDamage() { return originalDamage; }
    public float getNewDamage() { return newDamage; }

    public void setNewDamage(float damage) {
        if (Thread.currentThread() != owner) throw new IllegalStateException("Damage requires the dispatch owner");
        newDamage = damage;
    }
}
