package net.megu.probe;

import com.mojang.brigadier.CommandDispatcher;
import java.io.IOException;
import java.io.PrintStream;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Modifier;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashSet;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.jar.JarFile;
import net.fabricmc.api.DedicatedServerModInitializer;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerTickEvents;
import net.fabricmc.fabric.api.event.player.PlayerBlockBreakEvents;
import net.fabricmc.fabric.api.event.player.UseBlockCallback;
import net.minecraft.block.BlockState;
import net.minecraft.command.CommandRegistryAccess;
import net.minecraft.entity.player.PlayerEntity;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.command.CommandManager;
import net.minecraft.server.command.ServerCommandSource;
import net.minecraft.util.ActionResult;
import net.minecraft.util.Hand;
import net.minecraft.util.hit.BlockHitResult;
import net.minecraft.util.math.BlockPos;
import net.minecraft.world.World;

/** Dedicated-server callback runner for one batch in a fresh Java 21 process. */
public final class FabricRuntimeAdapter {
    private static final String SCHEMA = "megu-fabric-s1-s2-v1";
    private static final int MAX_JAR_BYTES = 64 * 1024 * 1024;

    private final MinecraftServer server = new MinecraftServer();
    private final World world = new World();
    private final PlayerEntity player = new PlayerEntity();
    private String stage = "input";

    public static void main(String[] args) {
        var adapter = new FabricRuntimeAdapter();
        PrintStream report = System.out;
        // A fixture can log without corrupting the machine-readable result.
        System.setOut(System.err);
        int exit = 0;
        Map<String, Object> outcome;
        try {
            outcome = adapter.run(args);
        } catch (AdmissionFailure rejected) {
            report.println("{\"admission\":" + rejected.report
                + ",\"schema\":\"" + SCHEMA + "\",\"status\":\"REJECTED\"}");
            System.exit(2);
            return;
        } catch (Exception | LinkageError | AssertionError failure) {
            Throwable cause = failure instanceof InvocationTargetException wrapper ? wrapper.getCause() : failure;
            outcome = new LinkedHashMap<>();
            outcome.put("schema", SCHEMA);
            outcome.put("status", "ERROR");
            outcome.put("stage", adapter.stage);
            outcome.put("exception", cause.getClass().getName());
            outcome.put("message", cause.getMessage());
            exit = 1;
        }
        report.println(json(outcome));
        System.exit(exit);
    }

    private Map<String, Object> run(String[] args) throws Exception {
        if (args.length < 3) {
            throw new IllegalArgumentException("Usage: FabricRuntimeAdapter <fabric_shimcheck.py> <batch> <mod.jar>...");
        }
        Path gate = Path.of(args[0]).toRealPath();
        List<Operation> batch = parseBatch(args[1]);
        String namespace = new String(resource("/fabric-runtime-namespace"), StandardCharsets.UTF_8).strip();
        stage = "admission";
        try (var inputs = new Inputs(gate, namespace, args)) {
            stage = "entrypoint_preflight";
            var main = constructors(inputs, "main", ModInitializer.class);
            var dedicated = constructors(inputs, "server", DedicatedServerModInitializer.class);
            List<Map<String, String>> invoked = new ArrayList<>();
            RegistryStore.beginRegistration();
            stage = "entrypoints.main";
            for (Initializer init : main) {
                ((ModInitializer) init.newInstance()).onInitialize();
                invoked.add(init.report("main"));
            }
            stage = "entrypoints.server";
            for (Initializer init : dedicated) {
                ((DedicatedServerModInitializer) init.newInstance()).onInitializeServer();
                invoked.add(init.report("server"));
            }

            var registry = RegistryStore.freeze();
            var dispatcher = new CommandDispatcher<ServerCommandSource>();
            stage = "command_registration";
            CommandRegistrationCallback.EVENT.invoker().register(dispatcher, new CommandRegistryAccess() {},
                CommandManager.RegistrationEnvironment.DEDICATED);
            var commands = dispatcher.freeze();
            stage = "server_starting";
            ServerLifecycleEvents.SERVER_STARTING.invoker().onServerStarting(server);
            stage = "server_started";
            ServerLifecycleEvents.SERVER_STARTED.invoker().onServerStarted(server);
            List<Map<String, Object>> results = new ArrayList<>();
            for (Operation operation : batch) {
                stage = "batch[" + results.size() + "]." + operation.kind;
                results.add(dispatch(operation));
            }
            stage = "server_stopping";
            ServerLifecycleEvents.SERVER_STOPPING.invoker().onServerStopping(server);

            stage = "cleanup";
            return Map.of("schema", SCHEMA, "status", "OK", "namespace", namespace,
                "mods", inputs.mods.stream().map(mod -> Map.of("id", mod.id, "sha256", mod.sha)).toList(),
                "entrypoints", invoked, "commands", commands, "registry_frozen", true, "registry", registry,
                "batch", results, "surface_sha256", inputs.surfaceSha);
        }
    }

