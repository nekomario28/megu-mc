package net.mcspine.neoforge;

import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.bus.api.BusBuilder;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.neoforge.capabilities.RegisterCapabilitiesEvent;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.network.event.RegisterPayloadHandlersEvent;
import net.neoforged.neoforge.registries.RegisterEvent;

/** Static compatibility gate followed by the supported NeoForge event bootstrap. */
public final class Bootstrap {
    static Object instance;
    private static URLClassLoader modLoader;
    private static final Map<String, Object> instances = new LinkedHashMap<>();
    private static final Map<String, IEventBus> modBuses = new LinkedHashMap<>();
    private static final List<Class<?>> gameSubscribers = new ArrayList<>();
    private static String frozenRegistry;
    private static boolean initialized;

    private Bootstrap() { }

    public static synchronized Class<?> initialize(String directory) throws Exception {
        if (initialized) throw new IllegalStateException("NeoForge bootstrap already initialized");
        Path mods = Path.of(directory).resolve("mods");
        Files.createDirectories(mods);
        List<Path> jars;
        try (var paths = Files.list(mods)) {
            jars = paths.sorted(Comparator.comparing(path -> path.getFileName().toString())).toList();
        }
        for (Path jar : jars) {
            if (!Files.isRegularFile(jar) || !jar.getFileName().toString().endsWith(".jar")) {
                throw new IllegalStateException("ShimCheck rejected NeoForge mod: [INVALID_MOD_ENTRY] " + jar);
            }
        }
        if (jars.isEmpty()) {
            DirectBatchBridge.beginRegistration();
            finishRegistration();
            initialized = true;
            return net.mcspine.probe.Host.class;
        }

        ShimCheck.Discovery discovery;
        try {
            discovery = ShimCheck.verifyMods(jars, ShimCheck.Surface.readFromResource());
        } catch (Exception error) {
            throw new IllegalStateException("ShimCheck rejected NeoForge mod: [INVALID_SURFACE] "
                + error.getMessage(), error);
        }
        if (!discovery.compatible()) {
            throw new IllegalStateException("ShimCheck rejected NeoForge mod: " + discovery.rejectionDetails());
        }

        // The entire mod set has passed bytecode and metadata inspection before class loading.
        modLoader = new URLClassLoader(jars.stream().map(path -> {
            try { return path.toUri().toURL(); }
            catch (java.net.MalformedURLException error) { throw new IllegalArgumentException(error); }
        }).toArray(URL[]::new), Bootstrap.class.getClassLoader());
        try {
            DirectBatchBridge.beginRegistration();
            for (ShimCheck.VerifiedJar jar : discovery.jars()) {
                for (String id : jar.modIds()) {
                    modBuses.put(id, BusBuilder.builder().startShutdown().build());
                }
            }
            for (ShimCheck.VerifiedJar jar : discovery.jars()) {
                for (ShimCheck.ModEntry entry : jar.entries()) {
                    Class<?> type = Class.forName(entry.className(), false, modLoader);
                    Object mod = construct(type, entry.modId(), modBuses.get(entry.modId()));
                    instances.putIfAbsent(entry.modId(), mod);
                    if (instance == null) instance = mod;
                }
            }
            for (ShimCheck.VerifiedJar jar : discovery.jars()) {
                for (ShimCheck.Subscriber subscriber : jar.subscribers()) {
                    Class<?> type = Class.forName(subscriber.className(), false, modLoader);
                    if (subscriber.bus().equals("GAME")) {
                        NeoForge.EVENT_BUS.register(type);
                        gameSubscribers.add(type);
                    } else {
                        modBuses.get(subscriber.modId()).register(type);
                    }
                }
            }
            for (IEventBus bus : modBuses.values()) bus.start();
            finishRegistration();
            initialized = true;
        } catch (Exception | Error error) {
            try { close(); }
            catch (java.io.IOException cleanup) { error.addSuppressed(cleanup); }
            throw error;
        }
        return net.mcspine.probe.Host.class;
    }

