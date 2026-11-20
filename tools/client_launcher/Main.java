package net.mcspine.launcher;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.ArrayList;
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
        boolean detach = "1".equals(System.getenv("MC_SPINE_DETACH"));
        boolean integrated = false;
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
        Path binary = resolveBinary(gameDir);
        if (binary == null) {
            System.err.println("mc-spine launcher: native client not found or not executable "
                    + "(MC_SPINE_CLIENT_BIN, --gameDir/mc-spine-client, "
                    + "--gameDir/bin/mc-spine-client, PATH)");
            return 2;
        }

        List<String> command = new ArrayList<>();
        command.add(binary.toString());
        command.addAll(gameArgs);
        try {
            if (integrated) {
                return runIntegrated(gameDir, command);
            }
            Process process = new ProcessBuilder(command).inheritIO().start();
            writeManifest(detach, process.pid(), gameArgs.size());
            if (detach) {
                return 0;
            }
            return process.waitFor();
        } catch (IOException error) {
            System.err.println("mc-spine launcher: " + error.getMessage());
            return 2;
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            System.err.println("mc-spine launcher: interrupted while waiting for native client");
            return 1;
        }
    }

    private static int runIntegrated(String gameDir, List<String> client)
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
        Process bridge = new ProcessBuilder(command)
                .redirectOutput(ProcessBuilder.Redirect.INHERIT)
                .redirectError(ProcessBuilder.Redirect.INHERIT)
                .start();
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

    private static Path resolveBinary(String gameDir) {
        String configured = System.getenv("MC_SPINE_CLIENT_BIN");
        if (configured != null && !configured.isBlank()) {
            return executable(configured);
        }
        if (gameDir != null) {
            try {
                Path root = Path.of(gameDir);
                for (String suffix : new String[]{"mc-spine-client", "bin/mc-spine-client"}) {
                    Path resolved = executable(root.resolve(suffix).toString());
                    if (resolved != null) {
                        return resolved;
                    }
                }
            } catch (RuntimeException ignored) {
                // A malformed gameDir cannot be used as a native binary path.
            }
        }
        // Optional system property for a profile generated with --client-bin.
        Path profileBinary = executable(System.getProperty("mc.spine.clientBin"));
        if (profileBinary != null) {
            return profileBinary;
        }
        String path = System.getenv("PATH");
        if (path != null) {
            for (String dir : path.split(java.util.regex.Pattern.quote(
                    java.io.File.pathSeparator), -1)) {
                for (String name : new String[]{"mc-spine-client", "mc-spine"}) {
                    Path found = executable(Path.of(dir.isEmpty() ? "." : dir, name).toString());
                    if (found != null) {
                        return found;
                    }
                }
            }
        }
        return null;
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