    private static byte[] resource(String name) throws IOException {
        try (var stream = FabricRuntimeAdapter.class.getResourceAsStream(name)) {
            if (stream == null) throw new IOException("Missing runtime resource " + name);
            return stream.readAllBytes();
        }
    }

    private static List<Initializer> constructors(Inputs inputs, String key, Class<?> api)
            throws ReflectiveOperationException {
        List<Initializer> result = new ArrayList<>();
        for (Mod mod : inputs.mods) {
            for (String name : mod.entries.getOrDefault(key, List.of())) {
                Class<?> type = Class.forName(name, false, mod.loader);
                if (type.getClassLoader() != mod.loader || !Modifier.isPublic(type.getModifiers())
                        || Modifier.isAbstract(type.getModifiers()) || !api.isAssignableFrom(type)) {
                    throw new IllegalArgumentException("Invalid " + key + " entrypoint: " + name);
                }
                result.add(new Initializer(mod.id, type.getConstructor()));
            }
        }
        return result;
    }

    private record Initializer(String mod, Constructor<?> constructor) {
        Object newInstance() throws ReflectiveOperationException { return constructor.newInstance(); }

        Map<String, String> report(String kind) {
            return Map.of("mod", mod, "kind", kind, "class", constructor.getDeclaringClass().getName());
        }
    }

    private record Operation(String kind, BlockPos pos) {}

    private static List<Operation> parseBatch(String value) {
        List<Operation> batch = new ArrayList<>();
        if (value.equals("-")) return batch;
        for (String token : value.split(",", -1)) {
            String[] parts = token.split(":", -1);
            if (token.equals("tick")) {
                batch.add(new Operation("tick", null));
            } else if (parts.length == 4 && (parts[0].equals("break") || parts[0].equals("use"))) {
                var pos = new BlockPos(Integer.parseInt(parts[1]), Integer.parseInt(parts[2]), Integer.parseInt(parts[3]));
                batch.add(new Operation(parts[0], pos));
            } else {
                throw new IllegalArgumentException("Invalid batch operation: " + token);
            }
        }
        return batch;
    }

    private Map<String, Object> dispatch(Operation operation) {
        if (operation.kind.equals("tick")) {
            ServerTickEvents.START_SERVER_TICK.invoker().onStartTick(server);
            ServerTickEvents.END_SERVER_TICK.invoker().onEndTick(server);
            return Map.of("kind", "tick");
        }
        BlockPos pos = operation.pos;
        var coords = List.of(pos.getX(), pos.getY(), pos.getZ());
        if (operation.kind.equals("break")) {
            var state = new BlockState();
            boolean allowed = PlayerBlockBreakEvents.BEFORE.invoker().beforeBlockBreak(world, player, pos, state, null);
            if (allowed) PlayerBlockBreakEvents.AFTER.invoker().afterBlockBreak(world, player, pos, state, null);
            return Map.of("kind", "break", "pos", coords, "canceled", !allowed, "mutations", List.of());
        }
        ActionResult result = UseBlockCallback.EVENT.invoker().interact(player, world, Hand.MAIN_HAND,
            new BlockHitResult(pos));
        return Map.of("kind", "use", "pos", coords, "result", result.name(), "handled", result != ActionResult.PASS,
            "canceled", result == ActionResult.FAIL, "mutations", List.of());
    }

    private record Mod(String id, String sha, ModClassLoader loader, Map<String, List<String>> entries) {}

    private static final class ModClassLoader extends URLClassLoader {
        private final Set<String> excluded;

        ModClassLoader(Path jar, Set<String> excluded) throws IOException {
            super(new URL[]{jar.toUri().toURL()}, FabricRuntimeAdapter.class.getClassLoader());
            this.excluded = Set.copyOf(excluded);
        }

