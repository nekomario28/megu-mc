package net.megu.launcher;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.URISyntaxException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.InvalidPathException;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.attribute.PosixFilePermissions;
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
import java.time.ZoneOffset;
import java.time.format.DateTimeFormatter;

/** Launcher entry point for the native client, pure-Java integrated server bridge, and Official Launcher self-installer. */
public final class Main {
    private static final String LOOPBACK = "127.0.0.1";
    private static final String LIBRARY_COORD = "net.megu:bootstrapper:1.0.0";
    private static final String LIBRARY_REL = "libraries/net/megu/bootstrapper/1.0.0/bootstrapper-1.0.0.jar";
    private static final DateTimeFormatter ISO_UTC =
            DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH:mm:ssxxx").withZone(ZoneOffset.UTC);
    private static final String RUN_SH = """
            #!/bin/sh
            # megu dedicated server launcher (Linux/macOS).
            cd "$(dirname "$0")" || exit 1
            if [ "$EULA" != "TRUE" ] && ! grep -q '^eula=true' eula.txt 2>/dev/null; then
                echo "megu: agree to the Minecraft EULA first:" >&2
                echo "  put 'eula=true' into eula.txt, or re-run with EULA=TRUE." >&2
                exit 1
            fi
            if [ -x ./megu ]; then
                exec ./megu "$@"
            fi
            exec megu "$@"
            """;
    private static final String RUN_BAT = "@echo off\r\n"
            + "rem megu dedicated server launcher (Windows).\r\n"
            + "cd /d \"%~dp0\"\r\n"
            + "if /i \"%EULA%\"==\"TRUE\" goto :run\r\n"
            + "findstr /b /l /c:\"eula=true\" eula.txt >nul 2>&1\r\n"
            + "if errorlevel 1 (\r\n"
            + "    echo megu: agree to the Minecraft EULA first ^(eula=true in eula.txt, or EULA=TRUE^) 1>&2\r\n"
            + "    exit /b 1\r\n"
            + ")\r\n"
            + ":run\r\n"
            + "megu.exe %*\r\n";
    private static final String SERVER_README = """
            megu dedicated server
            =========================

            1. Accept the Minecraft EULA before the first launch. This installer
               NEVER sets eula=true for you. Either create eula.txt yourself:
                   eula=true
               or pass the environment variable EULA=TRUE.

            2. Start the server:
                   ./run.sh      (Linux / macOS)
                   run.bat       (Windows)
               Extra arguments are forwarded to the server binary via "$@" / %*.

            3. --server-bin stages the native binary next to these scripts as
               megu / megu.exe. Without it the scripts use megu(.exe)
               from this directory or PATH.
            """;
    private static final Set<String> QUICK_PLAY_TARGETS = Set.of(
            "--quickPlayMultiplayer", "--quickPlaySingleplayer", "--quickPlayRealms");

    private Main() {}

    public static void main(String[] args) {
        System.exit(run(args));
    }

