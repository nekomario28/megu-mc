package net.mcspine.neoforge;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.jar.Attributes;
import java.util.jar.Manifest;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import org.objectweb.asm.AnnotationVisitor;
import org.objectweb.asm.ClassReader;
import org.objectweb.asm.ClassVisitor;
import org.objectweb.asm.ConstantDynamic;
import org.objectweb.asm.FieldVisitor;
import org.objectweb.asm.Handle;
import org.objectweb.asm.Label;
import org.objectweb.asm.MethodVisitor;
import org.objectweb.asm.Opcodes;
import org.objectweb.asm.Type;

/** Static, conservative 1.21.1 NeoForge server-compatibility gate. Never loads mod classes. */
public final class ShimCheck {
    private static final String MOD = "Lnet/neoforged/fml/common/Mod;";
    private static final String SUBSCRIBER = "Lnet/neoforged/fml/common/EventBusSubscriber;";
    private static final String ONLY_IN = "Lnet/neoforged/api/distmarker/OnlyIn;";
    private static final Set<String> REJECTED_INJECTIONS = Set.of(
        "Redirect", "Overwrite", "ModifyVariable", "ModifyArg", "ModifyConstant", "ModifyExpressionValue",
        "WrapOperation", "ModifyReturnValue", "WrapWithCondition");
    private static final String[] PREFIXES = {"net/minecraft/", "net/neoforged/"};
    private static final int MAX_CLASS_BYTES = 16 * 1024 * 1024;
    private static final int MAX_METADATA_BYTES = 1024 * 1024;

    private ShimCheck() { }

    public record Rejection(String code, String symbol, String detail) { }
    public record ModEntry(String modId, String className) { }
    public record Subscriber(String modId, String className, String bus) { }
    public record VerifiedJar(Path path, List<String> modIds, List<ModEntry> entries,
                              List<Subscriber> subscribers, List<Rejection> rejections) { }
    public record Discovery(List<VerifiedJar> jars) {
        public boolean compatible() {
            return jars.stream().allMatch(jar -> jar.rejections().isEmpty());
        }
        public String rejectionDetails() {
            List<String> details = new ArrayList<>();
            for (VerifiedJar jar : jars) {
                for (Rejection reason : jar.rejections()) {
                    details.add(jar.path() + " [" + reason.code() + "] " + reason.symbol()
                        + " (" + reason.detail() + ")");
                }
            }
            return String.join("; ", details);
        }
    }

    /** Audit the entire mod set before any application class is loaded. */
    public static Discovery verifyMods(List<Path> jars, Surface surface) {
        List<JarReport> reports = new ArrayList<>();
        for (Path jar : jars) {
            try {
                reports.add(scan(jar, surface));
            } catch (IOException error) {
                JarReport report = new JarReport(jar.toString());
                report.reject("INVALID_JAR", jar.toString(), error.getMessage());
                reports.add(report);
            }
        }
        checkInterModDependencies(reports);
        List<VerifiedJar> verified = new ArrayList<>();
        for (int i = 0; i < reports.size(); i++) {
            JarReport report = reports.get(i);
            List<ModEntry> entries = new ArrayList<>();
            List<Subscriber> subscribers = new ArrayList<>();
            for (ClassInfo info : report.classes.values()) {
                if (info.modId != null && !info.client && report.mods.containsKey(info.modId)) {
                    entries.add(new ModEntry(info.modId, info.name.replace('/', '.')));
                }
                if (info.subscriber && !info.client) {
                    String owner = info.subscriberModId.isEmpty() && report.mods.size() == 1
                        ? report.mods.keySet().iterator().next() : info.subscriberModId;
                    if (report.mods.containsKey(owner)) {
                        subscribers.add(new Subscriber(owner, info.name.replace('/', '.'), info.subscriberBus));
                    }
                }
            }
            verified.add(new VerifiedJar(jars.get(i), List.copyOf(report.mods.keySet()),
                List.copyOf(entries), List.copyOf(subscribers),
                report.reasons.stream().map(r -> new Rejection(r.code, r.symbol, r.detail)).toList()));
        }
        return new Discovery(List.copyOf(verified));
    }

    public static void main(String[] args) {
        boolean json = false;
        Path surface = Path.of("compat/neoforge-events/shim-surface.json");
        List<Path> jars = new ArrayList<>();
        try {
            for (int i = 0; i < args.length; i++) {
                switch (args[i]) {
                    case "--json" -> json = true;
                    case "--surface" -> {
                        if (++i == args.length) throw new IOException("--surface requires a path");
                        surface = Path.of(args[i]);
                    }
                    default -> {
                        if (args[i].startsWith("--")) throw new IOException("Unknown option: " + args[i]);
                        jars.add(Path.of(args[i]));
                    }
                }
            }
            if (jars.isEmpty()) throw new IOException("Provide at least one mod .jar");
            Surface available = Surface.read(surface);
            List<JarReport> reports = new ArrayList<>();
            for (Path jar : jars) reports.add(scan(jar, available));
            checkInterModDependencies(reports);
            boolean compatible = reports.stream().allMatch(r -> r.reasons.isEmpty());
            Map<String, Object> output = new LinkedHashMap<>();
            output.put("status", compatible ? "COMPATIBLE" : "REJECTED");
            output.put("minecraft", "1.21.1");
            output.put("neoforge", "21.1.172");
            output.put("jars", reports.stream().map(JarReport::report).toList());
            if (json) System.out.println(Json.encode(output));
            else {
                System.out.println("NeoForge shimcheck: " + output.get("status"));
                for (JarReport report : reports) {
                    System.out.println(report.path + "  " + report.status() + "  symbols "
                        + report.supported.size() + "/" + report.referenced.size());
                    for (Reason reason : report.reasons) System.out.println("  " + reason.code + " " + reason.symbol + " " + reason.detail);
                }
            }
            System.exit(compatible ? 0 : 2);
        } catch (IOException | IllegalArgumentException error) {
            System.err.println("shimcheck: " + error.getMessage());
            System.exit(1);
        }
    }