        @Override protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (excluded.contains(name) || name.startsWith("net.megu.probe.")) {
                throw new ClassNotFoundException(name);
            }
            return super.loadClass(name, resolve);
        }
    }

    private static final class AdmissionFailure extends Exception {
        final String report;

        AdmissionFailure(String report) { this.report = report; }
    }

    /** Temporary staged JARs give the auditor and class loaders the same input bytes. */
    private static final class Inputs implements AutoCloseable {
        private final Path dir;
        private final String surfaceSha;
        private final List<Mod> mods = new ArrayList<>();

        Inputs(Path gate, String namespace, String[] args) throws Exception {
            dir = Files.createTempDirectory("fabric-runtime-");
            try {
                Path plan = dir.resolve("loader-plan.tsv");
                Path surface = dir.resolve("shim-surface.json");
                byte[] surfaceBytes = resource("/fabric-runtime-surface.json");
                Files.write(surface, surfaceBytes);
                surfaceSha = HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(surfaceBytes));
                var command = new ArrayList<>(List.of("python3", gate.toString(), "--java-home",
                    System.getProperty("java.home"), "--surface", surface.toString(),
                    "--runtime-plan", plan.toString(), "--namespace", namespace));
                List<Path> jars = new ArrayList<>();
                for (int i = 2; i < args.length; i++) {
                    Path copy = dir.resolve(jars.size() + ".jar");
                    snapshot(Path.of(args[i]), copy);
                    jars.add(copy);
                    command.add(copy.toString());
                }
                Process gateProcess = new ProcessBuilder(command).redirectError(ProcessBuilder.Redirect.INHERIT).start();
                String report;
                try (var stream = gateProcess.getInputStream()) {
                    report = new String(stream.readAllBytes(), StandardCharsets.UTF_8).strip();
                }
                switch (gateProcess.waitFor()) {
                    case 0 -> readPlan(plan, jars);
                    case 2 -> throw new AdmissionFailure(report);
                    default -> throw new IOException("Fabric admission tool failed: " + report);
                }
            } catch (Exception | Error failure) {
                try { close(); }
                catch (IOException cleanup) { failure.addSuppressed(cleanup); }
                throw failure;
            }
        }

        private static void snapshot(Path source, Path copy) throws IOException, AdmissionFailure {
            String name = copy.getFileName().toString();
            if (!Files.isRegularFile(source) || Files.size(source) > MAX_JAR_BYTES) {
                throw reject(name, "INVALID_JAR");
            }
            try (var stream = Files.newInputStream(source)) {
                byte[] bytes = stream.readNBytes(MAX_JAR_BYTES + 1);
                if (bytes.length > MAX_JAR_BYTES) throw reject(name, "INVALID_JAR");
                Files.write(copy, bytes);
            }
            try (var jar = new JarFile(copy.toFile(), false)) {
                var manifest = jar.getManifest();
                if (manifest != null) {
                    var attrs = manifest.getMainAttributes();
                    if (attrs.getValue("Class-Path") != null) throw reject(name, "UNSUPPORTED_MANIFEST_CLASS_PATH");
                    if (attrs.getValue("MixinConfigs") != null) throw reject(name, "UNSUPPORTED_RUNTIME_MIXIN");
                }
            } catch (IOException malformed) {
                throw reject(name, "INVALID_JAR");
            }
        }

        private static AdmissionFailure reject(String jar, String code) {
            return new AdmissionFailure(json(Map.of("jars", List.of(Map.of("jar", jar,
                "diagnostics", List.of(Map.of("code", code)))))));
        }

        private void readPlan(Path plan, List<Path> jars) throws IOException {
            var lines = Files.readAllLines(plan).iterator();
            String[] row = lines.next().split("\t", -1);
            for (Path jar : jars) {
                if (row.length != 3 || !row[0].equals("jar")) throw new IOException("Missing admitted JAR record");
                String id = row[1];
                String sha = row[2];
                var excluded = new HashSet<String>();
                Map<String, List<String>> entries = new LinkedHashMap<>();
                row = new String[0];
                while (lines.hasNext()) {
                    row = lines.next().split("\t", -1);
                    if (row[0].equals("jar")) break;
                    if (row.length != 2) throw new IOException("Invalid admitted JAR plan");
                    switch (row[0]) {
                        case "skip" -> excluded.add(row[1]);
                        case "main", "server" -> entries.computeIfAbsent(row[0], key -> new ArrayList<>()).add(row[1]);
                        default -> throw new IOException("Unknown admission plan record");
                    }
                    row = new String[0];
                }
                mods.add(new Mod(id, sha, new ModClassLoader(jar, excluded), entries));
            }
            if (row.length != 0) throw new IOException("Extra admitted JAR records");
        }

        @Override public void close() throws IOException {
            IOException failure = null;
            for (Mod mod : mods) {
                try { mod.loader.close(); }
                catch (IOException e) {
                    if (failure == null) failure = e;
                    else failure.addSuppressed(e);
                }
            }
            try (var paths = Files.walk(dir)) {
                for (Path path : paths.sorted(Comparator.reverseOrder()).toList()) Files.delete(path);
            } catch (IOException e) {
                if (failure == null) failure = e;
                else failure.addSuppressed(e);
            }
            if (failure != null) throw failure;
        }
    }

    private static String json(Object value) {
        if (value == null) return "null";
        if (value instanceof String s) {
            var out = new StringBuilder("\"");
            for (char c : s.toCharArray()) {
                if (c == '"' || c == '\\') out.append('\\').append(c);
                else if (c < 32 || Character.isSurrogate(c)) out.append(String.format("\\u%04x", (int) c));
                else out.append(c);
            }
            return out.append('"').toString();
        }
        if (value instanceof Boolean || value instanceof Number) return value.toString();
        if (value instanceof List<?> list) {
            return "[" + String.join(",", list.stream().map(FabricRuntimeAdapter::json).toList()) + "]";
        }
        if (value instanceof Map<?, ?> map) {
            var sorted = new TreeMap<String, Object>();
            map.forEach((key, item) -> sorted.put((String) key, item));
            return "{" + String.join(",", sorted.entrySet().stream()
                .map(e -> json(e.getKey()) + ":" + json(e.getValue())).toList()) + "}";
        }
        throw new IllegalArgumentException("Unsupported report value " + value.getClass().getName());
    }

    private FabricRuntimeAdapter() {}
}