    private static int run(String[] args) {
        if (args.length == 0) {
            if (!"1".equals(System.getenv("MEGU_HEADLESS"))
                    && !java.awt.GraphicsEnvironment.isHeadless()) {
                return InstallerGui.show();
            }
            return runSelfInstall(null, "1.21.1", true, null, null);
        }
        if ("--megu-integrated-watch".equals(args[0])) {
            return runIntegratedWatch(args);
        }
        List<String> gameArgs = new ArrayList<>();
        String gameDir = null;
        String clientBin = null;
        String clientSha256 = null;
        String launcherAccessToken = null;
        String installDir = null;
        String installServerDir = null;
        String serverBin = null;
        String mcVersion = "1.21.1";
        String handoffManifest = null;
        boolean installMode = false;
        boolean uninstallMode = false;
        boolean noProfile = false;
        boolean detach = "1".equals(System.getenv("MEGU_DETACH"));
        boolean integrated = false;
        boolean tokenEnv = "1".equals(System.getenv("MEGU_TOKEN_ENV"));
        boolean handoffFile = "1".equals(System.getenv("MEGU_HANDOFF_FILE"));
        for (int i = 0; i < args.length; i++) {
            String arg = args[i];
            if ("--install".equals(arg)) {
                installMode = true;
                continue;
            }
            if ("--uninstall".equals(arg)) {
                uninstallMode = true;
                continue;
            }
            if ("--no-profile".equals(arg)) {
                noProfile = true;
                continue;
            }
            if ("--install-server".equals(arg) || arg.startsWith("--install-server=")) {
                if (arg.contains("=")) {
                    installServerDir = arg.substring(arg.indexOf('=') + 1);
                } else if (i + 1 < args.length) {
                    installServerDir = args[++i];
                } else {
                    System.err.println("megu launcher: missing value for --install-server");
                    return 2;
                }
                continue;
            }
            if ("--server-bin".equals(arg) || arg.startsWith("--server-bin=")) {
                if (arg.contains("=")) {
                    serverBin = arg.substring(arg.indexOf('=') + 1);
                } else if (i + 1 < args.length) {
                    serverBin = args[++i];
                } else {
                    System.err.println("megu launcher: missing value for --server-bin");
                    return 2;
                }
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
            if ("--megu-detach".equals(arg)) {
                detach = true;
                continue;
            }
            if ("--megu-integrated".equals(arg)) {
                integrated = true;
                continue;
            }
            if ("--megu-token-env".equals(arg)) {
                tokenEnv = true;
                continue;
            }
            if ("--megu-handoff-file".equals(arg)) {
                handoffFile = true;
                continue;
            }
            if ("--megu-handoff-manifest".equals(arg)) {
                if (i + 1 >= args.length) {
                    System.err.println("megu launcher: missing value for --megu-handoff-manifest");
                    return 2;
                }
                handoffManifest = args[++i];
                handoffFile = true;
                continue;
            }
            if (arg.startsWith("--megu-handoff-manifest=")) {
                handoffManifest = arg.substring("--megu-handoff-manifest=".length());
                handoffFile = true;
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
                    System.err.println("megu launcher: missing value for " + key);
                    return 2;
                }
                if (value.isBlank()) {
                    System.err.println("megu launcher: empty value for " + key);
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

        if (uninstallMode && (installMode || installServerDir != null)) {
            System.err.println("megu launcher: --uninstall conflicts with install flags");
            return 2;
        }
        if (serverBin != null && installServerDir == null) {
            System.err.println("megu launcher: --server-bin requires --install-server");
            return 2;
        }
        if (installServerDir != null) {
            int code = runServerInstall(installServerDir, serverBin);
            if (code != 0 || !installMode) {
                return code;
            }
        }
        if (uninstallMode) {
            return runUninstall(installDir, mcVersion, !noProfile);
        }
        if (installMode) {
            return runSelfInstall(installDir, mcVersion, !noProfile, clientBin, clientSha256);
        }

        if (integrated && (detach || gameDir == null || gameDir.isBlank())) {
            System.err.println("megu launcher: --megu-integrated requires --gameDir and wait mode");
            return 2;
        }
        Path binary = resolveBinary(gameDir, clientBin);
        if (binary == null) {
            System.err.println("megu launcher: native client not found or not executable "
                    + "(--client-bin, MEGU_CLIENT_BIN, --gameDir/<name>, "
                    + "--gameDir/bin/<name>, PATH; Unix and Windows .exe names)");
            return 2;
        }

        List<String> command = new ArrayList<>();
        command.add(binary.toString());
        command.addAll(gameArgs);
        try {
            String expectedSha256 = clientSha256;
            if (expectedSha256 == null) {
                String profileSha256 = System.getProperty("megu.clientSha256");
                expectedSha256 = profileSha256 != null && !profileSha256.isBlank()
                        ? profileSha256 : System.getenv("MEGU_CLIENT_SHA256");
            }
            verifySha256(binary, expectedSha256);
            ensureExecutable(binary);
            String handoffPath = handoffFile ? handoffManifestPath(handoffManifest, gameDir) : null;
            if (integrated) {
                return runIntegratedParent(gameDir, command, launcherAccessToken, handoffFile, handoffPath);
            }
            String mode = detach ? "detach" : "wait";
            ProcessBuilder builder = new ProcessBuilder(command).inheritIO();
            builder.environment().put("MEGU_HANDOFF_JSON",
                    handoffJson(mode, -1, gameArgs.size(), null));
            if (launcherAccessToken != null && !launcherAccessToken.isEmpty()) {
                builder.environment().put("MEGU_LAUNCHER_ACCESS_TOKEN", launcherAccessToken);
            }
            Process process = builder.start();
            if (handoffPath != null) {
                writeHandoffManifest(Path.of(handoffPath), mode, process.pid(), gameArgs.size(), null);
            }
            if (detach) {
                return 0;
            }
            return process.waitFor();
        } catch (IOException error) {
            System.err.println("megu launcher: " + redact(error.getMessage(), gameArgs));
            return 2;
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            System.err.println("megu launcher: interrupted while waiting for native client");
            return 1;
        }
    }

    /** Self-installs this bootstrapper JAR and registers a profile in Official Launcher's launcher_profiles.json. */
    private static int runSelfInstall(String overrideDir, String mcVersion, boolean writeProfile,
            String clientBin, String clientSha256) {
        try {
            System.out.println(installClient(resolveMinecraftDir(overrideDir), mcVersion,
                    writeProfile, clientBin, clientSha256));
            return 0;
        } catch (IOException | InvalidPathException error) {
            System.err.println("megu launcher: install failed: " + error.getMessage());
            return 2;
        }
    }

    private static int runUninstall(String overrideDir, String mcVersion, boolean updateProfile) {
        try {
            System.out.println(uninstallClient(resolveMinecraftDir(overrideDir), mcVersion,
                    updateProfile));
            return 0;
        } catch (IOException | InvalidPathException error) {
            System.err.println("megu launcher: uninstall failed: " + error.getMessage());
            return 2;
        }
    }

    private static int runServerInstall(String directory, String serverBin) {
        try {
            System.out.println(installServer(Path.of(directory), serverBin));
            return 0;
        } catch (IOException | InvalidPathException error) {
            System.err.println("megu launcher: server install failed: " + error.getMessage());
            return 2;
        }
    }

    private static Path resolveMinecraftDir(String overrideDir) {
        return (overrideDir != null && !overrideDir.isBlank())
                ? Path.of(overrideDir).toAbsolutePath().normalize()
                : detectMinecraftDir();
    }

    /** Fabric/NeoForge-style client install: versions/<id>/<id>.json + libraries + launcher profile. */
    static String installClient(Path mcDir, String mcVersion, boolean writeProfile,
            String clientBin, String clientSha256) throws IOException {
        String versionId = versionId(mcVersion);
        Path selfJar = selfJarPath();
        if (selfJar == null || !Files.isRegularFile(selfJar)) {
            throw new IOException("cannot locate running bootstrapper jar");
        }
        Path pinnedBin = null;
        if (clientBin != null && !clientBin.isBlank()) {
            pinnedBin = Path.of(clientBin).toAbsolutePath().normalize();
            if (!Files.isRegularFile(pinnedBin)) {
                throw new IOException("--client-bin must be an existing file: " + clientBin);
            }
        } else if (selfJar.getParent() != null) {
            String gpuName = isWindows() ? "megu-client-gpu.exe" : "megu-client-gpu";
            String srvName = isWindows() ? "megu.exe" : "megu";
            Path siblingGpu = selfJar.getParent().resolve(gpuName);
            if (Files.isRegularFile(siblingGpu) && !Files.isSymbolicLink(siblingGpu)) {
                Path binDir = mcDir.resolve("bin");
                Files.createDirectories(binDir);
                Path stagedGpu = binDir.resolve(gpuName);
                refuseSymlink(stagedGpu);
                Files.copy(siblingGpu, stagedGpu, StandardCopyOption.REPLACE_EXISTING);
                makeExecutable(stagedGpu);
                pinnedBin = stagedGpu.toAbsolutePath().normalize();
                Path siblingSrv = selfJar.getParent().resolve(srvName);
                if (Files.isRegularFile(siblingSrv) && !Files.isSymbolicLink(siblingSrv)) {
                    Path stagedSrv = binDir.resolve(srvName);
                    refuseSymlink(stagedSrv);
                    Files.copy(siblingSrv, stagedSrv, StandardCopyOption.REPLACE_EXISTING);
                    makeExecutable(stagedSrv);
                }
            }
        }
        if (clientSha256 != null && !clientSha256.isBlank()) {
            if (!clientSha256.matches("(?i)[0-9a-f]{64}")) {
                throw new IOException("--client-sha256 must be 64 hexadecimal characters");
            }
            if (pinnedBin != null) {
                verifySha256(pinnedBin, clientSha256);
            }
        }
        Path libDest = mcDir.resolve(LIBRARY_REL);
        Files.createDirectories(libDest.getParent());
        if (!selfJar.toRealPath().equals(libDest.toAbsolutePath().normalize())) {
            Files.copy(selfJar, libDest, StandardCopyOption.REPLACE_EXISTING);
        }
        Path verDir = mcDir.resolve("versions").resolve(versionId);
        Files.createDirectories(verDir);
        atomicWriteString(verDir.resolve(versionId + ".json"),
                versionJson(versionId, mcVersion, pinnedBin, clientSha256));
        if (writeProfile) {
            updateProfilesFiles(mcDir, versionId, "Megu (" + mcVersion + ")", false);
        }
        return "Installed " + versionId + " into " + mcDir;
    }

    /**
     * Used by the interactive Swing GUI ("Install client", like neoforge-installer.jar):
     * if {@code <mcDir>/bin/megu-client-gpu(.exe)} is not yet staged and no sibling
     * binary sits next to the JAR, automatically download and SHA-256-verify the
     * matching OS release bundle from GitHub Releases into {@code <mcDir>/bin/}.
     */
    static String installClientFromGui(Path mcDir, String mcVersion, boolean writeProfile)
            throws IOException {
        String gpuName = isWindows() ? "megu-client-gpu.exe" : "megu-client-gpu";
        String srvName = isWindows() ? "megu.exe" : "megu";
        Path binDir = mcDir.resolve("bin");
        Path stagedGpu = binDir.resolve(gpuName);
        Path selfJar = selfJarPath();
        boolean hasSibling = selfJar != null && selfJar.getParent() != null
                && Files.isRegularFile(selfJar.getParent().resolve(gpuName));
        if (!hasSibling && !Files.isRegularFile(stagedGpu)
                && System.getenv("MEGU_NO_AUTO_FETCH") == null) {
            fetchReleaseBinariesInto(mcDir, binDir, gpuName, srvName);
        }
        String pin = Files.isRegularFile(stagedGpu)
                ? stagedGpu.toAbsolutePath().normalize().toString() : null;
        return installClient(mcDir, mcVersion, writeProfile, pin, null);
    }

    private static void fetchReleaseBinariesInto(Path mcDir, Path binDir,
            String gpuName, String srvName) throws IOException {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        String arch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
        String target;
        if (os.contains("win") && (arch.contains("64") || arch.contains("amd64"))) {
            target = "windows-x86_64";
        } else if (os.contains("mac") && (arch.contains("aarch64") || arch.contains("arm"))) {
            target = "macos-arm64";
        } else if (os.contains("linux") && (arch.contains("64") || arch.contains("amd64"))) {
            target = "linux-x86_64";
        } else {
            return;
        }
        String tag = System.getenv().getOrDefault("MEGU_RELEASE_TAG", "v0.1.0-alpha.1");
        String repo = System.getenv().getOrDefault("MEGU_GITHUB_REPO", "nekomario28/megu-mc");
        String archive = "megu-" + tag + "-" + target + ".tar.gz";
        String baseUrl = "https://github.com/" + repo + "/releases/download/" + tag;
        Path work = mcDir.resolve("megu-runtime");
        Files.createDirectories(work);
        Path sumsFile = work.resolve("SHA256SUMS-all.txt");
        Path tarFile = work.resolve(archive);
        try {
            if (!runCurl(baseUrl + "/SHA256SUMS-all.txt", sumsFile)
                    || !runCurl(baseUrl + "/" + archive, tarFile)) {
                return;
            }
            String expected = null;
            for (String line : Files.readAllLines(sumsFile, StandardCharsets.UTF_8)) {
                String[] parts = line.trim().split("\\s+", 2);
                if (parts.length == 2 && parts[1].replace("*", "").equals(archive)) {
                    expected = parts[0];
                    break;
                }
            }
            if (expected == null) {
                return;
            }
            verifySha256(tarFile, expected);
            Process tar = new ProcessBuilder("tar", "-xzf", tarFile.toString(), "-C", work.toString())
                    .start();
            if (tar.waitFor() != 0) {
                return;
            }
            Path extracted = work.resolve("megu-" + tag + "-" + target);
            Files.createDirectories(binDir);
            for (String bin : List.of(gpuName, srvName)) {
                Path src = extracted.resolve(bin);
                if (Files.isRegularFile(src) && !Files.isSymbolicLink(src)) {
                    Path dst = binDir.resolve(bin);
                    refuseSymlink(dst);
                    Files.copy(src, dst, StandardCopyOption.REPLACE_EXISTING);
                    makeExecutable(dst);
                }
            }
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
        } finally {
            Files.deleteIfExists(tarFile);
        }
    }

    private static boolean runCurl(String url, Path dest) throws IOException, InterruptedException {
        Process p = new ProcessBuilder("curl", "-fsSL", url, "-o", dest.toString()).start();
        return p.waitFor() == 0 && Files.isRegularFile(dest);
    }

    /** Removes our version directory and launcher profile entry, preserving all other profiles. */
    static String uninstallClient(Path mcDir, String mcVersion, boolean updateProfile)
            throws IOException {
        String versionId = versionId(mcVersion);
        Path verDir = mcDir.resolve("versions").resolve(versionId);
        if (Files.isSymbolicLink(verDir)) {
            Files.deleteIfExists(verDir);
        } else {
            deleteTree(verDir);
        }
        if (updateProfile) {
            updateProfilesFiles(mcDir, versionId, null, true);
        }
        return "Uninstalled " + versionId + " from " + mcDir;
    }

    /** NeoForge-style server install: stages run.sh / run.bat / README-SERVER.txt into {@code target}. */
    static String installServer(Path target, String serverBin) throws IOException {
        for (Path segment : target) {
            if ("..".equals(segment.toString())) {
                throw new IOException("refusing path traversal in --install-server: " + target);
            }
        }
        Path dir = target.toAbsolutePath().normalize();
        Files.createDirectories(dir);
        if (Files.isSymbolicLink(dir)) {
            throw new IOException("refusing symlinked --install-server directory: " + dir);
        }
        if (serverBin != null && !serverBin.isBlank()) {
            Path binary = Path.of(serverBin).toAbsolutePath().normalize();
            if (!Files.isRegularFile(binary)) {
                throw new IOException("--server-bin must be an existing file: " + serverBin);
            }
            boolean exe = binary.getFileName().toString()
                    .toLowerCase(Locale.ROOT).endsWith(".exe");
            Path staged = dir.resolve(exe ? "megu.exe" : "megu");
            refuseSymlink(staged);
            Files.copy(binary, staged, StandardCopyOption.REPLACE_EXISTING);
            makeExecutable(staged);
        }
        Path runSh = dir.resolve("run.sh");
        Path runBat = dir.resolve("run.bat");
        Path readme = dir.resolve("README-SERVER.txt");
        refuseSymlink(runSh);
        refuseSymlink(runBat);
        refuseSymlink(readme);
        atomicWriteString(runSh, RUN_SH);
        makeExecutable(runSh);
        atomicWriteString(runBat, RUN_BAT);
        atomicWriteString(readme, SERVER_README);
        return "Installed megu server launcher into " + dir;
    }

    private static String versionId(String mcVersion) throws IOException {
        if (!"1.21.1".equals(mcVersion) && !"26.3".equals(mcVersion)) {
            throw new IOException("unsupported --mc-version: " + mcVersion);
        }
        return "megu-" + mcVersion;
    }

    private static String isoNow() {
        return ISO_UTC.format(java.time.Instant.now());
    }

    private static String versionJson(String versionId, String mcVersion,
            Path pinnedBin, String clientSha256) {
        String now = isoNow();
        List<String> jvm = new ArrayList<>();
        if (pinnedBin != null) {
            jvm.add("-Dmegu.clientBin=" + pinnedBin);
        }
        if (clientSha256 != null && !clientSha256.isBlank()) {
            jvm.add("-Dmegu.clientSha256=" + clientSha256.toLowerCase(Locale.ROOT));
        }
        StringBuilder json = new StringBuilder("{\n")
                .append("  \"id\": \"").append(escapeJson(versionId)).append("\",\n")
                .append("  \"inheritsFrom\": \"").append(escapeJson(mcVersion)).append("\",\n")
                .append("  \"type\": \"release\",\n")
                .append("  \"mainClass\": \"net.megu.launcher.Main\",\n")
                .append("  \"releaseTime\": \"").append(now).append("\",\n")
                .append("  \"time\": \"").append(now).append("\",\n");
        if (!jvm.isEmpty()) {
            json.append("  \"arguments\": {\n    \"jvm\": [\n");
            for (int i = 0; i < jvm.size(); i++) {
                json.append("      \"").append(escapeJson(jvm.get(i))).append('"')
                        .append(i + 1 < jvm.size() ? ",\n" : "\n");
            }
            json.append("    ]\n  },\n");
        }
        return json.append("  \"libraries\": [\n    {\"name\": \"").append(LIBRARY_COORD)
                .append("\"}\n  ]\n}\n").toString();
    }

    static Path detectMinecraftDir() {
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
        Path standard = home.resolve(".minecraft");
        if (!Files.isDirectory(standard)) {
            Path flatpak = home.resolve(".var/app/com.mojang.Minecraft/.minecraft");
            if (Files.isDirectory(flatpak)) {
                return flatpak;
            }
        }
        return standard;
    }

    private static void refuseSymlink(Path file) throws IOException {
        if (Files.isSymbolicLink(file)) {
            throw new IOException("refusing to overwrite symlink: " + file);
        }
    }

    private static void deleteTree(Path root) throws IOException {
        if (Files.isDirectory(root, LinkOption.NOFOLLOW_LINKS)) {
            try (var entries = Files.newDirectoryStream(root)) {
                for (Path child : entries) {
                    deleteTree(child);
                }
            }
        }
        Files.deleteIfExists(root);
    }

    private static void makeExecutable(Path file) {
        try {
            Files.setPosixFilePermissions(file, PosixFilePermissions.fromString("rwxr-xr-x"));
        } catch (UnsupportedOperationException | IOException error) {
            file.toFile().setExecutable(true, false);
        }
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

    /** Edits launcher_profiles.json and launcher_profiles_microsoft_store.json when present. */
    private static void updateProfilesFiles(Path mcDir, String versionId, String displayName,
            boolean uninstall) throws IOException {
        Path standard = mcDir.resolve("launcher_profiles.json");
        Path store = mcDir.resolve("launcher_profiles_microsoft_store.json");
        if (uninstall || Files.exists(standard) || Files.exists(store)) {
            if (Files.exists(standard)) {
                editProfilesFile(standard, versionId, displayName, uninstall);
            }
            if (Files.exists(store)) {
                editProfilesFile(store, versionId, displayName, uninstall);
            }
            return;
        }
        editProfilesFile(standard, versionId, displayName, false);
    }

    /** Surgically adds/updates/removes our profile entry, preserving every other user profile. */
    private static void editProfilesFile(Path file, String versionId, String displayName,
            boolean uninstall) throws IOException {
        String now = isoNow();
        String text = Files.isRegularFile(file)
                ? Files.readString(file, StandardCharsets.UTF_8) : null;
        int root = text == null ? 0 : skipJsonWhitespace(text, 0);
        if (text == null || root >= text.length() || text.charAt(root) != '{') {
            if (uninstall) {
                return;
            }
            backupProfiles(file, text);
            atomicWriteString(file, freshProfilesJson(versionId, displayName, now));
            return;
        }
        int[] profiles = jsonMember(text, root, "profiles");
        String updated;
        if (profiles == null) {
            if (uninstall) {
                return;
            }
            String member = "\"profiles\": {\n    "
                    + profileMember(versionId, displayName, now) + "\n  }";
            updated = insertJsonMember(text, root, member);
        } else {
            int objStart = skipJsonWhitespace(text, profiles[1]);
            if (objStart >= text.length() || text.charAt(objStart) != '{') {
                if (uninstall) {
                    return;
                }
                updated = text.substring(0, objStart) + "{\n    "
                        + profileMember(versionId, displayName, now) + "\n  }"
                        + text.substring(profiles[2]);
            } else {
                int[] member = jsonMember(text, objStart, versionId);
                if (uninstall) {
                    if (member == null) {
                        return;
                    }
                    updated = removeJsonMember(text, objStart, member);
                } else if (member == null) {
                    updated = insertJsonMember(text, objStart,
                            profileMember(versionId, displayName, now));
                } else {
                    int valueStart = skipJsonWhitespace(text, member[1]);
                    if (valueStart >= text.length() || text.charAt(valueStart) != '{') {
                        updated = text.substring(0, valueStart)
                                + profileObject(versionId, displayName, now)
                                + text.substring(member[2]);
                    } else {
                        String obj = text.substring(valueStart, member[2]);
                        obj = upsertProfileField(obj, "name", displayName, false);
                        obj = upsertProfileField(obj, "type", "custom", false);
                        obj = upsertProfileField(obj, "created", now, false);
                        obj = upsertProfileField(obj, "lastUsed", now, true);
                        obj = upsertProfileField(obj, "lastVersionId", versionId, true);
                        obj = upsertProfileField(obj, "icon", "Furnace", false);
                        updated = text.substring(0, valueStart) + obj + text.substring(member[2]);
                    }
                }
            }
        }
        if (updated.equals(text)) {
            return;
        }
        backupProfiles(file, text);
        atomicWriteString(file, updated);
    }

    /** Writes <file>.bak holding the pre-edit content, only when missing or different. */
    private static void backupProfiles(Path file, String current) throws IOException {
        if (current == null) {
            return;
        }
        Path backup = file.resolveSibling(file.getFileName() + ".bak");
        if (Files.isRegularFile(backup)
                && Files.readString(backup, StandardCharsets.UTF_8).equals(current)) {
            return;
        }
        atomicWriteString(backup, current);
    }

    private static String profileMember(String versionId, String displayName, String now) {
        return "\"" + escapeJson(versionId) + "\": " + profileObject(versionId, displayName, now);
    }

    private static String profileObject(String versionId, String displayName, String now) {
        return "{\n      \"name\": \"" + escapeJson(displayName) + "\",\n"
                + "      \"type\": \"custom\",\n"
                + "      \"created\": \"" + now + "\",\n"
                + "      \"lastUsed\": \"" + now + "\",\n"
                + "      \"lastVersionId\": \"" + escapeJson(versionId) + "\",\n"
                + "      \"icon\": \"Furnace\"\n    }";
    }

    private static String freshProfilesJson(String versionId, String displayName, String now) {
        return "{\n  \"profiles\": {\n    " + profileMember(versionId, displayName, now)
                + "\n  },\n  \"version\": 3\n}\n";
    }

    /** Index just past the JSON string literal starting at {@code i} (which must be '"'). */
    private static int skipJsonString(String text, int i) {
        int n = text.length();
        i++;
        while (i < n) {
            char c = text.charAt(i);
            if (c == '\\') {
                i += 2;
            } else if (c == '"') {
                return i + 1;
            } else {
                i++;
            }
        }
        return n;
    }

    private static int skipJsonWhitespace(String text, int i) {
        while (i < text.length() && Character.isWhitespace(text.charAt(i))) {
            i++;
        }
        return i;
    }

    /** Index just past the JSON value starting at {@code i}; string/object/array aware. */
    private static int skipJsonValue(String text, int i) {
        int n = text.length();
        i = skipJsonWhitespace(text, i);
        if (i >= n) {
            return n;
        }
        char c = text.charAt(i);
        if (c == '"') {
            return skipJsonString(text, i);
        }
        if (c == '{' || c == '[') {
            int depth = 0;
            while (i < n) {
                c = text.charAt(i);
                if (c == '"') {
                    i = skipJsonString(text, i);
                    continue;
                }
                if (c == '{' || c == '[') {
                    depth++;
                } else if ((c == '}' || c == ']') && --depth == 0) {
                    return i + 1;
                }
                i++;
            }
            return n;
        }
        while (i < n && !Character.isWhitespace(text.charAt(i)) && text.charAt(i) != ','
                && text.charAt(i) != '}' && text.charAt(i) != ']') {
            i++;
        }
        return i;
    }

    /** Locates the depth-1 member {@code key} of the object at {@code objStart};
        returns {keyStart, valueStart, valueEnd} or null. */
    private static int[] jsonMember(String text, int objStart, String key) {
        int n = text.length();
        int i = objStart + 1;
        String wanted = '"' + key + '"';
        while (true) {
            i = skipJsonWhitespace(text, i);
            if (i >= n || text.charAt(i) != '"') {
                return null;
            }
            int keyStart = i;
            i = skipJsonString(text, i);
            String name = text.substring(keyStart, i);
            i = skipJsonWhitespace(text, i);
            if (i >= n || text.charAt(i) != ':') {
                return null;
            }
            i = skipJsonWhitespace(text, i + 1);
            int valueStart = i;
            int valueEnd = skipJsonValue(text, i);
            if (name.equals(wanted)) {
                return new int[]{keyStart, valueStart, valueEnd};
            }
            i = skipJsonWhitespace(text, valueEnd);
            if (i < n && text.charAt(i) == ',') {
                i++;
            } else {
                return null;
            }
        }
    }

    /** Inserts {@code member} ("key": value text) into the object starting at {@code objStart}. */
    private static String insertJsonMember(String text, int objStart, String member) {
        int close = skipJsonValue(text, objStart) - 1;
        int j = close - 1;
        while (j > objStart && Character.isWhitespace(text.charAt(j))) {
            j--;
        }
        if (j == objStart) {
            return text.substring(0, objStart + 1) + "\n    " + member + "\n  "
                    + text.substring(close);
        }
        String comma = text.charAt(j) == ',' ? "" : ",";
        return text.substring(0, j + 1) + comma + "\n    " + member + text.substring(j + 1);
    }

    /** Removes the member described by {@code member} plus one adjacent comma. */
    private static String removeJsonMember(String text, int objStart, int[] member) {
        int start = member[0];
        int end = member[2];
        int i = skipJsonWhitespace(text, end);
        if (i < text.length() && text.charAt(i) == ',') {
            end = i + 1;
        } else {
            int j = start - 1;
            while (j > objStart && Character.isWhitespace(text.charAt(j))) {
                j--;
            }
            if (j > objStart && text.charAt(j) == ',') {
                start = j;
            }
        }
        return text.substring(0, start) + text.substring(end);
    }

    /** Sets a string field inside the {@code obj} JSON object text, inserting it when absent. */
    private static String upsertProfileField(String obj, String key, String value,
            boolean overwrite) {
        int[] member = jsonMember(obj, 0, key);
        String quoted = '"' + escapeJson(value) + '"';
        if (member != null) {
            return overwrite
                    ? obj.substring(0, member[1]) + quoted + obj.substring(member[2])
                    : obj;
        }
        int j = obj.length() - 2;
        while (j > 0 && Character.isWhitespace(obj.charAt(j))) {
            j--;
        }
        String field = '"' + key + "\": " + quoted;
        if (j == 0) {
            return "{\n      " + field + "\n    }";
        }
        String comma = obj.charAt(j) == ',' ? "" : ",";
        return obj.substring(0, j + 1) + comma + "\n      " + field + obj.substring(j + 1);
    }

    private static String escapeJson(String value) {
        StringBuilder out = new StringBuilder(value.length() + 8);
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            switch (c) {
                case '"' -> out.append("\\\"");
                case '\\' -> out.append("\\\\");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default -> {
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
                }
            }
        }
        return out.toString();
    }

    private static void atomicWriteString(Path destination, String content) throws IOException {
        Path parent = destination.toAbsolutePath().getParent();
        Files.createDirectories(parent);
        Path temp = Files.createTempFile(parent, ".megu-", ".tmp");
        try {
            Files.writeString(temp, content, StandardCharsets.UTF_8);
            Files.move(temp, destination, StandardCopyOption.REPLACE_EXISTING);
        } finally {
            Files.deleteIfExists(temp);
        }
    }

    /** Pure-Java parent wrapper: spawns a tiny Java child watching stdin EOF so even SIGKILL stops the server cleanly. */
    private static int runIntegratedParent(String gameDir, List<String> client, String accessToken,
            boolean handoffFile, String handoffPath)
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
        cmd.add("--megu-integrated-watch");
        cmd.add(gameDir);
        cmd.add("--");
        cmd.addAll(client);
        ProcessBuilder builder = new ProcessBuilder(cmd)
                .redirectOutput(ProcessBuilder.Redirect.INHERIT)
                .redirectError(ProcessBuilder.Redirect.INHERIT);
        if (accessToken != null && !accessToken.isEmpty()) {
            builder.environment().put("MEGU_LAUNCHER_ACCESS_TOKEN", accessToken);
        }
        if (handoffFile && handoffPath != null) {
            builder.environment().put("MEGU_HANDOFF_FILE", "1");
            builder.environment().put("MEGU_HANDOFF_MANIFEST", handoffPath);
        }
        Process bridge = builder.start();
        try (var parentPipe = bridge.getOutputStream()) {
            parentPipe.flush();
            return bridge.waitFor();
        }
    }

    /** Pure-Java integrated server & client coordinator (zero python3 dependency on Windows/macOS/Linux). */
    private static int runIntegratedWatch(String[] args) {
        if (args.length < 4 || !"--".equals(args[2])) {
            System.err.println("megu integrated: malformed internal watch arguments");
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
        }, "megu-parent-eof");
        parentWatch.setDaemon(true);
        parentWatch.start();

        Process server = null;
        Process clientProc = null;
        try {
            Path serverBin = resolveServerBinary(gameDir);
            if (serverBin == null) {
                throw new IOException("megu server not found or not executable (MEGU_SERVER_BIN, --gameDir, PATH)");
            }
            ensureExecutable(serverBin);
            String worldName = System.getenv("MEGU_INTEGRATED_WORLD");
            if (worldName == null || worldName.isBlank()) {
                worldName = "megu";
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
            env.put("MEGU_BIND_ADDR", LOOPBACK);
            env.put("MEGU_PORT", Integer.toString(port));
            env.put("MEGU_WORLD_DIR", world.toString());
            env.put("MEGU_RESOURCE_PACK_URL", "");
            env.put("MEGU_MAX_CONN_PER_IP", "1");
            env.remove("MEGU_RCON_PORT");
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
                        if (line.startsWith("megu ") && line.contains(banner)) {
                            ready.countDown();
                        }
                        System.err.println(line);
                    }
                } catch (IOException ignored) {
                    // Server closed output.
                }
            }, "megu-server-log");
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
            cb.environment().put("MEGU_HANDOFF_JSON",
                    handoffJson("integrated", -1, clientCmd.size() - 1, endpoint));
            clientProc = cb.start();
            String handoffPath = handoffManifestPath(null, gameDir.toString());
            if ("1".equals(System.getenv("MEGU_HANDOFF_FILE")) && handoffPath != null) {
                writeHandoffManifest(Path.of(handoffPath), "integrated", clientProc.pid(),
                        clientCmd.size() - 1, endpoint);
            }
            while (clientProc.isAlive() && !stopping.get()) {
                clientProc.waitFor(50, TimeUnit.MILLISECONDS);
            }
            if (stopping.get()) {
                stopProcess(clientProc);
                return 1;
            }
            return clientProc.exitValue();
        } catch (IOException error) {
            System.err.println("megu integrated: " + redact(error.getMessage(), client));
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
        String configured = System.getenv("MEGU_SERVER_HOME");
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
            throw new IOException("MEGU_SERVER_HOME must contain config_packets.bin and assets");
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
        String configured = System.getenv("MEGU_SERVER_BIN");
        if (configured != null && !configured.isBlank()) {
            return regularFile(configured);
        }
        List<String> names = isWindows()
                ? List.of("megu.exe", "megu")
                : List.of("megu", "megu.exe");
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
        String[] unix = {"megu-client-gpu", "megu-client", "megu", "megu"};
        String[] windows = {"megu-client-gpu.exe", "megu-client.exe",
                            "megu.exe", "megu.exe"};
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
        String configured = System.getenv("MEGU_CLIENT_BIN");
        if (configured != null && !configured.isBlank()) {
            return regularFile(configured);
        }
        String property = System.getProperty("megu.clientBin");
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

    private static String handoffJson(String mode, long pid, int argumentCount, String endpoint) {
        StringBuilder json = new StringBuilder();
        json.append("{\"mode\":\"").append(mode).append('"');
        if (pid > 0) {
            json.append(",\"pid\":").append(pid);
        }
        json.append(",\"argumentCount\":").append(argumentCount);
        if (endpoint != null) {
            json.append(",\"endpoint\":\"").append(endpoint).append('"');
        }
        return json.append("}\n").toString();
    }

    private static String handoffManifestPath(String override, String gameDir) {
        if (override != null && !override.isBlank()) {
            return override;
        }
        String configured = System.getenv("MEGU_HANDOFF_MANIFEST");
        if (configured != null && !configured.isBlank()) {
            return configured;
        }
        if (gameDir != null && !gameDir.isBlank()) {
            return Path.of(gameDir).resolve(".megu-handoff.json").toString();
        }
        return null;
    }

    private static void writeHandoffManifest(Path destination, String mode, long pid,
            int argumentCount, String endpoint) throws IOException {
        atomicWriteString(destination.toAbsolutePath(),
                handoffJson(mode, pid, argumentCount, endpoint));
    }
}