    private static JarReport scan(Path path, Surface surface) throws IOException {
        if (!path.toString().endsWith(".jar")) throw new IOException("Expected a .jar: " + path);
        JarReport result = new JarReport(path.toString());
        try (ZipFile zip = new ZipFile(path.toFile())) {
            var entries = zip.stream().filter(e -> !e.isDirectory()).sorted(Comparator.comparing(ZipEntry::getName)).toList();
            Set<String> names = new HashSet<>();
            for (ZipEntry entry : entries) {
                if (!names.add(entry.getName())) result.reject("DUPLICATE_JAR_ENTRY", entry.getName(), "Repeated zip entry");
            }
            ZipEntry metadata = zip.getEntry("META-INF/neoforge.mods.toml");
            if (metadata == null) result.reject("MISSING_METADATA", "META-INF/neoforge.mods.toml", "Missing NeoForge metadata");
            else {
                try { result.metadata = Metadata.parse(read(zip, metadata, MAX_METADATA_BYTES)); }
                catch (IllegalArgumentException error) { result.reject("MALFORMED_METADATA", metadata.getName(), error.getMessage()); }
            }
            for (ZipEntry entry : entries) {
                if (!entry.getName().endsWith(".class")) continue;
                try {
                    byte[] bytes = read(zip, entry, MAX_CLASS_BYTES);
                    ClassInfo info = new ClassInfo();
                    new ClassReader(bytes).accept(new AuditVisitor(info), ClassReader.SKIP_FRAMES);
                    if (!entry.getName().equals(info.name + ".class")) {
                        result.reject("CLASS_PATH_MISMATCH", entry.getName(), info.name);
                    }
                    result.classes.put(info.name, info);
                } catch (IOException | RuntimeException error) {
                    result.reject("MALFORMED_CLASS", entry.getName(), error.getMessage());
                }
            }
            checkMixins(zip, result);
            if (result.metadata != null) validateMetadata(result);
            Set<String> ownClasses = result.classes.keySet();
            for (ClassInfo info : result.classes.values()) {
                if (info.client) {
                    result.clientClasses.add(info.name);
                    continue;
                }
                for (String symbol : info.symbols) {
                    result.referenced.add(symbol);
                    if (surface.supports(symbol)) result.supported.add(symbol);
                    else result.reject("UNSUPPORTED_SYMBOL", symbol, "Referenced from " + info.name);
                }
                for (String reference : info.ownReferences) {
                    if (ownClasses.contains(reference) && result.classes.get(reference).client) {
                        result.reject("SERVER_REFERENCES_CLIENT_CLASS", reference, "Referenced from " + info.name);
                    }
                }
            }
            result.unsupported.addAll(result.referenced);
            result.unsupported.removeAll(result.supported);
        }
        return result;
    }

    private static byte[] read(ZipFile zip, ZipEntry entry, int max) throws IOException {
        if (entry.getSize() > max) throw new IOException("Entry too large: " + entry.getName());
        try (InputStream stream = zip.getInputStream(entry)) {
            byte[] data = stream.readNBytes(max + 1);
            if (data.length > max) throw new IOException("Entry too large: " + entry.getName());
            return data;
        }
    }

