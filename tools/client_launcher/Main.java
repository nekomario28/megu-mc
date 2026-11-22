package net.mcspine.launcher;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.URISyntaxException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/** Launcher entry point for the native client, pure-Java integrated server bridge, and Official Launcher self-installer. */
public final class Main {
    private static final String LOOPBACK = "127.0.0.1";
    private static final String LIBRARY_COORD = "net.mcspine:bootstrapper:1.0.0";
    private static final String LIBRARY_REL = "libraries/net/mcspine/bootstrapper/1.0.0/bootstrapper-1.0.0.jar";
    private static final Set<String> QUICK_PLAY_TARGETS = Set.of(
            "--quickPlayMultiplayer", "--quickPlaySingleplayer", "--quickPlayRealms");

    private Main() {}

    public static void main(String[] args) {
        System.exit(run(args));
    }

    private static int run(String[] args) {
        if (args.length == 0) {
            return runSelfInstall(null, "1.21.1");
        }
        if ("--mc-spine-integrated-watch".equals(args[0])) {
            return runIntegratedWatch(args);
        }
        List<String> gameArgs = new ArrayList<>();
        String gameDir = null;
        String clientBin = null;
        String clientSha256 = null;
        String launcherAccessToken = null;
        String installDir = null;
        String mcVersion = "1.21.1";
        boolean installMode = false;
        boolean detach = "1".equals(System.getenv("MC_SPINE_DETACH"));
        boolean integrated = false;
        boolean tokenEnv = "1".equals(System.getenv("MC_SPINE_TOKEN_ENV"));
        for (int i = 0; i < args.length; i++) {
            String arg = args[i];
            if ("--install".equals(arg)) {
                installMode = true;
                continue;
            }
            if ("--minecraft-dir".equals(arg) && i + 1 < args.length) {
                installDir = args[++i];
                continue;
            }
            if (arg.startsWith("--minecraft-dir=")) {
                installDir = arg.substring("--minecraft-dir=".length());
                continue;
            }
            if ("--mc-version".equals(arg) && i + 1 < args.length) {
                mcVersion = args[++i];
                continue;
            }
            if (arg.startsWith("--mc-version=")) {
                mcVersion = arg.substring("--mc-version=".length());
                continue;
            }
            if ("--mc-spine-detach".equals(arg)) {
                detach = true;
                continue;
            }
            if ("--mc-spine-integrated".equals(arg)) {
                integrated = true;
                continue;
            }
            if ("--mc-spine-token-env".equals(arg)) {
                tokenEnv = true;
                continue;
            }
            String key = arg.contains("=") ? arg.substring(0, arg.indexOf('=')) : arg;
            if (tokenEnv && "--accessToken".equals(key)) {
                if (arg.contains("=")) {
                    launcherAccessToken = arg.substring(arg.indexOf('=') + 1);
                } else if (i + 1 < args.length) {
                    launcherAccessToken = args[++i];
                }
                continue;
            }
            if ("--client-bin".equals(key) || "--client-sha256".equals(key)) {
                String value;
                if (arg.contains("=")) {
                    value = arg.substring(arg.indexOf('=') + 1);
                } else if (i + 1 < args.length) {
                    value = args[++i];
                } else {
                    System.err.println("mc-spine launcher: missing value for " + key);
                    return 2;
                }
                if (value.isBlank()) {
                    System.err.println("mc-spine launcher: empty value for " + key);
                    return 2;
                }
                if ("--client-bin".equals(key)) {
                    clientBin = value;
                } else {
                    clientSha256 = value;
                }
                continue;
            }
            gameArgs.add(arg);
            if ("--gameDir".equals(arg) && i + 1 < args.length) {
                gameDir = args[i + 1];
            } else if (arg.startsWith("--gameDir=")) {
                gameDir = arg.substring("--gameDir=".length());
            }
        }

        if (installMode) {
            return runSelfInstall(installDir, mcVersion);
        }

        if (integrated && (detach || gameDir == null || gameDir.isBlank())) {
            System.err.println("mc-spine launcher: --mc-spine-integrated requires --gameDir and wait mode");
            return 2;
        }
        Path binary = resolveBinary(gameDir, clientBin);
        if (binary == null) {
            System.err.println("mc-spine launcher: native client not found or not executable "
                    + "(--client-bin, MC_SPINE_CLIENT_BIN, --gameDir/<name>, "
                    + "--gameDir/bin/<name>, PATH; Unix and Windows .exe names)");
            return 2;
        }

        List<String> command = new ArrayList<>();
        command.add(binary.toString());
        command.addAll(gameArgs);
        try {
            String expectedSha256 = clientSha256;
            if (expectedSha256 == null) {
                String profileSha256 = System.getProperty("mc.spine.clientSha256");
                expectedSha256 = profileSha256 != null && !profileSha256.isBlank()
                        ? profileSha256 : System.getenv("MC_SPINE_CLIENT_SHA256");
            }
            verifySha256(binary, expectedSha256);
            ensureExecutable(binary);
            if (integrated) {
                return runIntegratedParent(gameDir, command, launcherAccessToken);
            }
            ProcessBuilder builder = new ProcessBuilder(command).inheritIO();
            if (launcherAccessToken != null && !launcherAccessToken.isEmpty()) {
                builder.environment().put("MC_SPINE_LAUNCHER_ACCESS_TOKEN", launcherAccessToken);
            }
            Process process = builder.start();
            writeManifest("wait", detach, process.pid(), gameArgs.size(), null);
            if (detach) {
                return 0;
            }
            return process.waitFor();
        } catch (IOException error) {
            System.err.println("mc-spine launcher: " + redact(error.getMessage(), gameArgs));
            return 2;
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            System.err.println("mc-spine launcher: interrupted while waiting for native client");
            return 1;
        }
    }

