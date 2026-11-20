package net.neoforged.neoforge.registries;

import java.util.Collection;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.function.Supplier;
import net.minecraft.core.Holder;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.item.BlockItem;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;

public class DeferredRegister<T> {
    private final ResourceKey<? extends Registry<T>> registry;
    private final String namespace;
    private final Map<DeferredHolder<T, ? extends T>, Consumer<RegisterEvent>> entries = new LinkedHashMap<>();
    private boolean attached;
    private boolean registered;

    private DeferredRegister(ResourceKey<? extends Registry<T>> registry, String namespace) {
        RegisterEvent.valueType(registry);
        this.registry = registry;
        this.namespace = Objects.requireNonNull(namespace);
    }

    public static <T> DeferredRegister<T> create(ResourceKey<? extends Registry<T>> registry, String namespace) {
        return new DeferredRegister<>(registry, namespace);
    }

    public static <T> DeferredRegister<T> create(ResourceLocation registry, String namespace) {
        return create(ResourceKey.<T>createRegistryKey(registry), namespace);
    }

    public static Blocks createBlocks(String namespace) { return new Blocks(namespace); }
    public static Items createItems(String namespace) { return new Items(namespace); }

    public <I extends T> DeferredHolder<T, I> register(String name, Supplier<? extends I> factory) {
        return register(name, id -> factory.get());
    }

    public <I extends T> DeferredHolder<T, I> register(String name, Function<ResourceLocation, ? extends I> factory) {
        return defer(name, factory, DeferredHolder::new);
    }

    final <I extends T, H extends DeferredHolder<T, I>> H defer(String path,
            Function<ResourceLocation, ? extends I> factory, Function<ResourceKey<T>, H> holderFactory) {
        if (registered) throw new IllegalStateException("Registry already submitted: " + registry.location());
        Objects.requireNonNull(factory);
        var id = ResourceLocation.fromNamespaceAndPath(namespace, path);
        H holder = holderFactory.apply(ResourceKey.create(registry, id));
        entries.put(holder, event -> holder.bind(event.registerValue(registry, id, () -> factory.apply(id))));
        return holder;
    }

    public void register(IEventBus bus) {
        if (attached) throw new IllegalStateException("Deferred register already attached: " + registry.location());
        bus.addListener(RegisterEvent.class, this::submit);
        attached = true;
    }

    private void submit(RegisterEvent event) {
        if (registry != event.getRegistryKey()) return;
        if (registered) throw new IllegalStateException("Registry already submitted: " + registry.location());
        registered = true;
        entries.values().forEach(entry -> entry.accept(event));
    }

    public Collection<DeferredHolder<T, ? extends T>> getEntries() {
        return java.util.Collections.unmodifiableSet(entries.keySet());
    }
    public ResourceKey<? extends Registry<T>> getRegistryKey() { return registry; }
    public ResourceLocation getRegistryName() { return registry.location(); }
    public String getNamespace() { return namespace; }

    public static class Blocks extends DeferredRegister<Block> {
        private Blocks(String namespace) { super(Registries.BLOCK, namespace); }

        @Override
        public <B extends Block> DeferredBlock<B> register(String name, Supplier<? extends B> factory) {
            return register(name, id -> factory.get());
        }

        @Override
        public <B extends Block> DeferredBlock<B> register(String name, Function<ResourceLocation, ? extends B> factory) {
            return defer(name, factory, DeferredBlock::new);
        }

        public <B extends Block> DeferredBlock<B> registerBlock(String name,
                Function<BlockBehaviour.Properties, ? extends B> factory, BlockBehaviour.Properties properties) {
            return register(name, () -> factory.apply(properties));
        }

        public <B extends Block> DeferredBlock<B> registerBlock(String name,
                Function<BlockBehaviour.Properties, ? extends B> factory) {
            return registerBlock(name, factory, BlockBehaviour.Properties.of());
        }

        public DeferredBlock<Block> registerSimpleBlock(String name, BlockBehaviour.Properties properties) {
            return registerBlock(name, Block::new, properties);
        }

        public DeferredBlock<Block> registerSimpleBlock(String name) {
            return registerBlock(name, Block::new);
        }
    }

    public static class Items extends DeferredRegister<Item> {
        private Items(String namespace) { super(Registries.ITEM, namespace); }

        @Override
        public <I extends Item> DeferredItem<I> register(String name, Supplier<? extends I> factory) {
            return register(name, id -> factory.get());
        }

        @Override
        public <I extends Item> DeferredItem<I> register(String name, Function<ResourceLocation, ? extends I> factory) {
            return defer(name, factory, DeferredItem::new);
        }

        public <I extends Item> DeferredItem<I> registerItem(String name,
                Function<Item.Properties, ? extends I> factory, Item.Properties properties) {
            return register(name, () -> factory.apply(properties));
        }

        public <I extends Item> DeferredItem<I> registerItem(String name, Function<Item.Properties, ? extends I> factory) {
            return registerItem(name, factory, new Item.Properties());
        }

        public DeferredItem<Item> registerSimpleItem(String name, Item.Properties properties) {
            return registerItem(name, Item::new, properties);
        }

        public DeferredItem<Item> registerSimpleItem(String name) {
            return registerItem(name, Item::new);
        }

        public DeferredItem<BlockItem> registerSimpleBlockItem(String name, Supplier<? extends Block> block,
                Item.Properties properties) {
            return register(name, () -> new BlockItem(block.get(), properties));
        }

        public DeferredItem<BlockItem> registerSimpleBlockItem(String name, Supplier<? extends Block> block) {
            return registerSimpleBlockItem(name, block, new Item.Properties());
        }

        public DeferredItem<BlockItem> registerSimpleBlockItem(Holder<Block> block, Item.Properties properties) {
            return registerSimpleBlockItem(block.unwrapKey().orElseThrow().location().getPath(), block::value, properties);
        }

        public DeferredItem<BlockItem> registerSimpleBlockItem(Holder<Block> block) {
            return registerSimpleBlockItem(block, new Item.Properties());
        }
    }
}