    private static void validateMetadata(JarReport result) {
        Metadata meta = result.metadata;
        if (!"javafml".equals(meta.root.get("modLoader"))) {
            result.reject("UNSUPPORTED_MOD_LOADER", "modLoader", String.valueOf(meta.root.get("modLoader")));
        }
        validateVersion(result, "loaderVersion", meta.root.get("loaderVersion"), "4.0.39", "INVALID_LOADER_VERSION");
        if (meta.mods.isEmpty()) result.reject("MISSING_MOD", "[[mods]]", "No mod entries");
        Set<String> ids = new HashSet<>();
        if (meta.root.containsKey("clientSideOnly") && !(meta.root.get("clientSideOnly") instanceof Boolean)) {
            result.reject("INVALID_CLIENT_SIDE_ONLY", "clientSideOnly", "Expected boolean");
        }
        for (Map<String, Object> mod : meta.mods) {
            String id = string(mod, "modId");
            if (id == null || !id.matches("[a-z][a-z0-9_]{1,63}")) {
                result.reject("INVALID_MOD_ID", String.valueOf(id), "Invalid or missing modId");
                continue;
            }
            if (!ids.add(id)) result.reject("DUPLICATE_MOD_ID", id, "Duplicate [[mods]]");
            if (string(mod, "version") == null || string(mod, "version").isBlank()) {
                result.reject("MISSING_MOD_VERSION", id, "Missing version");
            }
            if (string(mod, "displayName") == null || string(mod, "displayName").isBlank()) {
                result.reject("MISSING_DISPLAY_NAME", id, "Missing displayName");
            }
            result.mods.put(id, string(mod, "version"));
            if (mod.containsKey("clientSideOnly") && !(mod.get("clientSideOnly") instanceof Boolean)) {
                result.reject("INVALID_CLIENT_SIDE_ONLY", id + ".clientSideOnly", "Expected boolean");
            }
            if (Boolean.TRUE.equals(mod.get("clientSideOnly"))) result.clientSideOnly = true;
        }
        if (Boolean.TRUE.equals(meta.root.get("clientSideOnly"))) result.clientSideOnly = true;
        if (result.clientSideOnly) result.reject("CLIENT_ONLY_MOD", result.path, "Mod declares clientSideOnly=true");
        for (var entry : meta.dependencies.entrySet()) {
            String owner = entry.getKey();
            if (!ids.contains(owner)) result.reject("UNKNOWN_DEPENDENCY_OWNER", owner, "No matching [[mods]]");
            for (Map<String, Object> dep : entry.getValue()) {
                String id = string(dep, "modId");
                String range = string(dep, "versionRange");
                Object mandatory = dep.get("mandatory");
                String type = string(dep, "type");
                if ((mandatory != null && !(mandatory instanceof Boolean))
                    || (dep.containsKey("type") && type == null)
                    || (type != null && !Set.of("required", "optional", "incompatible", "discouraged").contains(type))) {
                    result.reject("INVALID_DEPENDENCY", owner, "Invalid mandatory/type value for " + id);
                    continue;
                }
                boolean required = Boolean.TRUE.equals(mandatory) || "required".equals(type);
                if (mandatory == null && type == null) required = true;
                if (mandatory != null && type != null && Boolean.TRUE.equals(mandatory) != "required".equals(type)) {
                    result.reject("INVALID_DEPENDENCY", owner, "Contradictory mandatory/type for " + id);
                    continue;
                }
                if (type != null && (type.equals("incompatible") || type.equals("discouraged"))) {
                    result.reject("UNSUPPORTED_DEPENDENCY_TYPE", id, "S0 does not implement dependency type " + type);
                    continue;
                }
                if (id == null || range == null) {
                    result.reject("INVALID_DEPENDENCY", owner, "Missing modId or versionRange");
                    continue;
                }
                String side = string(dep, "side");
                if (side == null && !dep.containsKey("side")) side = "BOTH";
                if (side == null || !Set.of("BOTH", "CLIENT", "SERVER").contains(side.toUpperCase(java.util.Locale.ROOT))) {
                    result.reject("INVALID_DEPENDENCY", owner, "Invalid side for " + id);
                    continue;
                }
                result.dependencies.add(new Dependency(owner, id, range, required, side));
                if (required && !side.equalsIgnoreCase("CLIENT") && (id.equals("neoforge") || id.equals("minecraft"))) {
                    validateVersion(result, id, range, id.equals("neoforge") ? "21.1.172" : "1.21.1", "INCOMPATIBLE_DEPENDENCY");
                }
            }
        }
        for (ClassInfo info : result.classes.values()) {
            if (info.modId != null && !ids.contains(info.modId)) {
                result.reject("MOD_ANNOTATION_MISMATCH", info.name, "@Mod(" + info.modId + ") missing from TOML");
            }
            if (info.subscriber) {
                if (!Set.of("GAME", "MOD").contains(info.subscriberBus)) {
                    result.reject("INVALID_SUBSCRIBER_BUS", info.name, info.subscriberBus);
                }
                if (!info.subscriberModId.isEmpty() && !ids.contains(info.subscriberModId)) {
                    result.reject("UNKNOWN_SUBSCRIBER_MOD", info.name, info.subscriberModId);
                } else if (info.subscriberModId.isEmpty() && ids.size() != 1) {
                    result.reject("AMBIGUOUS_SUBSCRIBER_MOD", info.name, "Specify modid for multi-mod JAR");
                }
            }
        }
        for (String id : ids) {
            if (result.classes.values().stream().noneMatch(c -> id.equals(c.modId))) {
                result.reject("MISSING_MOD_ANNOTATION", id, "No @Mod class");
            }
        }
    }

    private static String string(Map<String, Object> properties, String key) {
        Object value = properties.get(key);
        return value instanceof String text ? text : null;
    }

    private static void validateVersion(JarReport report, String label, Object range, String current, String code) {
        if (!(range instanceof String text) || !VersionRange.valid(text)) {
            report.reject(code, label, "Invalid range " + range);
        } else if (!VersionRange.contains(text, current)) {
            report.reject(code, label, current + " outside " + text);
        }
    }

    private static void checkInterModDependencies(List<JarReport> reports) {
        Map<String, String> installed = new HashMap<>();
        Map<String, JarReport> owners = new TreeMap<>();
        Map<String, ClassInfo> allClasses = new HashMap<>();
        for (JarReport report : reports) {
            for (var entry : report.mods.entrySet()) {
                if (installed.putIfAbsent(entry.getKey(), entry.getValue()) != null) {
                    report.reject("DUPLICATE_MOD_ID", entry.getKey(), "Already declared by another JAR");
                }
                owners.putIfAbsent(entry.getKey(), report);
            }
            for (ClassInfo info : report.classes.values()) {
                if (allClasses.putIfAbsent(info.name, info) != null) {
                    report.reject("DUPLICATE_CLASS", info.name, "Already declared by another JAR");
                }
            }
        }
        Map<String, Set<String>> edges = new TreeMap<>();
        for (JarReport report : reports) {
            for (ClassInfo info : report.classes.values()) {
                if (info.client) continue;
                for (String ref : info.ownReferences) {
                    ClassInfo external = allClasses.get(ref);
                    if (external != null && external.client) {
                        report.reject("SERVER_REFERENCES_CLIENT_CLASS", ref, "Referenced from " + info.name);
                    }
                }
            }
            for (Dependency dep : report.dependencies) {
                if (dep.id.equals("neoforge") || dep.id.equals("minecraft") || !dep.required
                    || dep.side.equalsIgnoreCase("CLIENT")) continue;
                String version = installed.get(dep.id);
                if (version == null) report.reject("MISSING_DEPENDENCY", dep.id, "Required " + dep.range);
                else {
                    try {
                        if (!VersionRange.valid(dep.range) || !VersionRange.contains(dep.range, version)) {
                            report.reject("INCOMPATIBLE_DEPENDENCY", dep.id, version + " outside " + dep.range);
                        } else {
                            edges.computeIfAbsent(dep.owner, ignored -> new TreeSet<>()).add(dep.id);
                        }
                    } catch (IllegalArgumentException error) {
                        report.reject("INCOMPATIBLE_DEPENDENCY", dep.id, error.getMessage());
                    }
                }
            }
        }
        for (String owner : installed.keySet()) {
            findDependencyCycle(owner, edges, new TreeSet<>(), new ArrayList<>(), owners);
        }
    }