    /** Self-installs this bootstrapper JAR and registers a profile in Official Launcher's launcher_profiles.json. */
    private static int runSelfInstall(String overrideDir, String mcVersion) {
        if (!"1.21.1".equals(mcVersion) && !"26.3".equals(mcVersion)) {
            System.err.println("mc-spine launcher: unsupported --mc-version: " + mcVersion);
            return 2;
        }
        try {
            Path mcDir = (overrideDir != null && !overrideDir.isBlank())
                    ? Path.of(overrideDir).toAbsolutePath().normalize()
                    : detectMinecraftDir();
            Path selfJar = selfJarPath();
            if (selfJar == null || !Files.isRegularFile(selfJar)) {
                System.err.println("mc-spine launcher: cannot locate running bootstrapper jar");
                return 2;
            }
            String versionId = "mc-spine-" + mcVersion;
            Path libDest = mcDir.resolve(LIBRARY_REL);
            Files.createDirectories(libDest.getParent());
            if (!selfJar.toRealPath().equals(libDest.toAbsolutePath().normalize())) {
                Files.copy(selfJar, libDest, StandardCopyOption.REPLACE_EXISTING);
            }
            Path verDir = mcDir.resolve("versions").resolve(versionId);
            Files.createDirectories(verDir);
            String profileJson = "{\n"
                    + "  \"id\": \"" + versionId + "\",\n"
                    + "  \"inheritsFrom\": \"" + mcVersion + "\",\n"
                    + "  \"type\": \"release\",\n"
                    + "  \"mainClass\": \"net.mcspine.launcher.Main\",\n"
                    + "  \"libraries\": [\n"
                    + "    {\"name\": \"" + LIBRARY_COORD + "\"}\n"
                    + "  ]\n"
                    + "}\n";
            atomicWriteString(verDir.resolve(versionId + ".json"), profileJson);
            updateLauncherProfiles(mcDir.resolve("launcher_profiles.json"), versionId, "Megu (" + mcVersion + ")");
            System.out.println("Installed " + versionId + " into " + mcDir);
            return 0;
        } catch (IOException error) {
            System.err.println("mc-spine launcher: install failed: " + error.getMessage());
            return 2;
        }
    }