    private static void finishRegistration() {
        frozenRegistry = freezeRegistries();
        S4NetworkBridge.beginRegistration();
        S4CapabilityBridge.beginRegistration();
        var payloads = new RegisterPayloadHandlersEvent();
        var capabilities = new RegisterCapabilitiesEvent();
        for (IEventBus bus : modBuses.values()) bus.post(payloads);
        S4NetworkBridge.freezeRegistration();
        for (IEventBus bus : modBuses.values()) bus.post(capabilities);
        S4CapabilityBridge.freezeRegistration();
        NeoForge.EVENT_BUS.start();
        DirectBatchBridge.freezeRegistration();
    }

    private static String freezeRegistries() {
        var wire = new StringBuilder("S2\n");
        // Registry keys are ordered block, item. Finish the block event on every
        // mod bus first so item factories can resolve their deferred blocks.
        for (var key : List.of(Registries.BLOCK, Registries.ITEM)) {
            var event = new RegisterEvent(key);
            try {
                for (IEventBus bus : modBuses.values()) bus.post(event);
            } finally {
                event.closeRegistration();
            }
            var names = event.registeredNames().stream()
                .sorted(Comparator.comparing(ResourceLocation::getNamespace).thenComparing(ResourceLocation::getPath))
                .toList();
            // First free IDs in the pinned 1.21.1 vanilla block-state and item tables.
            int id = key == Registries.BLOCK ? 26_684 : 1_333;
            for (ResourceLocation name : names) {
                Object value = event.registeredValue(name);
                if (key == Registries.BLOCK) S4CapabilityBridge.bindBlock(id, name, (Block) value);
                else S4CapabilityBridge.bindItem(id, (Item) value);
                wire.append(key.location()).append('\t').append(id++).append('\t').append(name).append('\n');
            }
        }
        return wire.append("end").toString();
    }

    public static synchronized String frozenRegistryEncoded() {
        if (!initialized) throw new IllegalStateException("NeoForge registry is not frozen");
        return frozenRegistry;
    }

    private static Object construct(Class<?> type, String modId, IEventBus modBus) throws Exception {
        List<Constructor<?>> supported = new ArrayList<>();
        for (Constructor<?> constructor : type.getDeclaredConstructors()) {
            Class<?>[] parameters = constructor.getParameterTypes();
            if (parameters.length > 2) continue;
            boolean bus = false;
            boolean dist = false;
            boolean valid = true;
            for (Class<?> parameter : parameters) {
                if (parameter == IEventBus.class && !bus) bus = true;
                else if (parameter == Dist.class && !dist) dist = true;
                else valid = false;
            }
            if (valid) supported.add(constructor);
        }
        if (supported.size() != 1) {
            throw new IllegalStateException("@Mod(" + modId + ") " + type.getName()
                + " requires exactly one supported constructor (IEventBus and/or Dist): found " + supported.size());
        }
        Constructor<?> constructor = supported.getFirst();
        Object[] arguments = java.util.Arrays.stream(constructor.getParameterTypes()).map(parameter ->
            parameter == IEventBus.class ? modBus : Dist.DEDICATED_SERVER).toArray();
        try {
            constructor.setAccessible(true);
            return constructor.newInstance(arguments);
        } catch (InvocationTargetException error) {
            throw new IllegalStateException("@Mod(" + modId + ") constructor failed: " + type.getName(), error.getCause());
        }
    }

    public static synchronized Map<String, Object> instances() { return Map.copyOf(instances); }
    public static synchronized IEventBus modBus(String modId) { return modBuses.get(modId); }

    /** Release class-loader and event registrations for isolated runtimes/tests. */
    public static synchronized void close() throws java.io.IOException {
        initialized = false;
        frozenRegistry = null;
        DirectBatchBridge.clearRegistration();
        S4NetworkBridge.clearRegistration();
        S4CapabilityBridge.clearRegistration();
        for (Class<?> subscriber : gameSubscribers) NeoForge.EVENT_BUS.unregister(subscriber);
        gameSubscribers.clear();
        for (Object mod : instances.values()) NeoForge.EVENT_BUS.unregister(mod);
        instances.clear();
        modBuses.clear();
        instance = null;
        URLClassLoader loader = modLoader;
        modLoader = null;
        if (loader != null) loader.close();
    }

    public static Class<?> initializeServer() { return ServerHooks.class; }
}