    private static void findDependencyCycle(String node, Map<String, Set<String>> edges, Set<String> visited,
                                            List<String> path, Map<String, JarReport> owners) {
        if (path.contains(node)) {
            List<String> cycle = new ArrayList<>(path.subList(path.indexOf(node), path.size()));
            cycle.add(node);
            String detail = String.join(" -> ", cycle);
            for (String id : cycle) owners.get(id).reject("CYCLIC_DEPENDENCY", id, detail);
            return;
        }
        if (!visited.add(node)) return;
        path.add(node);
        for (String next : edges.getOrDefault(node, Set.of())) {
            findDependencyCycle(next, edges, visited, path, owners);
        }
        path.remove(path.size() - 1);
    }

    private static void checkMixins(ZipFile zip, JarReport result) throws IOException {
        Set<String> configs = new TreeSet<>();
        if (result.metadata != null) {
            for (Map<String, Object> declaration : result.metadata.mixins) {
                String config = string(declaration, "config");
                if (config == null || config.isBlank()) result.reject("INVALID_MIXIN_DECLARATION", "[[mixins]]", "Missing config");
                else configs.add(config);
            }
            for (Map<String, Object> declaration : result.metadata.accessTransformers) {
                result.reject("UNSUPPORTED_ACCESS_TRANSFORMER", String.valueOf(declaration.getOrDefault("file", "[[accessTransformers]]")), "Access transformers are unsupported in S0");
            }
        }
        ZipEntry manifestEntry = zip.getEntry("META-INF/MANIFEST.MF");
        if (manifestEntry != null) {
            try (InputStream stream = zip.getInputStream(manifestEntry)) {
                String value = new Manifest(stream).getMainAttributes().getValue("MixinConfigs");
                if (value != null) for (String config : value.split(",")) if (!config.isBlank()) configs.add(config.trim());
            }
        }
        for (ClassInfo info : result.classes.values()) {
            for (String injection : info.injections) {
                if (REJECTED_INJECTIONS.contains(injection)) {
                    result.reject("UNSUPPORTED_MIXIN_INJECTION", info.name + " @" + injection, "Unsupported S0 injection");
                }
            }
            for (String at : info.atValues) {
                if (!Set.of("HEAD", "RETURN", "TAIL").contains(at)) {
                    result.reject("UNSUPPORTED_MIXIN_AT", info.name + " @At(" + at + ")", "Unsupported injection point");
                }
            }
        }
        for (String config : configs) {
            if (config.startsWith("/") || config.contains("..") || !config.endsWith(".json")) {
                result.reject("INVALID_MIXIN_CONFIG", config, "Unsafe config path");
                continue;
            }
            ZipEntry entry = zip.getEntry(config);
            if (entry == null) {
                result.reject("MISSING_MIXIN_CONFIG", config, "Declared mixin config not in JAR");
                continue;
            }
            try {
                Map<String, Object> doc = Json.object(Json.parse(new String(read(zip, entry, MAX_METADATA_BYTES), StandardCharsets.UTF_8)));
                String pkg = doc.get("package") instanceof String value ? value : "";
                for (String section : List.of("mixins", "server", "client")) {
                    Object classes = doc.get(section);
                    if (classes == null) continue;
                    for (Object element : Json.array(classes)) {
                        if (!(element instanceof String name)) throw new IllegalArgumentException("Invalid mixin class");
                        String internal = (pkg.isBlank() ? name : pkg + "." + name).replace('.', '/');
                        if (!result.classes.containsKey(internal)) {
                            result.reject("MISSING_MIXIN_CLASS", internal, "Referenced by " + config);
                        }
                    }
                }
                // There is no Mixin transformer in NF-0; even HEAD/RETURN/TAIL cannot execute yet.
                result.reject("UNSUPPORTED_MIXIN_CONFIG", config, "No mixin transformer in S0");
            } catch (IllegalArgumentException error) {
                result.reject("INVALID_MIXIN_CONFIG", config, error.getMessage());
            }
        }
    }

    private record Dependency(String owner, String id, String range, boolean required, String side) { }
    private record Reason(String code, String symbol, String detail) implements Comparable<Reason> {
        @Override public int compareTo(Reason other) {
            int byCode = code.compareTo(other.code);
            if (byCode != 0) return byCode;
            int bySymbol = symbol.compareTo(other.symbol);
            return bySymbol != 0 ? bySymbol : detail.compareTo(other.detail);
        }
        Map<String, String> asMap() { return Map.of("code", code, "symbol", symbol, "detail", detail); }
    }

    private static final class JarReport {
        final String path;
        final Map<String, ClassInfo> classes = new TreeMap<>();
        final Map<String, String> mods = new TreeMap<>();
        final List<Dependency> dependencies = new ArrayList<>();
        final TreeSet<String> referenced = new TreeSet<>();
        final TreeSet<String> supported = new TreeSet<>();
        final TreeSet<String> unsupported = new TreeSet<>();
        final TreeSet<String> clientClasses = new TreeSet<>();
        final TreeSet<Reason> reasons = new TreeSet<>();
        Metadata metadata;
        boolean clientSideOnly;
        JarReport(String path) { this.path = path; }
        void reject(String code, String symbol, String detail) { reasons.add(new Reason(code, symbol, detail)); }
        String status() { return reasons.isEmpty() ? "COMPATIBLE" : "REJECTED"; }
        Map<String, Object> report() {
            Map<String, Object> item = new LinkedHashMap<>();
            item.put("jar", path);
            item.put("status", status());
            item.put("modIds", mods.keySet());
            item.put("clientSideOnly", clientSideOnly);
            item.put("clientClasses", clientClasses);
            item.put("coverage", Map.of("supported", supported.size(), "total", referenced.size(),
                "unsupported", unsupported.size(), "percent", referenced.isEmpty() ? 100 : 100.0 * supported.size() / referenced.size()));
            item.put("supportedSymbols", supported);
            item.put("unsupportedSymbols", unsupported);
            item.put("annotations", classes.values().stream().filter(info -> !info.annotations.isEmpty()).map(info -> {
                Map<String, Object> entry = new LinkedHashMap<>();
                entry.put("class", info.name);
                entry.put("annotations", info.annotations);
                if (info.modId != null) entry.put("modId", info.modId);
                if (info.subscriber) {
                    entry.put("eventBusSubscriber", Map.of("bus", info.subscriberBus, "dist", info.subscriberDist));
                }
                return entry;
            }).toList());
            item.put("unsupportedMixins", reasons.stream().filter(r -> r.code.contains("MIXIN")).map(Reason::asMap).toList());
            item.put("unsupportedAccessTransformers", reasons.stream().filter(r -> r.code.contains("ACCESS_TRANSFORMER")).map(Reason::asMap).toList());
            item.put("reasons", reasons.stream().map(Reason::asMap).toList());
            return item;
        }
    }