    private static Path detectMinecraftDir() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        Path home = Path.of(System.getProperty("user.home", "."));
        if (os.contains("win")) {
            String appdata = System.getenv("APPDATA");
            return (appdata != null && !appdata.isBlank())
                    ? Path.of(appdata, ".minecraft")
                    : home.resolve("AppData").resolve("Roaming").resolve(".minecraft");
        }
        if (os.contains("mac")) {
            return home.resolve("Library").resolve("Application Support").resolve("minecraft");
        }
        return home.resolve(".minecraft");
    }

    private static Path selfJarPath() {
        try {
            var source = Main.class.getProtectionDomain().getCodeSource();
            if (source == null || source.getLocation() == null) {
                return null;
            }
            return Path.of(source.getLocation().toURI()).toAbsolutePath().normalize();
        } catch (URISyntaxException | RuntimeException error) {
            return null;
        }
    }

    private static void updateLauncherProfiles(Path profilesFile, String versionId, String displayName)
            throws IOException {
        String entry = "    \"" + versionId + "\": {\n"
                + "      \"name\": \"" + displayName + "\",\n"
                + "      \"type\": \"custom\",\n"
                + "      \"lastVersionId\": \"" + versionId + "\"\n"
                + "    }";
        if (!Files.isRegularFile(profilesFile)) {
            String fresh = "{\n  \"profiles\": {\n" + entry + "\n  },\n  \"version\": 3\n}\n";
            atomicWriteString(profilesFile, fresh);
            return;
        }
        String text = Files.readString(profilesFile, StandardCharsets.UTF_8);
        if (text.contains("\"" + versionId + "\"")) {
            return;
        }
        int pIdx = text.indexOf("\"profiles\"");
        if (pIdx < 0) {
            String fresh = "{\n  \"profiles\": {\n" + entry + "\n  },\n  \"version\": 3\n}\n";
            atomicWriteString(profilesFile, fresh);
            return;
        }
        int brace = text.indexOf('{', pIdx);
        if (brace < 0) {
            return;
        }
        int nextNonWs = brace + 1;
        while (nextNonWs < text.length() && Character.isWhitespace(text.charAt(nextNonWs))) {
            nextNonWs++;
        }
        boolean emptyProfiles = nextNonWs < text.length() && text.charAt(nextNonWs) == '}';
        String insertion = emptyProfiles ? ("\n" + entry + "\n  ") : ("\n" + entry + ",");
        String updated = text.substring(0, brace + 1) + insertion + text.substring(brace + 1);
        atomicWriteString(profilesFile, updated);
    }

    private static void atomicWriteString(Path destination, String content) throws IOException {
        Path parent = destination.toAbsolutePath().getParent();
        Files.createDirectories(parent);
        Path temp = Files.createTempFile(parent, ".mc-spine-", ".tmp");
        try {
            Files.writeString(temp, content, StandardCharsets.UTF_8);
            Files.move(temp, destination, StandardCopyOption.REPLACE_EXISTING);
        } finally {
            Files.deleteIfExists(temp);
        }
    }

    /** Pure-Java parent wrapper: spawns a tiny Java child watching stdin EOF so even SIGKILL stops the server cleanly. */
    private static int runIntegratedParent(String gameDir, List<String> client, String accessToken)
            throws IOException, InterruptedException {
        Path selfJar = selfJarPath();
        String javaBin = Path.of(System.getProperty("java.home"), "bin",
                isWindows() ? "java.exe" : "java").toString();
        List<String> cmd = new ArrayList<>();
        cmd.add(javaBin);
        cmd.add("-Djava.io.tmpdir=" + System.getProperty("java.io.tmpdir", "/tmp"));
        cmd.add("-XX:-UsePerfData");
        if (selfJar != null && Files.isRegularFile(selfJar)) {
            cmd.add("-jar");
            cmd.add(selfJar.toString());
        } else {
            cmd.add("-cp");
            cmd.add(System.getProperty("java.class.path", "."));
            cmd.add(Main.class.getName());
        }
        cmd.add("--mc-spine-integrated-watch");
        cmd.add(gameDir);
        cmd.add("--");
        cmd.addAll(client);
        ProcessBuilder builder = new ProcessBuilder(cmd)
                .redirectOutput(ProcessBuilder.Redirect.INHERIT)
                .redirectError(ProcessBuilder.Redirect.INHERIT);
        if (accessToken != null && !accessToken.isEmpty()) {
            builder.environment().put("MC_SPINE_LAUNCHER_ACCESS_TOKEN", accessToken);
        }
        Process bridge = builder.start();
        try (var parentPipe = bridge.getOutputStream()) {
            return bridge.waitFor();
        }
    }

    /** Pure-Java integrated server & client coordinator (zero python3 dependency on Windows/macOS/Linux). */
    private static int runIntegratedWatch(String[] args) {
        if (args.length < 4 || !"--".equals(args[2])) {
            System.err.println("mc-spine integrated: malformed internal watch arguments");
            return 2;
        }
        Path gameDir = Path.of(args[1]).toAbsolutePath().normalize();
        List<String> client = new ArrayList<>();
        for (int i = 3; i < args.length; i++) {
            client.add(args[i]);
        }
        AtomicBoolean stopping = new AtomicBoolean(false);
        Thread parentWatch = new Thread(() -> {
            try {
                while (System.in.read() != -1) {
                    // Wait for parent EOF (including parent SIGKILL).
                }
            } catch (IOException ignored) {
                // Broken pipe equals EOF.
            } finally {
                stopping.set(true);
            }
        }, "mc-spine-parent-eof");
        parentWatch.setDaemon(true);
        parentWatch.start();

        Process server = null;
        Process clientProc = null;
        try {
            Path serverBin = resolveServerBinary(gameDir);
            if (serverBin == null) {
                throw new IOException("mc-spine server not found or not executable (MC_SPINE_SERVER_BIN, --gameDir, PATH)");
            }
            ensureExecutable(serverBin);
            String worldName = System.getenv("MC_SPINE_INTEGRATED_WORLD");
            if (worldName == null || worldName.isBlank()) {
                worldName = "mc-spine";
            }
            if (!worldName.matches("[a-zA-Z0-9 _-]{1,64}") || ".".equals(worldName) || "..".equals(worldName)) {
                throw new IOException("world must be a single safe directory name");
            }
            Path savesDir = gameDir.resolve("saves");
            Files.createDirectories(savesDir);
            Path world = savesDir.resolve(worldName).toAbsolutePath().normalize();
            Files.createDirectories(world);
            if (!world.getParent().equals(savesDir.toAbsolutePath().normalize())) {
                throw new IOException("world path escapes saves directory");
            }
            linkRuntimeData(world, serverBin, gameDir);

            int port;
            try (ServerSocket reserved = new ServerSocket(0, 1, InetAddress.getByName(LOOPBACK))) {
                port = reserved.getLocalPort();
            }
            String endpoint = LOOPBACK + ":" + port;
            ProcessBuilder sb = new ProcessBuilder(serverBin.toString());
            sb.directory(world.toFile());
            sb.redirectErrorStream(true);
            Map<String, String> env = sb.environment();
            env.put("MC_SPINE_BIND_ADDR", LOOPBACK);
            env.put("MC_SPINE_PORT", Integer.toString(port));
            env.put("MC_SPINE_WORLD_DIR", world.toString());
            env.put("MC_SPINE_RESOURCE_PACK_URL", "");
            env.put("MC_SPINE_MAX_CONN_PER_IP", "1");
            env.remove("MC_SPINE_RCON_PORT");
            server = sb.start();
            server.getOutputStream().close();

            CountDownLatch ready = new CountDownLatch(1);
            Process finalServer = server;
            String banner = " listening " + endpoint + " ";
            Thread logReader = new Thread(() -> {
                try (BufferedReader reader = new BufferedReader(
                        new InputStreamReader(finalServer.getInputStream(), StandardCharsets.UTF_8))) {
                    String line;
                    while ((line = reader.readLine()) != null) {
                        if (line.startsWith("mc-spine ") && line.contains(banner)) {
                            ready.countDown();
                        }
                        System.err.println(line);
                    }
                } catch (IOException ignored) {
                    // Server closed output.
                }
            }, "mc-spine-server-log");
            logReader.setDaemon(true);
            logReader.start();

            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
            while (!stopping.get()) {
                if (!server.isAlive()) {
                    throw new IOException("server exited during startup (" + server.exitValue() + ")");
                }
                if (ready.await(50, TimeUnit.MILLISECONDS)) {
                    break;
                }
                if (System.nanoTime() >= deadline) {
                    throw new IOException("server did not become ready at " + endpoint);
                }
            }
            if (stopping.get()) {
                return 1;
            }
            if (!server.isAlive()) {
                throw new IOException("server exited during startup (" + server.exitValue() + ")");
            }
            List<String> clientCmd = buildIntegratedClientCommand(client, endpoint);
            ProcessBuilder cb = new ProcessBuilder(clientCmd).inheritIO();
            clientProc = cb.start();
            writeManifest("integrated", false, clientProc.pid(), clientCmd.size() - 1, endpoint);
            while (clientProc.isAlive() && !stopping.get()) {
                clientProc.waitFor(50, TimeUnit.MILLISECONDS);
            }
            if (stopping.get()) {
                stopProcess(clientProc);
                return 1;
            }
            return clientProc.exitValue();
        } catch (IOException error) {
            System.err.println("mc-spine integrated: " + redact(error.getMessage(), client));
            return 2;
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            return 1;
        } finally {
            stopProcess(clientProc);
            stopProcess(server);
        }
    }

    private static List<String> buildIntegratedClientCommand(List<String> client, String endpoint) {
        List<String> out = new ArrayList<>();
        out.add(client.get(0));
        for (int i = 1; i < client.size(); i++) {
            String arg = client.get(i);
            if (QUICK_PLAY_TARGETS.contains(arg)) {
                i++;
                continue;
            }
            int eq = arg.indexOf('=');
            String key = eq >= 0 ? arg.substring(0, eq) : arg;
            if (QUICK_PLAY_TARGETS.contains(key)) {
                continue;
            }
            out.add(arg);
        }
        out.add("--quickPlayMultiplayer");
        out.add(endpoint);
        return out;
    }

    private static void linkRuntimeData(Path world, Path binary, Path gameDir) throws IOException {
        String configured = System.getenv("MC_SPINE_SERVER_HOME");
        List<Path> candidates = new ArrayList<>();
        if (configured != null && !configured.isBlank()) {
            candidates.add(Path.of(configured).toAbsolutePath().normalize());
        } else {
            candidates.add(gameDir);
            candidates.add(Path.of("").toAbsolutePath().normalize());
            for (Path p = binary.getParent(); p != null; p = p.getParent()) {
                candidates.add(p);
            }
        }
        Path home = null;
        for (Path c : candidates) {
            if (Files.isRegularFile(c.resolve("config_packets.bin")) && Files.isDirectory(c.resolve("assets"))) {
                home = c;
                break;
            }
        }
        if (home == null && configured != null && !configured.isBlank()) {
            throw new IOException("MC_SPINE_SERVER_HOME must contain config_packets.bin and assets");
        }
        if (home == null) {
            return;
        }
        for (String name : List.of("config_packets.bin", "assets", "datapacks/vanilla")) {
            Path src = home.resolve(name);
            Path dst = world.resolve(name);
            if (Files.exists(src) && !Files.exists(dst, LinkOption.NOFOLLOW_LINKS)
                    && !Files.isSymbolicLink(dst.getParent())) {
                Files.createDirectories(dst.getParent());
                Files.createSymbolicLink(dst, src.toRealPath());
            }
        }
    }

    private static void stopProcess(Process process) {
        if (process == null || !process.isAlive()) {
            return;
        }
        process.destroy();
        try {
            if (!process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
                process.waitFor(2, TimeUnit.SECONDS);
            }
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
            process.destroyForcibly();
        }
    }

    private static Path resolveServerBinary(Path gameDir) {
        String configured = System.getenv("MC_SPINE_SERVER_BIN");
        if (configured != null && !configured.isBlank()) {
            return regularFile(configured);
        }
        List<String> names = isWindows()
                ? List.of("mc-spine.exe", "mc-spine")
                : List.of("mc-spine", "mc-spine.exe");
        for (String sub : new String[]{"", "bin/"}) {
            for (String n : names) {
                Path p = regularFile(gameDir.resolve(sub + n).toString());
                if (p != null) {
                    return p;
                }
            }
        }
        return findOnPath(names);
    }

    private static boolean isWindows() {
        return System.getProperty("os.name", "").toLowerCase(Locale.ROOT).contains("win");
    }

    private static Path regularFile(String candidate) {
        if (candidate == null || candidate.isBlank()) {
            return null;
        }
        try {
            Path path = Path.of(candidate).toAbsolutePath().normalize();
            return Files.isRegularFile(path) ? path : null;
        } catch (RuntimeException error) {
            return null;
        }
    }

    private static void ensureExecutable(Path binary) throws IOException {
        if (!Files.isExecutable(binary)) {
            binary.toFile().setExecutable(true);
            if (!Files.isExecutable(binary)) {
                throw new IOException("native client is not executable: " + binary);
            }
        }
    }

    private static List<String> binaryNames() {
        String[] unix = {"mc-spine-client-gpu", "mc-spine-client", "megu", "mc-spine"};
        String[] windows = {"mc-spine-client-gpu.exe", "mc-spine-client.exe",
                            "megu.exe", "mc-spine.exe"};
        boolean windowsOs = isWindows();
        List<String> names = new ArrayList<>();
        for (String name : windowsOs ? windows : unix) {
            names.add(name);
        }
        for (String name : windowsOs ? unix : windows) {
            names.add(name);
        }
        return names;
    }

    private static Path findOnPath(List<String> names) {
        String path = System.getenv("PATH");
        if (path == null) {
            return null;
        }
        for (String dir : path.split(java.util.regex.Pattern.quote(java.io.File.pathSeparator), -1)) {
            for (String name : names) {
                Path found = regularFile(Path.of(dir.isEmpty() ? "." : dir, name).toString());
                if (found != null && Files.isExecutable(found)) {
                    return found;
                }
            }
        }
        return null;
    }

    private static Path resolveBinary(String gameDir, String clientBin) {
        if (clientBin != null) {
            return regularFile(clientBin);
        }
        String configured = System.getenv("MC_SPINE_CLIENT_BIN");
        if (configured != null && !configured.isBlank()) {
            return regularFile(configured);
        }
        String property = System.getProperty("mc.spine.clientBin");
        if (property != null && !property.isBlank()) {
            return regularFile(property);
        }
        List<String> names = binaryNames();
        if (gameDir != null) {
            try {
                Path root = Path.of(gameDir);
                for (String suffix : new String[]{"", "bin/"}) {
                    for (String name : names) {
                        Path resolved = regularFile(root.resolve(suffix + name).toString());
                        if (resolved != null) {
                            return resolved;
                        }
                    }
                }
            } catch (RuntimeException ignored) {
                // A malformed gameDir cannot be used as a native binary path.
            }
        }
        return findOnPath(names);
    }

    private static void verifySha256(Path binary, String expected) throws IOException {
        if (expected == null || expected.isBlank()) {
            return;
        }
        if (!expected.matches("(?i)[0-9a-f]{64}")) {
            throw new IOException("--client-sha256 must be 64 hexadecimal characters");
        }
        try {
            MessageDigest sha256 = MessageDigest.getInstance("SHA-256");
            try (var input = Files.newInputStream(binary)) {
                byte[] buffer = new byte[64 * 1024];
                int count;
                while ((count = input.read(buffer)) != -1) {
                    sha256.update(buffer, 0, count);
                }
            }
            if (!HexFormat.of().formatHex(sha256.digest()).equalsIgnoreCase(expected)) {
                throw new IOException("native client SHA-256 mismatch");
            }
        } catch (NoSuchAlgorithmException error) {
            throw new IOException("SHA-256 unavailable", error);
        }
    }

    private static String redact(String diagnostic, List<String> arguments) {
        String message = diagnostic == null ? "launch failed" : diagnostic;
        for (int i = 0; i < arguments.size(); i++) {
            String argument = arguments.get(i);
            for (String key : List.of("--accessToken", "--authToken", "--clientToken", "--token")) {
                String secret = null;
                if (argument.equals(key) && i + 1 < arguments.size()) {
                    secret = arguments.get(i + 1);
                } else if (argument.startsWith(key + "=")) {
                    secret = argument.substring(key.length() + 1);
                }
                if (secret != null && !secret.isEmpty()) {
                    message = message.replace(secret, "<REDACTED>");
                }
            }
        }
        return message.replaceAll("(?i)\\bBearer\\s+[^\\s,;\\\"']+", "Bearer <REDACTED>");
    }

    private static void writeManifest(String modeName, boolean detach, long pid, int argumentCount, String endpoint)
            throws IOException {
        String destination = System.getenv("MC_SPINE_HANDOFF_MANIFEST");
        if (destination == null || destination.isBlank()) {
            return;
        }
        String mode = "integrated".equals(modeName) ? "integrated" : (detach ? "detach" : "wait");
        StringBuilder json = new StringBuilder();
        json.append("{\"mode\":\"").append(mode)
                .append("\",\"pid\":").append(pid)
                .append(",\"argumentCount\":").append(argumentCount);
        if (endpoint != null) {
            json.append(",\"endpoint\":\"").append(endpoint).append('"');
        }
        json.append("}\n");
        atomicWriteString(Path.of(destination).toAbsolutePath(), json.toString());
    }
}
