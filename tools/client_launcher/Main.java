package net.mcspine.launcher;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;

/** Launcher entry point for the native client and its optional loopback server. */
public final class Main {
    private Main() {}

    public static void main(String[] args) {
        System.exit(run(args));
    }

    private static int run(String[] args) {
        List<String> gameArgs = new ArrayList<>();
        String gameDir = null;
        String clientBin = null;
        String clientSha256 = null;
        String launcherAccessToken = null;
        boolean detach = "1".equals(System.getenv("MC_SPINE_DETACH"));
        boolean integrated = false;
        boolean tokenEnv = "1".equals(System.getenv("MC_SPINE_TOKEN_ENV"));
        for (int i = 0; i < args.length; i++) {
            String arg = args[i];
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
            if (integrated) {
                return runIntegrated(gameDir, command, launcherAccessToken);
            }
            ProcessBuilder builder = new ProcessBuilder(command).inheritIO();
            if (launcherAccessToken != null && !launcherAccessToken.isEmpty()) {
                builder.environment().put("MC_SPINE_LAUNCHER_ACCESS_TOKEN", launcherAccessToken);
            }
            Process process = builder.start();
            writeManifest(detach, process.pid(), gameArgs.size());
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

    private static int runIntegrated(String gameDir, List<String> client, String accessToken)
            throws IOException, InterruptedException {
        List<String> command;
        try (var source = Main.class.getResourceAsStream("/integrated_bridge.py")) {
            if (source == null) {
                throw new IOException("integrated_bridge.py missing from bootstrapper jar");
            }
            command = new ArrayList<>(List.of("python3", "-c",
                    new String(source.readAllBytes(), StandardCharsets.UTF_8),
                    "--parent-stdin", "--game-dir", gameDir, "--"));
        }
        command.addAll(client);

        // The child watches stdin for EOF, including if the JVM exits abruptly.
        ProcessBuilder builder = new ProcessBuilder(command)
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

    private static Path executable(String candidate) {
        if (candidate == null || candidate.isBlank()) {
            return null;
        }
        try {
            Path path = Path.of(candidate).toAbsolutePath().normalize();
            return Files.isRegularFile(path) && Files.isExecutable(path) ? path : null;
        } catch (RuntimeException error) {
            return null;
        }
    }

    /** Candidate native client names; platform-preferred order, both OSes searched. */
    private static List<String> binaryNames() {
        String[] unix = {"mc-spine-client-gpu", "mc-spine-client", "megu", "mc-spine"};
        String[] windows = {"mc-spine-client-gpu.exe", "mc-spine-client.exe",
                            "megu.exe", "mc-spine.exe"};
        boolean windowsOs = System.getProperty("os.name", "")
                .toLowerCase(java.util.Locale.ROOT).contains("win");
        List<String> names = new ArrayList<>();
        for (String name : windowsOs ? windows : unix) {
            names.add(name);
        }
        for (String name : windowsOs ? unix : windows) {
            names.add(name);
        }
        return names;
    }

    private static Path resolveBinary(String gameDir, String clientBin) {
        if (clientBin != null) {
            return executable(clientBin);
        }
        String configured = System.getenv("MC_SPINE_CLIENT_BIN");
        if (configured != null && !configured.isBlank()) {
            return executable(configured);
        }
        String property = System.getProperty("mc.spine.clientBin");
        if (property != null && !property.isBlank()) {
            return executable(property);
        }
        List<String> names = binaryNames();
        if (gameDir != null) {
            try {
                Path root = Path.of(gameDir);
                for (String suffix : new String[]{"", "bin/"}) {
                    for (String name : names) {
                        Path resolved = executable(root.resolve(suffix + name).toString());
                        if (resolved != null) {
                            return resolved;
                        }
                    }
                }
            } catch (RuntimeException ignored) {
                // A malformed gameDir cannot be used as a native binary path.
            }
        }
        String path = System.getenv("PATH");
        if (path != null) {
            for (String dir : path.split(java.util.regex.Pattern.quote(
                    java.io.File.pathSeparator), -1)) {
                for (String name : names) {
                    Path found = executable(Path.of(dir.isEmpty() ? "." : dir, name).toString());
                    if (found != null) {
                        return found;
                    }
                }
            }
        }
        return null;
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

    private static void writeManifest(boolean detach, long pid, int argumentCount)
            throws IOException {
        String destination = System.getenv("MC_SPINE_HANDOFF_MANIFEST");
        if (destination == null || destination.isBlank()) {
            return;
        }
        Path path = Path.of(destination).toAbsolutePath();
        Path parent = path.getParent();
        Files.createDirectories(parent);
        Path temp = Files.createTempFile(parent, ".mc-spine-handoff-", ".json");
        try {
            // Authentication tokens and other game arguments must never enter the manifest.
            String json = "{\"mode\":\"" + (detach ? "detach" : "wait")
                    + "\",\"pid\":" + pid + ",\"argumentCount\":" + argumentCount + "}\n";
            Files.writeString(temp, json, StandardCharsets.UTF_8);
            Files.move(temp, path, StandardCopyOption.REPLACE_EXISTING);
        } finally {
            Files.deleteIfExists(temp);
        }
    }
}