    private static final class ClassInfo {
        String name;
        String modId;
        final Set<String> modDist = new TreeSet<>();
        boolean modDistExplicit;
        boolean client;
        boolean onlyInClient;
        boolean subscriber;
        String subscriberModId = "";
        String subscriberBus = "GAME";
        final Set<String> subscriberDist = new TreeSet<>();
        final Set<String> symbols = new TreeSet<>();
        final Set<String> ownReferences = new TreeSet<>();
        final Set<String> injections = new TreeSet<>();
        final Set<String> atValues = new TreeSet<>();
        final Set<String> annotations = new TreeSet<>();
        void ref(String owner) {
            if (owner == null) return;
            if (owner.startsWith("[")) {
                descriptor(owner);
                return;
            }
            for (String prefix : PREFIXES) if (owner.startsWith(prefix)) { symbols.add(owner); return; }
            ownReferences.add(owner);
        }
        void member(String owner, String name, String desc, boolean method) {
            ref(owner);
            for (String prefix : PREFIXES) if (owner.startsWith(prefix)) {
                symbols.add(owner + "#" + name + (method ? desc : ":" + desc));
                break;
            }
            descriptor(desc);
        }
        void descriptor(String desc) {
            if (desc == null) return;
            try { type(Type.getType(desc)); }
            catch (IllegalArgumentException ignored) { /* A generic signature is handled separately. */ }
        }
        void type(Type value) {
            switch (value.getSort()) {
                case Type.ARRAY -> type(value.getElementType());
                case Type.OBJECT -> ref(value.getInternalName());
                case Type.METHOD -> {
                    for (Type arg : value.getArgumentTypes()) type(arg);
                    type(value.getReturnType());
                }
                default -> { }
            }
        }
        void constant(Object value) {
            if (value instanceof Type type) type(type);
            if (value instanceof Handle handle) member(handle.getOwner(), handle.getName(), handle.getDesc(),
                handle.getTag() != Opcodes.H_GETFIELD && handle.getTag() != Opcodes.H_GETSTATIC
                    && handle.getTag() != Opcodes.H_PUTFIELD && handle.getTag() != Opcodes.H_PUTSTATIC);
            if (value instanceof ConstantDynamic dynamic) {
                descriptor(dynamic.getDescriptor());
                constant(dynamic.getBootstrapMethod());
                for (int i = 0; i < dynamic.getBootstrapMethodArgumentCount(); i++) constant(dynamic.getBootstrapMethodArgument(i));
            }
        }
    }

