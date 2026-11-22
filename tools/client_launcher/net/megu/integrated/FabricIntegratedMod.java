package net.megu.integrated;

import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import net.fabricmc.api.ClientModInitializer;

/**
 * Fabric client-side loader adapter. Inert unless -Dmegu.integrated.enabled=true.
 *
 * <p>Lifecycle hooks are registered reflectively: Fabric API callback signatures use
 * Minecraft types that this plain-javac jar cannot reference without remapping, so
 * only the Fabric API event holders are named, and each callback is a dynamic proxy.
 */
public final class FabricIntegratedMod implements ClientModInitializer {
    private static final String LIFECYCLE = "net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientLifecycleEvents";
    private static final String CONNECTION = "net.fabricmc.fabric.api.client.networking.v1.ClientPlayConnectionEvents";
    private static final String[][] HOOKS = {
        {LIFECYCLE, "CLIENT_STOPPING", LIFECYCLE + "$ClientStopping"},
        {CONNECTION, "DISCONNECT", CONNECTION + "$Disconnect"},
    };

    @Override
    public void onInitializeClient() {
        IntegratedServerBridge.startConfigured();
        if (Boolean.getBoolean("megu.integrated.enabled")) {
            registerLifecycleHooks();
        }
    }

    /** Register client-stopping and world-disconnect hooks; returns how many were attached. */
    static int registerLifecycleHooks() {
        ClassLoader loader = FabricIntegratedMod.class.getClassLoader();
        int registered = 0;
        for (String[] hook : HOOKS) {
            try {
                Object event = Class.forName(hook[0], true, loader).getField(hook[1]).get(null);
                Class<?> callback = Class.forName(hook[2], true, loader);
                Method register = Class.forName("net.fabricmc.fabric.api.event.Event", true, loader)
                        .getMethod("register", Object.class);
                register.invoke(event, Proxy.newProxyInstance(loader, new Class<?>[] {callback},
                        (proxy, method, args) -> {
                            if (method.getDeclaringClass() == Object.class) {
                                return method.getName().equals("hashCode") ? System.identityHashCode(proxy)
                                        : method.getName().equals("equals") ? proxy == args[0]
                                        : "megu-lifecycle-hook";
                            }
                            IntegratedServerBridge.closeConfigured();
                            return null;
                        }));
                registered++;
            } catch (ReflectiveOperationException | LinkageError error) {
                System.err.println("megu integrated: Fabric hook " + hook[1] + " unavailable: " + error);
            }
        }
        return registered;
    }
}
