package net.neoforged.neoforge.registries;

import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.function.Consumer;
import java.util.function.Supplier;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.neoforged.bus.api.Event;
import net.neoforged.fml.event.IModBusEvent;

public class RegisterEvent extends Event implements IModBusEvent {
    private final ResourceKey<? extends Registry<?>> registry;
    private final Thread owner = Thread.currentThread();
    private final Set<ResourceLocation> registered = new HashSet<>();
    private boolean open = true;
    private Throwable failure;

    /** Bootstrap owns the registration window. */
    public RegisterEvent(ResourceKey<? extends Registry<?>> registry) {
        valueType(registry);
        this.registry = registry;
    }

    public ResourceKey<? extends Registry<?>> getRegistryKey() { return registry; }

    public <T> void register(ResourceKey<? extends Registry<T>> key, ResourceLocation name, Supplier<T> factory) {
        if (key == registry) registerValue(key, name, factory);
        else valueType(key);
    }

    public <T> void register(ResourceKey<? extends Registry<T>> key, Consumer<RegisterHelper<T>> consumer) {
        if (key == registry) {
            requireOpen();
            consumer.accept((name, value) -> registerValue(key, name, () -> value));
        } else {
            valueType(key);
        }
    }

    public interface RegisterHelper<T> {
        default void register(ResourceKey<T> key, T value) { register(key.location(), value); }
        void register(ResourceLocation name, T value);
    }

    final <T, I extends T> I registerValue(ResourceKey<? extends Registry<T>> key, ResourceLocation name,
            Supplier<? extends I> factory) {
        requireOpen();
        try {
            if (key != registry) throw new IllegalArgumentException("Wrong registry key: " + key);
            if (name == null || name.getNamespace().equals("minecraft")) {
                throw new IllegalArgumentException("Invalid dynamic registry name: " + name);
            }
            if (!registered.add(name)) {
                throw new IllegalStateException("Duplicate registry entry " + key.location() + " " + name);
            }
            I value = factory.get();
            if (!valueType(key).isInstance(value)) {
                throw new IllegalArgumentException("Invalid registry value for " + key.location() + " " + name);
            }
            return value;
        } catch (RuntimeException | Error error) {
            failure = error;
            throw error;
        }
    }

    private void requireOpen() {
        if (Thread.currentThread() != owner || !open) {
            throw new IllegalStateException("Registration outside the register phase");
        }
        if (failure != null) throw new IllegalStateException("Registration already failed", failure);
    }

    public final void closeRegistration() { open = false; }

    public final List<ResourceLocation> registeredNames() {
        if (open) throw new IllegalStateException("Registration phase still open");
        if (failure != null) throw new IllegalStateException("Registration failed", failure);
        return List.copyOf(registered);
    }

    static Class<?> valueType(ResourceKey<?> key) {
        if (key == Registries.BLOCK) return Block.class;
        if (key == Registries.ITEM) return Item.class;
        throw new IllegalArgumentException("Unsupported registry key: " + key);
    }
}