    private static final class AuditVisitor extends ClassVisitor {
        final ClassInfo info;
        AuditVisitor(ClassInfo info) { super(Opcodes.ASM9); this.info = info; }
        @Override public void visit(int version, int access, String name, String signature, String parent, String[] interfaces) {
            info.name = name;
            info.ref(parent);
            if (interfaces != null) for (String iface : interfaces) info.ref(iface);
            signature(signature);
        }
        void signature(String signature) {
            if (signature == null) return;
            try {
                new org.objectweb.asm.signature.SignatureReader(signature).accept(
                    new org.objectweb.asm.signature.SignatureVisitor(Opcodes.ASM9) {
                        @Override public void visitClassType(String name) { info.ref(name); }
                        @Override public void visitInnerClassType(String name) { info.ref(name); }
                    });
            } catch (IllegalArgumentException ignored) { /* Malformed generics cannot confer support. */ }
        }
        AnnotationVisitor annotation(String desc) { return annotation(desc, false); }
        AnnotationVisitor annotation(String desc, boolean classLevel) {
            info.descriptor(desc);
            if (desc.contains("/spongepowered/asm/mixin/")) {
                String simple = desc.substring(desc.lastIndexOf('/') + 1, desc.length() - 1);
                info.injections.add(simple);
            }
            return new AnnotationVisitor(Opcodes.ASM9) {
                @Override public void visit(String key, Object value) {
                    info.constant(value);
                    if (MOD.equals(desc) && "value".equals(key) && value instanceof String id) info.modId = id;
                    if (SUBSCRIBER.equals(desc) && "modid".equals(key) && value instanceof String id) info.subscriberModId = id;
                    if (desc.endsWith("/At;") && "value".equals(key) && value instanceof String at) info.atValues.add(at);
                }
                @Override public void visitEnum(String key, String enumDesc, String value) {
                    info.descriptor(enumDesc);
                    if (SUBSCRIBER.equals(desc) && "value".equals(key) && enumDesc.endsWith("/Dist;")) info.subscriberDist.add(value);
                    if (MOD.equals(desc) && "dist".equals(key) && enumDesc.endsWith("/Dist;")) info.modDist.add(value);
                    if (SUBSCRIBER.equals(desc) && enumDesc.endsWith("/EventBusSubscriber$Bus;")) info.subscriberBus = value;
                    if (classLevel && ONLY_IN.equals(desc) && "CLIENT".equals(value)) info.onlyInClient = true;
                }
                @Override public AnnotationVisitor visitArray(String key) {
                    if (MOD.equals(desc) && "dist".equals(key)) info.modDistExplicit = true;
                    return new AnnotationVisitor(Opcodes.ASM9) {
                        @Override public void visitEnum(String ignored, String enumDesc, String value) {
                            info.descriptor(enumDesc);
                            if (SUBSCRIBER.equals(desc) && "value".equals(key) && enumDesc.endsWith("/Dist;")) {
                                info.subscriberDist.add(value);
                            }
                            if (MOD.equals(desc) && "dist".equals(key) && enumDesc.endsWith("/Dist;")) {
                                info.modDist.add(value);
                            }
                        }
                    };
                }
                @Override public AnnotationVisitor visitAnnotation(String key, String nested) { return annotation(nested); }
            };
        }
        @Override public AnnotationVisitor visitAnnotation(String desc, boolean visible) {
            if (SUBSCRIBER.equals(desc)) info.subscriber = true;
            if (MOD.equals(desc) || SUBSCRIBER.equals(desc) || ONLY_IN.equals(desc) || desc.contains("/spongepowered/asm/mixin/")) {
                info.annotations.add(desc.substring(1, desc.length() - 1));
            }
            return annotation(desc, true);
        }
        @Override public void visitEnd() {
            info.client = info.onlyInClient
                || (info.subscriber && info.subscriberDist.contains("CLIENT")
                    && !info.subscriberDist.contains("DEDICATED_SERVER"))
                || (info.modId != null && info.modDistExplicit && !info.modDist.contains("DEDICATED_SERVER"));
        }
        @Override public FieldVisitor visitField(int access, String name, String desc, String signature, Object value) {
            info.descriptor(desc);
            signature(signature);
            info.constant(value);
            return new FieldVisitor(Opcodes.ASM9) {
                @Override public AnnotationVisitor visitAnnotation(String desc, boolean visible) { return annotation(desc); }
            };
        }
        @Override public MethodVisitor visitMethod(int access, String name, String desc, String signature, String[] exceptions) {
            info.descriptor(desc);
            signature(signature);
            if (exceptions != null) for (String exception : exceptions) info.ref(exception);
            return new MethodVisitor(Opcodes.ASM9) {
                @Override public AnnotationVisitor visitAnnotation(String desc, boolean visible) { return annotation(desc); }
                @Override public AnnotationVisitor visitParameterAnnotation(int parameter, String desc, boolean visible) { return annotation(desc); }
                @Override public void visitTypeInsn(int opcode, String type) { info.ref(type); }
                @Override public void visitFieldInsn(int opcode, String owner, String name, String desc) { info.member(owner, name, desc, false); }
                @Override public void visitMethodInsn(int opcode, String owner, String name, String desc, boolean itf) { info.member(owner, name, desc, true); }
                @Override public void visitInvokeDynamicInsn(String name, String desc, Handle bootstrap, Object... args) {
                    info.descriptor(desc);
                    info.constant(bootstrap);
                    for (Object arg : args) info.constant(arg);
                }
                @Override public void visitLdcInsn(Object value) { info.constant(value); }
                @Override public void visitMultiANewArrayInsn(String desc, int dims) { info.descriptor(desc); }
                @Override public void visitTryCatchBlock(Label start, Label end, Label handler, String type) { info.ref(type); }
                @Override public void visitLocalVariable(String name, String desc, String signature, Label start, Label end, int index) {
                    info.descriptor(desc);
                    signature(signature);
                }
                @Override public AnnotationVisitor visitInsnAnnotation(int ref, org.objectweb.asm.TypePath path, String desc, boolean visible) {
                    return annotation(desc);
                }
            };
        }
    }

    public static final class Surface {
        private final Map<String, Set<String>> methods = new TreeMap<>();
        private final Map<String, Set<String>> fields = new TreeMap<>();
        static Surface read(Path path) throws IOException {
            return parse(Files.readString(path, StandardCharsets.UTF_8));
        }
        public static Surface readFromResource() throws IOException {
            try (InputStream stream = ShimCheck.class.getResourceAsStream("shim-surface.json")) {
                if (stream == null) throw new IOException("Bundled NeoForge shim-surface.json missing");
                return parse(new String(stream.readAllBytes(), StandardCharsets.UTF_8));
            }
        }
        private static Surface parse(String json) throws IOException {
            Map<String, Object> data;
            try { data = Json.object(Json.parse(json)); }
            catch (IllegalArgumentException error) { throw new IOException("Invalid surface: " + error.getMessage(), error); }
            Surface surface = new Surface();
            if (!"mc-spine-neoforge-s0".equals(data.get("schema"))) throw new IOException("Unsupported surface schema");
            Map<String, Object> classes = Json.object(data.get("classes"));
            for (var entry : classes.entrySet()) {
                if (!entry.getKey().startsWith("net/minecraft/") && !entry.getKey().startsWith("net/neoforged/")) {
                    throw new IOException("Invalid surface class: " + entry.getKey());
                }
                Map<String, Object> members = Json.object(entry.getValue());
                surface.methods.put(entry.getKey(), parseMembers(members, "methods"));
                surface.fields.put(entry.getKey(), parseMembers(members, "fields"));
            }
            return surface;
        }
        private static Set<String> parseMembers(Map<String, Object> members, String key) {
            Set<String> result = new TreeSet<>();
            for (Object member : Json.array(members.getOrDefault(key, List.of()))) {
                if (!(member instanceof String text) || text.isEmpty() || text.contains("*")) {
                    throw new IllegalArgumentException("Invalid surface " + key + " entry");
                }
                if (!result.add(text)) throw new IllegalArgumentException("Duplicate surface member " + text);
            }
            return result;
        }
        boolean supports(String symbol) {
            int separator = symbol.indexOf('#');
            if (separator < 0) return methods.containsKey(symbol);
            String owner = symbol.substring(0, separator);
            String member = symbol.substring(separator + 1);
            int fieldSep = member.indexOf(':');
            return fieldSep >= 0 ? fields.getOrDefault(owner, Set.of()).contains(member)
                : methods.getOrDefault(owner, Set.of()).contains(member);
        }
    }

    /** Strict subset of TOML sufficient for the FML metadata contract. Rejects malformed syntax. */
    private static final class Metadata {
        final Map<String, Object> root = new LinkedHashMap<>();
        final List<Map<String, Object>> mods = new ArrayList<>();
        final Map<String, List<Map<String, Object>>> dependencies = new TreeMap<>();
        final List<Map<String, Object>> mixins = new ArrayList<>();
        final List<Map<String, Object>> accessTransformers = new ArrayList<>();
        static Metadata parse(byte[] bytes) {
            Metadata meta = new Metadata();
            String source = new String(bytes, StandardCharsets.UTF_8);
            Map<String, Object> current = meta.root;
            boolean multiline = false;
            int lineNumber = 0;
            for (String line : source.split("\\R", -1)) {
                lineNumber++;
                String trimmed = line.trim();
                if (multiline) {
                    if (trimmed.contains("\"\"\"")) multiline = false;
                    continue;
                }
                if (trimmed.isEmpty() || trimmed.startsWith("#")) continue;
                if (trimmed.startsWith("[[") && trimmed.endsWith("]]")) {
                    String section = trimmed.substring(2, trimmed.length() - 2).trim();
                    current = new LinkedHashMap<>();
                    if (section.equals("mods")) meta.mods.add(current);
                    else if (section.startsWith("dependencies.") && section.substring(13).matches("[a-zA-Z0-9_]+")) {
                        meta.dependencies.computeIfAbsent(section.substring(13), ignored -> new ArrayList<>()).add(current);
                    } else if (section.equals("mixins")) meta.mixins.add(current);
                    else if (section.equals("accessTransformers")) meta.accessTransformers.add(current);
                    else throw new IllegalArgumentException("Unknown array table at line " + lineNumber + ": " + section);
                    continue;
                }
                if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
                    // Other FML metadata sections may exist; keep parsing them for valid syntax.
                    String section = trimmed.substring(1, trimmed.length() - 1).trim();
                    if (!section.matches("[A-Za-z0-9_-]+(\\.[A-Za-z0-9_-]+)*")) {
                        throw new IllegalArgumentException("Invalid table at line " + lineNumber + ": " + section);
                    }
                    current = new LinkedHashMap<>();
                    continue;
                }
                int equals = trimmed.indexOf('=');
                if (equals <= 0) throw new IllegalArgumentException("Invalid key/value at line " + lineNumber);
                String key = trimmed.substring(0, equals).trim();
                if (!key.matches("[A-Za-z0-9_-]+")) throw new IllegalArgumentException("Invalid key at line " + lineNumber);
                String value = trimmed.substring(equals + 1).trim();
                if (value.startsWith("\"\"\"")) {
                    int close = value.indexOf("\"\"\"", 3);
                    if (close < 0) {
                        multiline = true;
                        value = "\"\"";
                    } else value = "\"" + value.substring(3, close) + "\"";
                }
                int comment = commentStart(value);
                if (comment >= 0) value = value.substring(0, comment).trim();
                if (current.putIfAbsent(key, tomlScalar(value, lineNumber)) != null) {
                    throw new IllegalArgumentException("Duplicate key at line " + lineNumber + ": " + key);
                }
            }
            if (multiline) throw new IllegalArgumentException("Unterminated multiline string");
            return meta;
        }
        private static int commentStart(String value) {
            boolean quoted = false;
            boolean escaped = false;
            for (int i = 0; i < value.length(); i++) {
                char c = value.charAt(i);
                if (c == '"' && !escaped) quoted = !quoted;
                if (c == '#' && !quoted) return i;
                escaped = c == '\\' && !escaped;
                if (c != '\\') escaped = false;
            }
            return -1;
        }
        private static Object tomlScalar(String raw, int line) {
            if (raw.equals("true")) return true;
            if (raw.equals("false")) return false;
            if (raw.matches("[+-]?[0-9]+")) return raw;
            if (raw.length() >= 2 && raw.startsWith("\"") && raw.endsWith("\"")) {
                try { return Json.parse(raw); }
                catch (IllegalArgumentException error) { throw new IllegalArgumentException("Invalid string at line " + line); }
            }
            if (raw.length() >= 2 && raw.startsWith("'") && raw.endsWith("'")) return raw.substring(1, raw.length() - 1);
            if (raw.startsWith("[")) {
                try { return Json.array(Json.parse(raw)); }
                catch (IllegalArgumentException error) { throw new IllegalArgumentException("Invalid array at line " + line); }
            }
            throw new IllegalArgumentException("Unsupported TOML value at line " + line + ": " + raw);
        }
    }

    private static final class VersionRange {
        static boolean valid(String range) {
            try { contains(range, "1.0"); return true; }
            catch (IllegalArgumentException error) { return false; }
        }
        static boolean contains(String range, String version) {
            List<Integer> candidate = numbers(version);
            if (!range.startsWith("[") && !range.startsWith("(")) return compare(candidate, numbers(range)) == 0;
            if (range.length() < 3 || !(range.endsWith("]") || range.endsWith(")"))) throw new IllegalArgumentException("Malformed range");
            if (range.startsWith("[") && range.endsWith("]") && !range.contains(",")) {
                return compare(candidate, numbers(range.substring(1, range.length() - 1))) == 0;
            }
            String[] parts = range.substring(1, range.length() - 1).split(",", -1);
            if (parts.length != 2) throw new IllegalArgumentException("Malformed range");
            if (!parts[0].isBlank()) {
                int lower = compare(candidate, numbers(parts[0].trim()));
                if (lower < 0 || (lower == 0 && range.charAt(0) == '(')) return false;
            }
            if (!parts[1].isBlank()) {
                int upper = compare(candidate, numbers(parts[1].trim()));
                if (upper > 0 || (upper == 0 && range.endsWith(")"))) return false;
            }
            return true;
        }
        private static List<Integer> numbers(String value) {
            if (!value.matches("[0-9]+(\\.[0-9]+)*")) throw new IllegalArgumentException("Unsupported version " + value);
            return Arrays.stream(value.split("\\.")).map(Integer::parseInt).toList();
        }
        private static int compare(List<Integer> a, List<Integer> b) {
            for (int i = 0; i < Math.max(a.size(), b.size()); i++) {
                int cmp = Integer.compare(i < a.size() ? a.get(i) : 0, i < b.size() ? b.get(i) : 0);
                if (cmp != 0) return cmp;
            }
            return 0;
        }
    }

    /** Small duplicate-key rejecting JSON parser for the surface and Mixin config files. */
    private static final class Json {
        private final String text;
        private int index;
        private Json(String text) { this.text = text; }
        static Object parse(String text) {
            Json parser = new Json(text);
            Object value = parser.value();
            parser.space();
            if (parser.index != text.length()) throw new IllegalArgumentException("Trailing JSON content");
            return value;
        }
        @SuppressWarnings("unchecked")
        static Map<String, Object> object(Object value) {
            if (!(value instanceof Map<?, ?>)) throw new IllegalArgumentException("Expected JSON object");
            return (Map<String, Object>) value;
        }
        static List<?> array(Object value) {
            if (!(value instanceof List<?> values)) throw new IllegalArgumentException("Expected JSON array");
            return values;
        }
        private void space() { while (index < text.length() && Character.isWhitespace(text.charAt(index))) index++; }
        private boolean take(char c) { space(); if (index < text.length() && text.charAt(index) == c) { index++; return true; } return false; }
        private Object value() {
            space();
            if (index >= text.length()) throw new IllegalArgumentException("Unexpected end of JSON");
            char c = text.charAt(index);
            if (c == '"') return string();
            if (take('{')) {
                Map<String, Object> map = new LinkedHashMap<>();
                if (!take('}')) do {
                    String key = string();
                    if (!take(':')) throw new IllegalArgumentException("Expected colon");
                    Object val = value();
                    if (map.putIfAbsent(key, val) != null) throw new IllegalArgumentException("Duplicate JSON key " + key);
                    if (take('}')) return map;
                    if (!take(',')) throw new IllegalArgumentException("Expected comma");
                } while (true);
                return map;
            }
            if (take('[')) {
                List<Object> array = new ArrayList<>();
                if (!take(']')) do {
                    array.add(value());
                    if (take(']')) return array;
                    if (!take(',')) throw new IllegalArgumentException("Expected comma");
                } while (true);
                return array;
            }
            for (String keyword : List.of("true", "false", "null")) if (text.startsWith(keyword, index)) {
                index += keyword.length();
                return keyword.equals("null") ? "null" : Boolean.valueOf(keyword);
            }
            int start = index;
            while (index < text.length() && "0123456789+-.eE".indexOf(text.charAt(index)) >= 0) index++;
            if (start != index) {
                try { return Double.parseDouble(text.substring(start, index)); }
                catch (NumberFormatException ignored) { }
            }
            throw new IllegalArgumentException("Invalid JSON value at offset " + index);
        }
        private String string() {
            if (!take('"')) throw new IllegalArgumentException("Expected JSON string at offset " + index);
            StringBuilder out = new StringBuilder();
            while (index < text.length()) {
                char c = text.charAt(index++);
                if (c == '"') return out.toString();
                if (c == '\\') {
                    if (index == text.length()) break;
                    char escaped = text.charAt(index++);
                    switch (escaped) {
                        case '"', '\\', '/' -> out.append(escaped);
                        case 'b' -> out.append('\b');
                        case 'f' -> out.append('\f');
                        case 'n' -> out.append('\n');
                        case 'r' -> out.append('\r');
                        case 't' -> out.append('\t');
                        case 'u' -> {
                            if (index + 4 > text.length()) throw new IllegalArgumentException("Invalid unicode escape");
                            out.append((char) Integer.parseInt(text.substring(index, index + 4), 16));
                            index += 4;
                        }
                        default -> throw new IllegalArgumentException("Invalid JSON escape");
                    }
                } else {
                    if (c < 32) throw new IllegalArgumentException("JSON control character");
                    out.append(c);
                }
            }
            throw new IllegalArgumentException("Unterminated JSON string");
        }
        static String encode(Object value) {
            if (value == null) return "null";
            if (value instanceof Boolean || value instanceof Number) return value.toString();
            if (value instanceof String text) {
                StringBuilder out = new StringBuilder("\"");
                for (char c : text.toCharArray()) {
                    switch (c) {
                        case '"' -> out.append("\\\"");
                        case '\\' -> out.append("\\\\");
                        case '\n' -> out.append("\\n");
                        case '\r' -> out.append("\\r");
                        case '\t' -> out.append("\\t");
                        default -> { if (c < 32) out.append(String.format("\\u%04x", (int) c)); else out.append(c); }
                    }
                }
                return out.append('"').toString();
            }
            if (value instanceof Map<?, ?> map) {
                List<String> entries = new ArrayList<>();
                map.entrySet().stream().sorted(Comparator.comparing(e -> String.valueOf(e.getKey())))
                    .forEach(entry -> entries.add(encode(String.valueOf(entry.getKey())) + ":" + encode(entry.getValue())));
                return "{" + String.join(",", entries) + "}";
            }
            if (value instanceof Iterable<?> iterable) {
                List<String> items = new ArrayList<>();
                for (Object item : iterable) items.add(encode(item));
                return "[" + String.join(",", items) + "]";
            }
            throw new IllegalArgumentException("Cannot encode " + value.getClass());
        }
    }
}
