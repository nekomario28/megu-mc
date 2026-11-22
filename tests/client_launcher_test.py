"""Real Java 21/bootstrapper, Prism, Modrinth and native bridge checks."""
import configparser
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
import tomllib

from wire_smoke import status


ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / "tools/client_launcher"


class LauncherTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-client-launcher-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.jar_dir = cls.root / "profile-one"
        cls.stub = cls.root / "native-client"
        cls.stub.write_text("""#!/usr/bin/env python3
import json
import os
import socket
import sys
import time
from pathlib import Path
if os.environ.get('TEST_INTEGRATED_CONNECT'):
    endpoint = sys.argv[sys.argv.index('--quickPlayMultiplayer') + 1]
    host, port = endpoint.split(':')
    with socket.create_connection((host, int(port)), timeout=1):
        pass
    if os.environ.get('TEST_INTEGRATED_PROTOCOL'):
        sys.path.insert(0, os.environ['TEST_WIRE_MODULE'])
        from wire_smoke import status
        status(int(port))
Path(os.environ['TEST_NATIVE_LOG']).write_text(json.dumps(sys.argv[1:]))
if os.environ.get('TEST_NATIVE_ENV_LOG'):
    Path(os.environ['TEST_NATIVE_ENV_LOG']).write_text(json.dumps({
        'MC_SPINE_LAUNCHER_ACCESS_TOKEN': os.environ.get('MC_SPINE_LAUNCHER_ACCESS_TOKEN'),
        'MC_SPINE_TOKEN_ENV': os.environ.get('MC_SPINE_TOKEN_ENV'),
        'MC_SPINE_HANDOFF_JSON': os.environ.get('MC_SPINE_HANDOFF_JSON'),
    }))
time.sleep(float(os.environ.get('TEST_NATIVE_SLEEP', '0')))
sys.exit(int(os.environ.get('TEST_NATIVE_EXIT', '0')))
""")
        cls.stub.chmod(0o755)
        cls.server = cls.root / "native-server"
        cls.server.write_text("""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import signal
import socket
import sys
import time

def stop(*_):
    Path('stopped').write_text(str(os.getpid()))
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
host = os.environ['MC_SPINE_BIND_ADDR']
port = int(os.environ['MC_SPINE_PORT'])
with socket.socket() as listener:
    time.sleep(0.2)
    listener.bind((host, port))
    listener.listen()
    Path('server.json').write_text(json.dumps({
        'pid': os.getpid(), 'port': port, 'bind': host, 'cwd': os.getcwd(),
        'world': os.environ['MC_SPINE_WORLD_DIR'],
        'rcon': os.environ.get('MC_SPINE_RCON_PORT'),
    }))
    print(f'mc-spine stub listening {host}:{port} (test)', flush=True)
    while True:
        conn, _ = listener.accept()
        conn.close()
""")
        cls.server.chmod(0o755)
        cls.generate(cls.jar_dir, "--build-jar")

    @classmethod
    def generate(cls, destination, *extra):
        result = subprocess.run(
            [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
             "--out-dir", str(destination), *extra],
            capture_output=True, text=True, check=True)
        return result.stdout

    def invocation(self, *args, extra_env=None):
        log = self.root / (self.id().replace(".", "_") + ".json")
        log.unlink(missing_ok=True)
        env = dict(os.environ, MC_SPINE_CLIENT_BIN=str(self.stub), TEST_NATIVE_LOG=str(log))
        env.pop("MC_SPINE_DETACH", None)
        env.pop("MC_SPINE_HANDOFF_MANIFEST", None)
        env.pop("MC_SPINE_HANDOFF_FILE", None)
        if extra_env:
            env.update(extra_env)
        # In detach mode the native child inherits stderr, so capturing that
        # pipe would make subprocess.run wait for the child after Java exits.
        stderr = subprocess.DEVNULL if "--mc-spine-detach" in args else subprocess.PIPE
        result = subprocess.run(args, env=env, stdout=subprocess.DEVNULL,
                                stderr=stderr, text=True, timeout=20)
        return result, log

    def java(self, *args, extra_env=None):
        return self.invocation("java", "-Djava.io.tmpdir=" + tempfile.gettempdir(),
                               "-XX:-UsePerfData",
                               "-jar", str(self.jar_dir / "mc-spine-bootstrapper.jar"),
                               *args, extra_env=extra_env)

    def integrated_fixture(self):
        game = self.root / self._testMethodName / "game with spaces"
        env = {"MC_SPINE_SERVER_BIN": str(self.server), "MC_SPINE_RCON_PORT": "25575",
               "MC_SPINE_INTEGRATED_WORLD": "Loopback World", "TEST_INTEGRATED_CONNECT": "1"}
        return game, game / "saves" / env["MC_SPINE_INTEGRATED_WORLD"], env

    def assert_server_stopped(self, world):
        info = json.loads((world / "server.json").read_text())
        self.assertEqual(info["world"], str(world))
        self.assertEqual(info["cwd"], str(world))
        self.assertEqual(info["bind"], "127.0.0.1")
        self.assertIsNone(info["rcon"])
        self.assertGreater(info["port"], 0)
        self.assertEqual((world / "stopped").read_text(), str(info["pid"]))
        with self.assertRaises(ProcessLookupError):
            os.kill(info["pid"], 0)
        return f"127.0.0.1:{info['port']}"

    def test_jar_reproducible_and_profile_installable(self):
        other = self.root / "profile-two"
        self.generate(other, "--build-jar", "--client-bin", str(self.stub))
        first = self.jar_dir / "mc-spine-bootstrapper.jar"
        second = other / "mc-spine-bootstrapper.jar"
        self.assertLess(first.stat().st_size, 32 * 1024)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        version = other / "versions/mc-spine-1.21.1/mc-spine-1.21.1.json"
        profile = json.loads(version.read_text())
        self.assertEqual(profile["mainClass"], "net.mcspine.launcher.Main")
        self.assertEqual(profile["inheritsFrom"], "1.21.1")
        self.assertEqual(profile["id"], "mc-spine-1.21.1")
        self.assertEqual(profile["libraries"], [{"name": "net.mcspine:bootstrapper:1.0.0"}])
        library = other / "libraries/net/mcspine/bootstrapper/1.0.0/bootstrapper-1.0.0.jar"
        self.assertEqual(library.read_bytes(), second.read_bytes())
        self.assertEqual(profile["arguments"]["jvm"],
                         [f"-Dmc.spine.clientBin={self.stub.resolve()}"])

    def test_game_arguments_preserved_verbatim_and_exit_code(self):
        args = ("--username", "Steve", "--uuid", "00000000-0000-0000-0000-000000000001",
                "--accessToken", "private-secret", "--userType", "msa", "--version", "1.21.1",
                "--gameDir", "/folder with spaces", "--assetsDir", "/other/assets",
                "--assetIndex", "17", "--width", "1920", "--height", "1080",
                "--quickPlayMultiplayer", "test.example:25565", "--server", "localhost",
                "--port", "25565", "--unknownFlag", "value with spaces")
        manifest = self.root / "handoff.json"
        env_log = self.root / "handoff-env.json"
        result, log = self.java(*args, extra_env={"MC_SPINE_HANDOFF_MANIFEST": str(manifest),
                                                  "TEST_NATIVE_ENV_LOG": str(env_log),
                                                  "TEST_NATIVE_EXIT": "37"})
        self.assertEqual(result.returncode, 37, result.stderr)
        self.assertEqual(json.loads(log.read_text()), list(args))
        self.assertFalse(manifest.exists())
        handoff = json.loads(json.loads(env_log.read_text())["MC_SPINE_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "wait")
        self.assertEqual(handoff["argumentCount"], len(args))
        self.assertNotIn("private-secret", json.dumps(handoff))

    def test_handoff_manifest_file_requires_explicit_legacy_flag(self):
        args = ("--username", "Steve", "--port", "25565")
        manifest = self.root / "legacy-handoff.json"
        result, _ = self.java(*args, extra_env={"MC_SPINE_HANDOFF_MANIFEST": str(manifest)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(manifest.exists())
        result, _ = self.java("--mc-spine-handoff-file", *args,
                              extra_env={"MC_SPINE_HANDOFF_MANIFEST": str(manifest)})
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(manifest.read_text())
        self.assertEqual(summary["mode"], "wait")
        self.assertEqual(summary["argumentCount"], len(args))
        self.assertGreater(summary["pid"], 0)
        manifest.unlink()
        cli_manifest = self.root / "cli-handoff.json"
        result, _ = self.java(f"--mc-spine-handoff-manifest={cli_manifest}", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(cli_manifest.read_text())["mode"], "wait")

    def test_launch_leaves_zero_handoff_temp_files(self):
        game = self.root / "handoff game dir"
        game.mkdir()
        env_log = self.root / "handoff-zero-env.json"
        watched = [game, Path(tempfile.gettempdir())]
        before = {d: set(d.glob(".mc-spine-handoff-*")) for d in watched}
        result, log = self.java("--username", "Steve", "--gameDir", str(game),
                                "--accessToken", "private-secret",
                                extra_env={"TEST_NATIVE_ENV_LOG": str(env_log),
                                           "MC_SPINE_HANDOFF_MANIFEST": str(game / "handoff.json"),
                                           "MC_SPINE_HANDOFF_FILE": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()),
                         ["--username", "Steve", "--gameDir", str(game),
                          "--accessToken", "private-secret"])
        for directory in watched:
            self.assertEqual(before[directory], set(directory.glob(".mc-spine-handoff-*")),
                             f"unexpected .mc-spine-handoff-* files in {directory}")
        self.assertFalse((game / "handoff.json").exists())
        handoff = json.loads(json.loads(env_log.read_text())["MC_SPINE_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "wait")
        self.assertEqual(handoff["argumentCount"], 6)
        self.assertNotIn("private-secret", json.dumps(handoff))

    def test_detach_does_not_wait_and_removes_only_private_flag(self):
        started = time.monotonic()
        result, log = self.java("--username", "Steve", "--mc-spine-detach", "--port", "25565",
                                extra_env={"TEST_NATIVE_SLEEP": "2.0"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 1.5)
        for _ in range(100):
            if log.is_file():
                break
            time.sleep(0.01)
        self.assertEqual(json.loads(log.read_text()), ["--username", "Steve", "--port", "25565"])

    def test_detach_environment(self):
        result, log = self.java("--uuid", "xyz", extra_env={"MC_SPINE_DETACH": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--uuid", "xyz"])

    def test_game_directory_binary_resolution(self):
        folder = self.root / "game with spaces" / "bin"
        folder.mkdir(parents=True)
        import shutil
        shutil.copyfile(self.stub, folder / "mc-spine-client")
        (folder / "mc-spine-client").chmod(0o755)
        result, log = self.java("--gameDir", str(folder.parent), "--username", "Alex",
                                extra_env={"MC_SPINE_CLIENT_BIN": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--gameDir", str(folder.parent),
                                                      "--username", "Alex"])

    def test_missing_native_binary_has_exit_code_two(self):
        result, _ = self.java("--username", "Steve",
                              extra_env={"MC_SPINE_CLIENT_BIN": str(self.root / "absent")})
        self.assertEqual(result.returncode, 2)
        self.assertIn("not found", result.stderr)

    def test_integrated_injects_loopback_and_stops_on_client_exit(self):
        game, world, env = self.integrated_fixture()
        manifest = game / "handoff.json"
        env_log = game.parent / "handoff-env.json"
        env.update(MC_SPINE_HANDOFF_MANIFEST=str(manifest), TEST_NATIVE_EXIT="37",
                   TEST_NATIVE_ENV_LOG=str(env_log))
        result, log = self.java("--mc-spine-handoff-file", "--mc-spine-integrated",
                                "--gameDir", str(game),
                                "--quickPlaySingleplayer=stale", "--quickPlayMultiplayer",
                                "example.invalid:25565", "--accessToken", "private-secret",
                                extra_env=env)
        self.assertEqual(result.returncode, 37, result.stderr)
        endpoint = self.assert_server_stopped(world)
        self.assertEqual(json.loads(log.read_text()),
                         ["--gameDir", str(game), "--accessToken", "private-secret",
                          "--quickPlayMultiplayer", endpoint])
        summary = json.loads(manifest.read_text())
        self.assertEqual(summary["endpoint"], endpoint)
        self.assertEqual(summary["mode"], "integrated")
        self.assertNotIn("private-secret", manifest.read_text())
        handoff = json.loads(json.loads(env_log.read_text())["MC_SPINE_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "integrated")
        self.assertEqual(handoff["endpoint"], endpoint)
        self.assertEqual(handoff["argumentCount"], 6)
        self.assertNotIn("private-secret", json.dumps(handoff))

    def test_integrated_stops_when_client_spawn_fails(self):
        game, world, env = self.integrated_fixture()
        broken = game.parent / "broken-client"
        game.parent.mkdir()
        broken.write_text("#!/nonexistent/interpreter\n")
        broken.chmod(0o755)
        env["MC_SPINE_CLIENT_BIN"] = str(broken)
        result, log = self.java("--mc-spine-integrated", f"--gameDir={game}", extra_env=env)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("broken-client", result.stderr)
        self.assertFalse(log.exists())
        self.assert_server_stopped(world)

    def test_integrated_cli_accepts_a_client_command(self):
        game, world, env = self.integrated_fixture()
        env_log = game.parent / "bridge-handoff-env.json"
        env["TEST_NATIVE_ENV_LOG"] = str(env_log)
        result, log = self.invocation(sys.executable, str(TOOLS / "integrated_bridge.py"),
                                      "--game-dir", str(game), "--world", world.name,
                                      "--", str(self.stub), "--username", "Alex", extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        endpoint = self.assert_server_stopped(world)
        self.assertEqual(json.loads(log.read_text()),
                         ["--username", "Alex", "--quickPlayMultiplayer", endpoint])
        handoff = json.loads(json.loads(env_log.read_text())["MC_SPINE_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "integrated")
        self.assertEqual(handoff["endpoint"], endpoint)
        self.assertEqual(handoff["argumentCount"], 4)

    def test_integrated_stops_when_java_wrapper_exits_abruptly(self):
        game, world, env = self.integrated_fixture()
        client_log = game.parent / "client.json"
        env = dict(os.environ, **env, MC_SPINE_CLIENT_BIN=str(self.stub),
                   TEST_NATIVE_LOG=str(client_log), TEST_NATIVE_SLEEP="30", MC_SPINE_DETACH="0")
        env.pop("MC_SPINE_HANDOFF_MANIFEST", None)
        command = ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData", "-jar",
                   str(self.jar_dir / "mc-spine-bootstrapper.jar"),
                   "--mc-spine-integrated", "--gameDir", str(game)]
        with subprocess.Popen(command, env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL) as java:
            try:
                deadline = time.monotonic() + 10
                while not client_log.is_file() and java.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(client_log.is_file())
            finally:
                java.kill()
                java.wait(timeout=5)

        info = json.loads((world / "server.json").read_text())
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            try:
                os.kill(info["pid"], 0)
            except ProcessLookupError:
                break
            time.sleep(0.02)
        self.assert_server_stopped(world)

    def test_wrapper_removes_jvm_arguments_and_execs_native(self):
        wrapper = str(TOOLS / "mc-spine-wrapper")
        args = ("--username", "Steve", "--uuid", "0-1-2", "--unknown", "as-is")
        result, log = self.invocation(wrapper, "/usr/bin/java", "-Xmx4G", "-Dfoo=bar",
                                      "-cp", "a.jar:b.jar", "--add-opens",
                                      "java.base/java.lang=ALL-UNNAMED", "-XX:+UseG1GC",
                                      "net.minecraft.client.main.Main", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), list(args))

    def test_wrapper_jar_mode_and_native_exit_code(self):
        result, log = self.invocation(str(TOOLS / "mc-spine-wrapper"), "java", "-Xms1G",
                                      "-jar", "minecraft.jar", "--username", "Alex",
                                      extra_env={"TEST_NATIVE_EXIT": "9"})
        self.assertEqual(result.returncode, 9, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--username", "Alex"])

    def test_java_integrated_real_server_and_native_protocol_handshake(self):
        server = ROOT / "target/debug/mc-spine"
        if not server.is_file():
            self.skipTest("cargo build --offline --locked required for real protocol handshake")
        game = self.root / "cf4-real-integrated" / "game"
        game.mkdir(parents=True)
        env = {"MC_SPINE_SERVER_BIN": str(server), "MC_SPINE_SERVER_HOME": str(ROOT),
               "TEST_INTEGRATED_CONNECT": "1", "TEST_INTEGRATED_PROTOCOL": "1",
               "TEST_WIRE_MODULE": str(ROOT / "tests")}
        result, log = self.java("--mc-spine-integrated", "--gameDir", str(game),
                                "--username", "Steve", "--accessToken", "super-secret-token",
                                "--version", "1.21.1", "--assetIndex", "17", extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("super-secret-token", result.stderr or "")
        args = json.loads(log.read_text())
        self.assertIn("--quickPlayMultiplayer", args)
        self.assertEqual(args[args.index("--accessToken") + 1], "super-secret-token")
        world = game / "saves/mc-spine"
        self.assertTrue((world / "config_packets.bin").is_symlink())
        self.assertTrue((world / "assets").is_symlink())

    def test_java_private_binary_flags_integrity_and_redaction(self):
        expected = hashlib.sha256(self.stub.read_bytes()).hexdigest()
        result, log = self.java("--client-bin", str(self.stub), "--client-sha256", expected,
                                "--uuid", "1", "--accessToken=super-secret-token")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--uuid", "1",
                                                       "--accessToken=super-secret-token"])
        for flag, value, error in (
            ("--client-bin", str(self.root / "absent"), "not found"),
            ("--client-sha256", "0" * 64, "SHA-256 mismatch"),
        ):
            with self.subTest(flag=flag):
                result, log = self.java(flag, value, "--accessToken", "super-secret-token")
                self.assertEqual(result.returncode, 2)
                self.assertIn(error, result.stderr)
                self.assertNotIn("super-secret-token", result.stderr)
                self.assertFalse(log.exists())
        broken = self.root / "super-secret-token"
        broken.write_text("#!/nonexistent/interpreter\n")
        broken.chmod(0o755)
        result, _ = self.java("--client-bin", str(broken),
                              "--accessToken", "super-secret-token")
        self.assertEqual(result.returncode, 2)
        self.assertIn("<REDACTED>", result.stderr)
        self.assertNotIn("super-secret-token", result.stderr)

    def test_java_profile_sha256_pin_has_priority_over_environment(self):
        valid_sha256 = hashlib.sha256(self.stub.read_bytes()).hexdigest()
        result, log = self.invocation(
            "/usr/lib/jvm/java-21-openjdk/bin/java",
            "-Djava.io.tmpdir=" + tempfile.gettempdir(),
            "-Dmc.spine.clientSha256=" + valid_sha256,
            "-XX:-UsePerfData", "-jar",
            str(self.jar_dir / "mc-spine-bootstrapper.jar"),
            "--accessToken", "super-secret-token",
            extra_env={"MC_SPINE_CLIENT_SHA256": "0" * 64})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("super-secret-token", result.stderr or "")
        self.assertTrue(log.exists())

        result, log = self.invocation(
            "/usr/lib/jvm/java-21-openjdk/bin/java",
            "-Djava.io.tmpdir=" + tempfile.gettempdir(),
            "-Dmc.spine.clientSha256=" + "0" * 64,
            "-XX:-UsePerfData", "-jar",
            str(self.jar_dir / "mc-spine-bootstrapper.jar"),
            "--accessToken", "super-secret-token",
            extra_env={"MC_SPINE_CLIENT_SHA256": valid_sha256})
        self.assertEqual(result.returncode, 2)
        self.assertIn("SHA-256 mismatch", result.stderr)
        self.assertNotIn("super-secret-token", result.stderr)
        self.assertFalse(log.exists())

    def test_windows_exe_names_resolve_in_gamedir_bin_and_path(self):
        import shutil
        cases = []
        for index, (rel, name) in enumerate((
                ("", "mc-spine-client.exe"), ("bin", "megu.exe"),
                ("bin", "mc-spine-client-gpu.exe"), ("", "megu.exe"))):
            folder = self.root / f"exe-game-{index}" / ("gamedir with spaces" if index % 2 else "gamedir")
            target = folder / rel / name if rel else folder / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.stub, target)
            target.chmod(0o755)
            cases.append((folder, target))
        for folder, target in cases:
            with self.subTest(binary=target.name):
                result, log = self.java("--gameDir", str(folder), "--username", "Steve",
                                        extra_env={"MC_SPINE_CLIENT_BIN": ""})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(log.read_text()),
                                 ["--gameDir", str(folder), "--username", "Steve"])
        bindir = self.root / "exe-path-bin"
        bindir.mkdir()
        shutil.copyfile(self.stub, bindir / "mc-spine.exe")
        (bindir / "mc-spine.exe").chmod(0o755)
        env_path = str(bindir) + os.pathsep + os.environ.get("PATH", "")
        result, log = self.java("--username", "PATHUser",
                                extra_env={"MC_SPINE_CLIENT_BIN": "", "PATH": env_path})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--username", "PATHUser"])

    def test_token_env_flag_and_env_var_strip_access_token(self):
        env_log = self.root / "token-env.json"
        extra = {"TEST_NATIVE_ENV_LOG": str(env_log)}
        for label, token_args, env_extra in (
                ("flag-space", ("--accessToken", "flag-secret-token"), {"MC_SPINE_TOKEN_ENV": ""}),
                ("flag-equals", ("--accessToken=equals-secret-token",), {"MC_SPINE_TOKEN_ENV": ""}),
                ("env-var", ("--accessToken", "envvar-secret-token"), {"MC_SPINE_TOKEN_ENV": "1"})):
            with self.subTest(mode=label):
                env_log.unlink(missing_ok=True)
                args = ["--username", "Steve", "--uuid", "u-1", *token_args,
                        "--gameDir", "/tmp/game dir", "--assetsDir", "/tmp/äöü assets",
                        "--assetIndex", "26.3"]
                if label != "env-var":
                    args.insert(0, "--mc-spine-token-env")
                result, log = self.java(*args, extra_env={**extra, **env_extra})
                self.assertEqual(result.returncode, 0, result.stderr)
                forwarded = json.loads(log.read_text())
                self.assertNotIn("--accessToken", forwarded)
                self.assertNotIn("--mc-spine-token-env", forwarded)
                self.assertEqual(forwarded, ["--username", "Steve", "--uuid", "u-1",
                                             "--gameDir", "/tmp/game dir",
                                             "--assetsDir", "/tmp/äöü assets",
                                             "--assetIndex", "26.3"])
                seen = json.loads(env_log.read_text())
                self.assertEqual(seen["MC_SPINE_LAUNCHER_ACCESS_TOKEN"],
                                 {"flag-space": "flag-secret-token",
                                  "flag-equals": "equals-secret-token",
                                  "env-var": "envvar-secret-token"}[label])
                self.assertNotIn("secret-token", result.stderr or "")

    def test_access_token_stays_in_argv_without_token_env(self):
        env_log = self.root / "token-env-default.json"
        result, log = self.java("--username", "Steve", "--accessToken", "argv-secret-token",
                                extra_env={"TEST_NATIVE_ENV_LOG": str(env_log),
                                           "MC_SPINE_TOKEN_ENV": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()),
                         ["--username", "Steve", "--accessToken", "argv-secret-token"])
        self.assertIsNone(json.loads(env_log.read_text())["MC_SPINE_LAUNCHER_ACCESS_TOKEN"])

    def test_launcher_forwards_unicode_and_spaced_arguments_verbatim(self):
        game = self.root / "unicode ゲーム dir" / "saves/ワールド"
        game.mkdir(parents=True)
        args = ("--username", "プレイヤー 名前", "--uuid", "00000000-0000-0000-0000-0000000000ff",
                "--gameDir", str(game), "--assetsDir", str(self.root / "assets 空白"),
                "--assetIndex", "5", "--quickPlayMultiplayer", "例え.テスト:25565")
        result, log = self.java(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), list(args))

    def test_mc_version_263_profile_and_detect_minecraft_dir(self):
        profile_dir = self.root / "profile-263"
        self.generate(profile_dir, "--mc-version", "26.3")
        version = profile_dir / "versions/mc-spine-26.3/mc-spine-26.3.json"
        profile = json.loads(version.read_text())
        self.assertEqual(profile["inheritsFrom"], "26.3")
        self.assertEqual(profile["id"], "mc-spine-26.3")
        self.assertEqual(profile["mainClass"], "net.mcspine.launcher.Main")
        custom = self.root / "profile-custom-id"
        self.generate(custom, "--mc-version", "26.3", "--version-id", "megu-custom")
        self.assertEqual(json.loads(
            (custom / "versions/megu-custom/megu-custom.json").read_text())["inheritsFrom"],
            "26.3")
        detected = subprocess.run(
            [sys.executable, str(TOOLS / "gen_launcher_profile.py"), "--detect-minecraft-dir"],
            capture_output=True, text=True, check=True)
        expected = (Path.home() / ".minecraft" if sys.platform != "darwin"
                    else Path.home() / "Library" / "Application Support" / "minecraft")
        if sys.platform == "win32":
            expected = Path(os.environ.get("APPDATA", Path.home() / "AppData/Roaming")) / ".minecraft"
        flatpak = Path.home() / ".var/app/com.mojang.Minecraft/.minecraft"
        if (sys.platform not in ("win32", "darwin")
                and not (Path.home() / ".minecraft").is_dir() and flatpak.is_dir()):
            expected = flatpak
        self.assertEqual(Path(detected.stdout.strip()), expected)
        self.assertFalse((self.root / "versions").exists())


class CF4ExportTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-cf4-export-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.native = cls.root / "fake native"
        cls.native.write_text('''#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path
Path(os.environ['TEST_NATIVE_LOG']).write_text(json.dumps(sys.argv[1:]))
''')
        cls.native.chmod(0o755)
        cls.integrated = cls.root / "mc-spine-integrated.jar"
        subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                        "--out-dir", str(cls.root)], check=True, capture_output=True)

    def export(self, destination, *arguments, check=True):
        return subprocess.run([sys.executable, str(TOOLS / "export_launcher_bundle.py"),
                               "--out-dir", str(destination), *arguments],
                              capture_output=True, text=True, check=check, timeout=25)

    def test_prism_instance_and_native_wrapper_reproducible(self):
        output = self.root / "Prism instance with spaces"
        args = ("--format", "prism", "--loader", "fabric", "--loader-version", "0.16.10",
                "--client-bin", str(self.native), "--integrated-jar", str(self.integrated))
        self.export(output, *args)
        original = {path.relative_to(output): path.read_bytes()
                    for path in output.rglob("*") if path.is_file()}
        self.export(output, *args)
        self.assertEqual(original, {path.relative_to(output): path.read_bytes()
                                    for path in output.rglob("*") if path.is_file()})
        pack = json.loads((output / "mmc-pack.json").read_text())
        self.assertEqual(pack["formatVersion"], 1)
        self.assertEqual(pack["components"], [
            {"uid": "net.minecraft", "version": "1.21.1", "important": True},
            {"uid": "net.fabricmc.fabric-loader", "version": "0.16.10"}])
        config = configparser.ConfigParser(interpolation=None)
        config.read(output / "instance.cfg")
        self.assertEqual(config["General"]["OverrideCommands"], "true")
        command = shlex.split(config["General"]["WrapperCommand"])
        self.assertEqual(command[0], str((output / "mc-spine-wrapper").resolve()))
        self.assertTrue(os.access(command[0], os.X_OK))
        self.assertEqual(command[1:], ["--client-sha256",
                                       hashlib.sha256(self.native.read_bytes()).hexdigest()])
        self.assertEqual((output / ".minecraft/mods/mc-spine-integrated.jar").read_bytes(),
                         self.integrated.read_bytes())
        log = self.root / "prism-native.json"
        env = dict(os.environ, TEST_NATIVE_LOG=str(log))
        env.pop("MC_SPINE_CLIENT_BIN", None)
        result = subprocess.run([*command, "/usr/lib/jvm/java-21-openjdk/bin/java", "-Xmx2G",
                                 "-cp", "minecraft.jar", "net.minecraft.client.main.Main",
                                 "--username", "Steve", "--uuid", "test-uuid", "--accessToken",
                                 "super-secret-token", "--assetIndex", "17", "--gameDir",
                                 "/game with spaces", "--version", "1.21.1"],
                                capture_output=True, text=True, env=env, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("super-secret-token", result.stderr + result.stdout)
        self.assertEqual(json.loads(log.read_text()), ["--username", "Steve", "--uuid",
                         "test-uuid", "--accessToken", "super-secret-token", "--assetIndex",
                         "17", "--gameDir", "/game with spaces", "--version", "1.21.1"])

    def test_mrpack_integrity_zip_metadata_and_reproducibility(self):
        arguments = ("--format", "mrpack", "--loader", "neoforge",
                     "--loader-version", "21.1.215", "--integrated-jar", str(self.integrated))
        first, second = self.root / "mrpack-one", self.root / "mrpack-two"
        self.export(first, *arguments)
        self.export(second, *arguments)
        name = "mc-spine-1.21.1.mrpack"
        self.assertEqual((first / name).read_bytes(), (second / name).read_bytes())
        with zipfile.ZipFile(first / name) as archive:
            self.assertEqual(archive.namelist(), ["modrinth.index.json",
                             "overrides/mods/mc-spine-integrated.jar"])
            index = json.loads(archive.read("modrinth.index.json"))
            self.assertEqual(index["formatVersion"], 1)
            self.assertEqual(index["game"], "minecraft")
            self.assertEqual(index["dependencies"], {"minecraft": "1.21.1",
                                                       "neoforge": "21.1.215"})
            self.assertEqual(index["files"], [])
            contents = archive.read("overrides/mods/mc-spine-integrated.jar")
            self.assertEqual(contents, self.integrated.read_bytes())
            meta = index["mcSpine"]["bundledOverrides"]["mods/mc-spine-integrated.jar"]
            self.assertEqual(meta["fileSize"], len(contents))
            self.assertEqual(meta["hashes"], {
                "sha1": hashlib.sha1(contents).hexdigest(),
                "sha512": hashlib.sha512(contents).hexdigest()})
            for item in archive.infolist():
                self.assertEqual(item.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((item.external_attr >> 16) & 0o777, 0o644)
                self.assertEqual(item.create_system, 3)

    def test_wrapper_and_exporter_fail_closed(self):
        wrapper = str(TOOLS / "mc-spine-wrapper")
        env = dict(os.environ, MC_SPINE_CLIENT_BIN=str(self.root / "missing"))
        test_args = ["java", "-jar", "minecraft.jar", "--accessToken", "super-secret-token"]
        for commands, reason in (
            ([wrapper, *test_args], "not found"),
            ([wrapper, "--client-bin", str(self.native), "--client-sha256", "0" * 64,
              *test_args], "SHA-256 mismatch"),
        ):
            with self.subTest(reason=reason):
                result = subprocess.run(commands, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 2)
                self.assertIn(reason, result.stderr)
                self.assertNotIn("super-secret-token", result.stderr + result.stdout)
        invalid = self.export(self.root / "invalid-sha", "--client-bin", str(self.native),
                              "--client-sha256", "0" * 64, check=False)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("SHA-256 mismatch", invalid.stderr)
        missing_loader = self.export(self.root / "invalid-mrpack", "--format", "mrpack",
                                     check=False)
        self.assertNotEqual(missing_loader.returncode, 0)

    def test_optional_mesh_library_abi_and_packaging(self):
        mesh = ROOT / "client-port/target/release/libmc_spine_client_mesh.so"
        if not mesh.is_file():
            self.skipTest("build client-port release cdylib to test ABI packaging")
        output = self.root / "mesh-bundle"
        self.export(output, "--format", "all", "--loader", "fabric",
                    "--loader-version", "0.16.10", "--integrated-jar",
                    str(self.integrated), "--mesh-library", str(mesh))
        self.assertEqual((output / "prism/native/libmc_spine_client_mesh.so").read_bytes(),
                         mesh.read_bytes())
        with zipfile.ZipFile(output / "mc-spine-1.21.1.mrpack") as archive:
            content = archive.read("overrides/mc-spine/native/libmc_spine_client_mesh.so")
            index = json.loads(archive.read("modrinth.index.json"))
            expected = index["mcSpine"]["bundledOverrides"]["mc-spine/native/libmc_spine_client_mesh.so"]
            self.assertEqual(expected["hashes"]["sha512"], hashlib.sha512(content).hexdigest())
            self.assertEqual(content, mesh.read_bytes())
        invalid_lib = self.root / "not-a-library.so"
        invalid_lib.write_bytes(b"missing ABI")
        invalid = self.export(self.root / "invalid-mesh", "--format", "prism",
                              "--mesh-library", str(invalid_lib), check=False)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("mc_spine_mesh_abi_version", invalid.stderr)


    def test_optional_kernel_library_abi_and_packaging(self):
        kernel = require_native(self, KERNEL_LIB_NAME)
        mesh = require_native(self, MESH_LIB_NAME)
        arguments = ("--format", "all", "--loader", "fabric", "--loader-version", "0.16.10",
                     "--integrated-jar", str(self.integrated), "--kernel-library", str(kernel),
                     "--mesh-library", str(mesh))
        first = self.root / "kernel-bundle-one"
        self.export(first, *arguments)
        original = {path.relative_to(first): path.read_bytes()
                    for path in first.rglob("*") if path.is_file()}
        self.export(first, *arguments)
        self.assertEqual(original, {path.relative_to(first): path.read_bytes()
                                    for path in first.rglob("*") if path.is_file()})
        for name, library in ((KERNEL_LIB_NAME, kernel), (MESH_LIB_NAME, mesh)):
            self.assertEqual((first / "prism/native" / name).read_bytes(), library.read_bytes())
        with zipfile.ZipFile(first / "mc-spine-1.21.1.mrpack") as archive:
            self.assertEqual(archive.namelist(), [
                "modrinth.index.json",
                f"overrides/mc-spine/native/{MESH_LIB_NAME}",
                f"overrides/mc-spine/native/{KERNEL_LIB_NAME}",
                "overrides/mods/mc-spine-integrated.jar"])
            index = json.loads(archive.read("modrinth.index.json"))
            content = archive.read(f"overrides/mc-spine/native/{KERNEL_LIB_NAME}")
            self.assertEqual(content, kernel.read_bytes())
            meta = index["mcSpine"]["bundledOverrides"][f"mc-spine/native/{KERNEL_LIB_NAME}"]
            self.assertEqual(meta["fileSize"], len(content))
            self.assertEqual(meta["hashes"], {"sha1": hashlib.sha1(content).hexdigest(),
                                              "sha512": hashlib.sha512(content).hexdigest()})
            for item in archive.infolist():
                self.assertEqual(item.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((item.external_attr >> 16) & 0o777, 0o644)

    def test_kernel_library_fails_closed(self):
        fixtures = self.root / "kernel-fixtures"
        wrong_abi = compile_c_library(fixtures / "abi", KERNEL_LIB_NAME,
                                      "unsigned mc_spine_kernel_abi_version(void) { return 2; }\n")
        no_symbol = compile_c_library(fixtures / "symbol", KERNEL_LIB_NAME,
                                      "unsigned mc_spine_mesh_abi_version(void) { return 1; }\n")
        junk = fixtures / "junk.so"
        junk.write_bytes(b"missing ABI")
        for index, (library, reason) in enumerate((
                (wrong_abi, "unsupported kernel C ABI version 2, expected 1"),
                (no_symbol, "mc_spine_kernel_abi_version"),
                (junk, "mc_spine_kernel_abi_version"),
                (fixtures / "absent.so", "kernel library is missing"))):
            with self.subTest(reason=reason):
                for fmt in ("prism", "mrpack"):
                    out = self.root / f"invalid-kernel-{index}-{fmt}"
                    extra = ("--loader", "fabric", "--loader-version", "0.16.10",
                             "--integrated-jar", str(self.integrated)) if fmt == "mrpack" else ()
                    result = self.export(out, "--format", fmt, *extra,
                                         "--kernel-library", str(library), check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(reason, result.stderr)
                    self.assertFalse(out.exists() and any(out.rglob("*.mrpack")))
                    self.assertFalse((out / "native").exists())

    def build_bootstrap_jar(self, destination):
        subprocess.run([sys.executable, str(TOOLS / "gen_launcher_profile.py"),
                        "--out-dir", str(destination), "--build-jar"],
                       check=True, capture_output=True)
        jar = destination / "mc-spine-bootstrapper.jar"
        self.assertLess(jar.stat().st_size, 32 * 1024)
        return jar

    def test_prism_mc_version_263_components(self):
        output = self.root / "prism-263"
        self.export(output, "--format", "prism", "--mc-version", "26.3",
                    "--loader", "fabric", "--loader-version", "0.16.10")
        pack = json.loads((output / "mmc-pack.json").read_text())
        self.assertEqual(pack["components"][0],
                         {"uid": "net.minecraft", "version": "26.3", "important": True})
        config = configparser.ConfigParser(interpolation=None)
        config.read(output / "instance.cfg")
        self.assertIn("mc-spine 26.3", config["General"]["name"])

    def test_mrpack_mc_version_263(self):
        output = self.root / "mrpack-263"
        self.export(output, "--format", "mrpack", "--loader", "fabric",
                    "--loader-version", "0.16.10", "--mc-version", "26.3",
                    "--integrated-jar", str(self.integrated))
        bundle = output / "mc-spine-26.3.mrpack"
        self.assertTrue(bundle.is_file())
        with zipfile.ZipFile(bundle) as archive:
            index = json.loads(archive.read("modrinth.index.json"))
        self.assertEqual(index["dependencies"]["minecraft"], "26.3")
        self.assertEqual(index["name"], "mc-spine 26.3")
        self.assertIn("26.3", index["summary"])

    def test_prism_bootstrap_jar_native_patch_export(self):
        staging = self.root / "bootstrap-staging"
        jar = self.build_bootstrap_jar(staging)
        output = self.root / "prism bootstrap patch"
        digest = hashlib.sha256(self.native.read_bytes()).hexdigest()
        self.export(output, "--format", "prism", "--mc-version", "26.3",
                    "--bootstrap-jar", str(jar), "--client-bin", str(self.native),
                    "--client-sha256", digest)
        patch = json.loads((output / "patches/net.mcspine.bootstrapper.json").read_text())
        self.assertEqual(patch["formatVersion"], 1)
        self.assertEqual(patch["uid"], "net.mcspine.bootstrapper")
        self.assertEqual(patch["mainClass"], "net.mcspine.launcher.Main")
        self.assertEqual(patch["inheritsFrom"], "26.3")
        self.assertEqual(patch["libraries"],
                         [{"name": "net.mcspine:bootstrapper:1.0.0", "MMC-hint": "local"}])
        self.assertEqual(patch["+jvmArgs"], [f"-Dmc.spine.clientSha256={digest}"])
        library = (output / "libraries/net/mcspine/bootstrapper/1.0.0/"
                   "bootstrapper-1.0.0.jar")
        self.assertEqual(library.read_bytes(), jar.read_bytes())
        self.assertTrue((output / "mmc-pack.json").is_file())
        pack = json.loads((output / "mmc-pack.json").read_text())
        self.assertEqual(pack["components"][0]["version"], "26.3")
        config = configparser.ConfigParser(interpolation=None)
        config.read(output / "instance.cfg")
        self.assertNotIn("WrapperCommand", config["General"])
        self.assertEqual(config["General"]["OverrideCommands"], "false")
        bundled = output / ".minecraft/bin/mc-spine-client"
        self.assertEqual(bundled.read_bytes(), self.native.read_bytes())
        self.assertTrue(os.access(bundled, os.X_OK))
        self.assertFalse((output / "mc-spine-wrapper").exists())
        # Export stays reproducible.
        again = self.root / "prism bootstrap patch again"
        self.export(again, "--format", "prism", "--mc-version", "26.3",
                    "--bootstrap-jar", str(jar), "--client-bin", str(self.native),
                    "--client-sha256", digest)
        self.assertEqual((again / "patches/net.mcspine.bootstrapper.json").read_bytes(),
                         (output / "patches/net.mcspine.bootstrapper.json").read_bytes())

    def test_bootstrap_jar_fails_closed(self):
        missing = self.export(self.root / "bad-bootstrap-missing", "--format", "prism",
                              "--bootstrap-jar", str(self.root / "absent.jar"), check=False)
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("existing JAR", missing.stderr)
        wrong_format = self.export(self.root / "bad-bootstrap-mrpack", "--format", "mrpack",
                                   "--loader", "fabric", "--loader-version", "0.16.10",
                                   "--bootstrap-jar", str(self.root / "x.jar"), check=False)
        self.assertNotEqual(wrong_format.returncode, 0)
        self.assertIn("requires --format prism or all", wrong_format.stderr)
        oversized = self.root / "oversized.jar"
        oversized.write_bytes(b"x" * (32 * 1024))
        too_big = self.export(self.root / "bad-bootstrap-big", "--format", "prism",
                              "--bootstrap-jar", str(oversized), check=False)
        self.assertNotEqual(too_big.returncode, 0)
        self.assertIn("32 KiB", too_big.stderr)


class IntegratedModTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-integrated-test-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.output = cls.root / "build-a"
        cls.jar = cls.output / "mc-spine-integrated.jar"
        for folder in (cls.output, cls.root / "build-b"):
            subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                            "--out-dir", str(folder)], check=True, capture_output=True)
        cls.mock = cls.root / "mock-mc-spine"
        cls.mock.write_text('''#!/usr/bin/env python3
import json
import os
import signal
import socket
import sys
from pathlib import Path

def vint(sock):
    value = 0
    for shift in range(0, 35, 7):
        byte = sock.recv(1)
        if not byte:
            raise EOFError()
        value |= (byte[0] & 127) << shift
        if byte[0] < 128:
            return value
    raise ValueError('oversized VarInt')

def encode(value):
    out = bytearray()
    while True:
        b = value & 127
        value >>= 7
        out.append(b | (128 if value else 0))
        if not value:
            return out

def read(sock, length):
    data = bytearray()
    while len(data) < length:
        part = sock.recv(length - len(data))
        if not part:
            raise EOFError()
        data.extend(part)
    return data

def stop(*_):
    Path('stopped').write_text(str(os.getpid()))
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
host = os.environ['MC_SPINE_BIND_ADDR']
port = int(os.environ['MC_SPINE_PORT'])
with socket.socket() as listener:
    listener.bind((host, port))
    listener.listen()
    Path('server.json').write_text(json.dumps({
        'pid': os.getpid(), 'host': host, 'port': port,
        'cwd': os.getcwd(), 'world': os.environ['MC_SPINE_WORLD_DIR'],
        'seed': os.environ['MC_SPINE_SEED'],
        'rcon': os.getenv('MC_SPINE_RCON_PORT'),
        'public': os.getenv('MC_SPINE_ALLOW_PUBLIC_BIND'),
    }))
    while True:
        conn, _ = listener.accept()
        with conn:
            conn.settimeout(2)
            if os.getenv('MOCK_NO_PROTOCOL'):
                continue
            try:
                length = vint(conn)
                packet = read(conn, length)
                # First byte is handshake packet id 0, next two bytes encode 767.
                if packet[:3] != b'\\x00\\xff\\x05':
                    continue
                if vint(conn) != 1 or read(conn, 1) != b'\\x00':
                    continue
                description = b'{"version":{"name":"1.21.1","protocol":767},"description":{"text":"mc-spine"}}'
                body = encode(0) + encode(len(description)) + description
                conn.sendall(encode(len(body)) + body)
            except (OSError, EOFError, ValueError):
                pass
''')
        cls.mock.chmod(0o755)
        harness = cls.root / "ConfiguredBridgeHarness.java"
        harness.write_text('''import net.mcspine.integrated.IntegratedServerBridge;
public final class ConfiguredBridgeHarness {
    public static void main(String[] args) throws Exception {
        IntegratedServerBridge.startConfigured();
        IntegratedServerBridge.startConfigured();
        Thread.sleep(300);
        IntegratedServerBridge.closeConfigured();
    }
}
''')
        subprocess.run(["javac", "-J-Djava.io.tmpdir=" + tempfile.gettempdir(),
                        "-J-XX:-UsePerfData", "--release", "21", "-cp", str(cls.jar),
                        "-d", str(cls.root), str(harness)], check=True, capture_output=True)

    def command(self, binary, game, world="E2-4 world", seed="4242", *extra, jar_entry=False):
        entry = (["-jar", str(self.jar)] if jar_entry else
                 ["-cp", str(self.jar), "net.mcspine.integrated.IntegratedServerBridge"])
        return ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
                *entry,
                "--server-bin", str(binary), "--game-dir", str(game),
                "--world", world, "--seed", seed, *extra]

    def start(self, binary, game, world="E2-4 world", seed="4242", env=None, jar_entry=False):
        from selectors import DefaultSelector, EVENT_READ

        command = self.command(binary, game, world, seed, jar_entry=jar_entry)
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, env=env)
        with DefaultSelector() as poller:
            poller.register(process.stdout, EVENT_READ)
            try:
                self.assertTrue(poller.select(35), "Java bridge readiness timed out")
                ready = process.stdout.readline().strip()
                self.assertTrue(ready.startswith("READY 127.0.0.1:"), ready)
                endpoint, pid = ready.split()[1:]
                port = int(endpoint.rsplit(":", 1)[1])
                server_pid = int(pid.removeprefix("pid="))
                status(port)
                return process, port, server_pid
            except BaseException:
                process.kill()
                process.communicate(timeout=10)
                raise

    def assert_dead(self, pid):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.05)
        self.fail(f"mc-spine server pid {pid} survived Java shutdown")

    def test_manifest_both_loaders_and_reproducible(self):
        self.assertEqual(self.jar.read_bytes(),
                         (self.root / "build-b/mc-spine-integrated.jar").read_bytes())
        with zipfile.ZipFile(self.jar) as archive:
            entries = set(archive.namelist())
            fabric = json.loads(archive.read("fabric.mod.json"))
            neoforge = tomllib.loads(archive.read("META-INF/neoforge.mods.toml").decode())
            self.assertEqual(fabric["id"], "mc_spine_integrated")
            self.assertEqual(fabric["depends"]["minecraft"], "1.21.1")
            self.assertEqual(fabric["depends"]["java"], ">=21")
            self.assertEqual(fabric["environment"], "client")
            self.assertEqual(fabric["entrypoints"]["client"],
                             ["net.mcspine.integrated.FabricIntegratedMod"])
            self.assertEqual(neoforge["modLoader"], "javafml")
            self.assertEqual(neoforge["mods"][0]["modId"], "mc_spine_integrated")
            self.assertEqual(neoforge["dependencies"]["mc_spine_integrated"][0]["modId"],
                             "minecraft")
            self.assertIn("net/mcspine/integrated/IntegratedServerBridge.class", entries)
            self.assertIn("net/mcspine/integrated/FabricIntegratedMod.class", entries)
            self.assertIn("net/mcspine/integrated/NeoForgeIntegratedMod.class", entries)
            self.assertFalse(any(name.startswith("net/fabricmc/") or
                                 name.startswith("net/neoforged/") for name in entries))

    def test_mock_protocol_seed_world_and_explicit_close(self):
        game = self.root / "mock-game"
        game.mkdir(exist_ok=True)
        env = dict(os.environ, MC_SPINE_RCON_PORT="25575", MC_SPINE_ALLOW_PUBLIC_BIND="1")
        process, port, pid = self.start(self.mock, game, env=env)
        world = game / "saves/E2-4 world"
        try:
            info = json.loads((world / "server.json").read_text())
            self.assertEqual((info["host"], info["port"], info["pid"]),
                             ("127.0.0.1", port, pid))
            self.assertEqual(info["seed"], "4242")
            self.assertEqual(info["world"], str(world))
            self.assertEqual(info["cwd"], str(world))
            self.assertIsNone(info["rcon"])
            self.assertIsNone(info["public"])
            process.stdin.write("close\n")
            process.stdin.flush()
            _, stderr = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0, stderr)
            self.assertEqual((world / "stopped").read_text(), str(pid))
        finally:
            if process.poll() is None:
                process.terminate()
                process.communicate(timeout=10)
        self.assert_dead(pid)

    def test_configured_mod_lifecycle_starts_once_and_closes(self):
        game = self.root / "configured-game"
        game.mkdir(exist_ok=True)
        command = ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
                   "-Dmc.spine.integrated.enabled=true",
                   "-Dmc.spine.integrated.serverBin=" + str(self.mock),
                   "-Dmc.spine.integrated.gameDir=" + str(game),
                   "-Dmc.spine.integrated.world=Configured world",
                   "-Dmc.spine.integrated.seed=123",
                   "-cp", os.pathsep.join([str(self.jar), str(self.root)]),
                   "ConfiguredBridgeHarness"]
        result = subprocess.run(command, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        world = game / "saves/Configured world"
        info = json.loads((world / "server.json").read_text())
        self.assertEqual(info["seed"], "123")
        self.assertEqual((world / "stopped").read_text(), str(info["pid"]))
        self.assert_dead(info["pid"])

    def test_shutdown_hook_and_world_lock(self):
        game = self.root / "lock-game"
        game.mkdir(exist_ok=True)
        process, _, pid = self.start(self.mock, game)
        try:
            duplicate = subprocess.run(self.command(self.mock, game, "E2-4 world"),
                                       capture_output=True, text=True, timeout=10)
            self.assertEqual(duplicate.returncode, 2)
            self.assertIn("already in use", duplicate.stderr)
            process.terminate()
            process.communicate(timeout=12)
            self.assertEqual(process.returncode, 143)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=10)
        self.assert_dead(pid)
        self.assertEqual((game / "saves/E2-4 world/stopped").read_text(), str(pid))

    def test_rejects_unsafe_options_and_non_protocol_server(self):
        game = self.root / "invalid-game"
        game.mkdir(exist_ok=True)
        cases = (
            (self.root / "missing", "E2-4 world", "4242", (), "not executable"),
            (self.mock, "../escape", "4242", (), "safe single directory"),
            (self.mock, "E2-4 world", "x", (), "seed must"),
            (self.mock, "E2-4 world", "4242", ("--bind", "0.0.0.0"), "only 127.0.0.1"),
            (self.mock, "E2-4 world", "4242", ("--port", "25565"), "ephemeral"),
        )
        for binary, world, seed, extra, reason in cases:
            with self.subTest(reason=reason):
                result = subprocess.run(self.command(binary, game, world, seed, *extra),
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(reason, result.stderr)
        env = dict(os.environ, MOCK_NO_PROTOCOL="1")
        result = subprocess.run(self.command(self.mock, game, "timeout-world", "1",
                                             "--timeout-ms", "700"),
                                capture_output=True, text=True, timeout=10, env=env)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("protocol 767 startup timeout", result.stderr)
        world = game / "saves/timeout-world"
        info = json.loads((world / "server.json").read_text())
        self.assert_dead(info["pid"])
        self.assertEqual((world / "stopped").read_text(), str(info["pid"]))

    def lifecycle_run(self, loader, event, enabled=True):
        if not hasattr(type(self), "lifecycle_dir"):
            fakes = self.root / "lifecycle-src"
            for name, body in LIFECYCLE_FAKES.items():
                path = fakes / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(body)
            classes = self.root / "lifecycle-classes"
            subprocess.run(["javac", "-J-Djava.io.tmpdir=" + tempfile.gettempdir(),
                            "-J-XX:-UsePerfData", "--release", "21", "-cp", str(self.jar),
                            "-d", str(classes), *map(str, sorted(fakes.rglob("*.java")))],
                           check=True, capture_output=True)
            type(self).lifecycle_dir = classes
        game = self.root / f"lifecycle-{loader}-{event}-{enabled}"
        game.mkdir(exist_ok=True)
        flags = ["-Dmc.spine.integrated.enabled=true"] if enabled else []
        return game, subprocess.run(
            ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData", *flags,
             "-Dmc.spine.integrated.serverBin=" + str(self.mock),
             "-Dmc.spine.integrated.gameDir=" + str(game),
             "-Dmc.spine.integrated.world=Lifecycle world",
             "-cp", os.pathsep.join([str(self.lifecycle_dir), str(self.jar)]),
             "LifecycleHarness", loader, event], capture_output=True, text=True, timeout=30)

    def test_client_lifecycle_hooks_close_server_for_both_loaders(self):
        for loader, event, hooks in (("fabric", "stopping", 2), ("fabric", "disconnect", 2),
                                     ("neoforge", "logout", 2), ("neoforge", "shutdown", 2)):
            with self.subTest(loader=loader, event=event):
                game, result = self.lifecycle_run(loader, event)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"HOOKS {hooks}", result.stdout)
                self.assertIn("STOPPED_BY_HOOK true", result.stdout)
                world = game / "saves/Lifecycle world"
                info = json.loads((world / "server.json").read_text())
                self.assertEqual((world / "stopped").read_text(), str(info["pid"]))
                self.assert_dead(info["pid"])

    def test_client_lifecycle_hooks_inert_unless_enabled(self):
        for loader in ("fabric", "neoforge"):
            with self.subTest(loader=loader):
                game, result = self.lifecycle_run(loader, "stopping", enabled=False)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("HOOKS 0", result.stdout)
                self.assertIn("INERT false", result.stdout)
                self.assertFalse((game / "saves").exists())

    def test_fabric_hooks_survive_missing_fabric_api(self):
        game = self.root / "lifecycle-no-fabric-api"
        game.mkdir(exist_ok=True)
        classes = self.root / "no-api-classes"
        source = self.root / "NoApiHarness.java"
        source.write_text("""import net.mcspine.integrated.FabricIntegratedMod;
public final class NoApiHarness {
    public static void main(String[] args) { new FabricIntegratedMod().onInitializeClient(); }
}
""")
        stub = self.root / "no-api-stub/net/fabricmc/api/ClientModInitializer.java"
        stub.parent.mkdir(parents=True, exist_ok=True)
        stub.write_text(LIFECYCLE_FAKES["net/fabricmc/api/ClientModInitializer.java"])
        subprocess.run(["javac", "-J-Djava.io.tmpdir=" + tempfile.gettempdir(),
                        "-J-XX:-UsePerfData", "--release", "21", "-cp", str(self.jar),
                        "-d", str(classes), str(stub), str(source)],
                       check=True, capture_output=True)
        result = subprocess.run(
            ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
             "-Dmc.spine.integrated.enabled=true",
             "-Dmc.spine.integrated.serverBin=" + str(self.mock),
             "-Dmc.spine.integrated.gameDir=" + str(game),
             "-Dmc.spine.integrated.world=Lifecycle world",
             "-cp", os.pathsep.join([str(classes), str(self.jar)]), "NoApiHarness"],
            capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Fabric hook CLIENT_STOPPING unavailable", result.stderr)
        self.assertIn("Fabric hook DISCONNECT unavailable", result.stderr)
        world = game / "saves/Lifecycle world"
        info = json.loads((world / "server.json").read_text())
        self.assertEqual((world / "stopped").read_text(), str(info["pid"]))
        self.assert_dead(info["pid"])


    def test_real_protocol_767_and_clean_shutdown(self):
        binary = ROOT / "target/debug/mc-spine"
        if not binary.is_file():
            self.skipTest("build target/debug/mc-spine to run the real binary integration test")
        game = self.root / "real-game"
        game.mkdir(exist_ok=True)
        env = dict(os.environ, MC_SPINE_SERVER_HOME=str(ROOT))
        process, port, pid = self.start(binary, game, env=env, jar_entry=True)
        world = game / "saves/E2-4 world"
        try:
            self.assertGreater(port, 0)
            self.assertTrue((world / "assets").is_symlink())
            self.assertTrue((world / "config_packets.bin").is_symlink())
            process.stdin.close()
            process.stdin = None
            _, stderr = process.communicate(timeout=12)
            self.assertEqual(process.returncode, 0, stderr)
        finally:
            if process.poll() is None:
                process.terminate()
                process.communicate(timeout=10)
        self.assert_dead(pid)


class DirectFrameBridgeTest(unittest.TestCase):
    """CF-3b: direct ByteBuffer MCSI v1 framing, fail-closed rules and relay."""

    HARNESS = r'''import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReference;
import java.util.zip.CRC32;

import net.mcspine.integrated.IntegratedServerBridge;
import net.mcspine.integrated.IntegratedServerBridge.DirectFrame;

public final class DirectFrameHarness {
    private static int checks;

    @FunctionalInterface
    interface ThrowingAction { void run() throws Exception; }

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
        checks++;
    }

    private static void expectIae(ThrowingAction action, String message) {
        try {
            action.run();
            throw new AssertionError("expected IllegalArgumentException: " + message);
        } catch (IllegalArgumentException expected) {
            String actual = expected.getMessage();
            if (actual == null || !actual.contains(message)) {
                throw new AssertionError("unexpected IAE message: " + actual);
            }
            checks++;
        } catch (Exception other) {
            throw new AssertionError("wrong exception for " + message, other);
        }
    }

    private static ByteBuffer direct(int capacity) {
        return ByteBuffer.allocateDirect(capacity).order(ByteOrder.LITTLE_ENDIAN);
    }

    private static ByteBuffer view(ByteBuffer buf) {
        ByteBuffer copy = buf.duplicate();
        copy.order(ByteOrder.LITTLE_ENDIAN);
        return copy;
    }

    private static byte[] sectionPayload(int length) {
        byte[] payload = new byte[length];
        for (int i = 0; i < length; i++) {
            payload[i] = (byte) (i * 31 + 7);
        }
        return payload;
    }

    private static byte[] heartbeatPayload(long tick) {
        return ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(tick).array();
    }

    private static void roundtrip() {
        byte[][] payloads = {
            {(byte) 0x00, (byte) 0xff, 0x05, 0x10, 0x00, 0x01, 0x02},
            sectionPayload(4096),
            heartbeatPayload(42L),
        };
        int[] kinds = {
            IntegratedServerBridge.KIND_PROTOCOL767_PACKET,
            IntegratedServerBridge.KIND_PALETTED_SECTION_SNAPSHOT,
            IntegratedServerBridge.KIND_TICK_HEARTBEAT,
        };
        for (int i = 0; i < kinds.length; i++) {
            ByteBuffer buf = direct(32 + payloads[i].length);
            IntegratedServerBridge.writeDirectFrame(buf, kinds[i], payloads[i]);
            check(buf.position() == 16 + payloads[i].length, "frame size");
            buf.flip();
            check(buf.order() == ByteOrder.LITTLE_ENDIAN, "byte order preserved");
            DirectFrame frame = IntegratedServerBridge.readDirectFrame(buf);
            check(frame.kind() == kinds[i], "frame kind");
            check(Arrays.equals(frame.payload(), payloads[i]), "frame payload");
            CRC32 crc = new CRC32();
            crc.update(payloads[i]);
            check(frame.crc32() == crc.getValue(), "frame crc32");
            check(buf.position() == buf.limit(), "buffer consumed");
        }
    }

    private static void failClosed() {
        byte[] payload = {1, 2, 3, 4};
        expectIae(() -> IntegratedServerBridge.writeDirectFrame(
                ByteBuffer.allocate(64).order(ByteOrder.LITTLE_ENDIAN), 1, payload), "direct");
        expectIae(() -> IntegratedServerBridge.readDirectFrame(null), "null");
        expectIae(() -> IntegratedServerBridge.writeDirectFrame(
                ByteBuffer.allocateDirect(64).order(ByteOrder.BIG_ENDIAN), 1, payload),
                "LITTLE_ENDIAN");
        expectIae(() -> IntegratedServerBridge.writeDirectFrame(direct(64), 1, null), "payload");
        expectIae(() -> IntegratedServerBridge.writeDirectFrame(direct(64), 99, payload), "kind");
        expectIae(() -> IntegratedServerBridge.writeDirectFrame(
                direct(300_000), 1, new byte[262_145]), "exceeds");
        expectIae(() -> IntegratedServerBridge.writeDirectFrame(direct(19), 1, payload), "too small");

        ByteBuffer badMagic = direct(64);
        IntegratedServerBridge.writeDirectFrame(badMagic, 1, payload);
        badMagic.putInt(0, 0x12345678);
        badMagic.flip();
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(badMagic)), "magic");

        ByteBuffer badVersion = direct(64);
        IntegratedServerBridge.writeDirectFrame(badVersion, 1, payload);
        badVersion.putShort(4, (short) 2);
        badVersion.flip();
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(badVersion)), "version");

        ByteBuffer badKind = direct(64);
        IntegratedServerBridge.writeDirectFrame(badKind, 1, payload);
        badKind.putShort(6, (short) 9);
        badKind.flip();
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(badKind)), "kind");

        ByteBuffer truncated = direct(64);
        IntegratedServerBridge.writeDirectFrame(truncated, 1, payload);
        truncated.flip();
        truncated.limit(19);
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(truncated)), "truncated");

        ByteBuffer shortHeader = direct(64);
        shortHeader.limit(10);
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(shortHeader)), "truncated");

        ByteBuffer oversized = direct(64);
        IntegratedServerBridge.writeDirectFrame(oversized, 1, payload);
        oversized.putInt(8, 262_145);
        oversized.flip();
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(oversized)), "length");

        ByteBuffer badCrc = direct(64);
        IntegratedServerBridge.writeDirectFrame(badCrc, 1, payload);
        badCrc.put(16, (byte) (payload[0] ^ 0x55));
        badCrc.flip();
        expectIae(() -> IntegratedServerBridge.readDirectFrame(view(badCrc)), "CRC-32");
    }

    private static void loopback() throws Exception {
        byte[] requestPayload = {(byte) 0x00, (byte) 0xff, 0x05, 0x02, 0x00};
        byte[] responsePayload = sectionPayload(512);
        AtomicReference<Throwable> serverError = new AtomicReference<>();
        try (ServerSocket listener = new ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))) {
            Thread server = new Thread(() -> {
                try (Socket socket = listener.accept()) {
                    socket.setSoTimeout(5000);
                    DataInputStream in = new DataInputStream(socket.getInputStream());
                    byte[] header = new byte[16];
                    in.readFully(header);
                    ByteBuffer fields = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN);
                    if (fields.getInt(0) != 0x4D435349
                            || Short.toUnsignedInt(fields.getShort(4)) != 1
                            || Short.toUnsignedInt(fields.getShort(6)) != 1
                            || fields.getInt(8) != requestPayload.length) {
                        throw new AssertionError("server saw a bad request header");
                    }
                    byte[] got = new byte[requestPayload.length];
                    in.readFully(got);
                    if (!Arrays.equals(got, requestPayload)) {
                        throw new AssertionError("server saw a bad request payload");
                    }
                    ByteBuffer response = direct(64 + responsePayload.length);
                    IntegratedServerBridge.writeDirectFrame(
                            response, IntegratedServerBridge.KIND_PALETTED_SECTION_SNAPSHOT,
                            responsePayload);
                    response.flip();
                    byte[] wire = new byte[response.remaining()];
                    response.get(wire);
                    OutputStream out = socket.getOutputStream();
                    out.write(wire);
                    out.flush();
                } catch (Throwable error) {
                    serverError.set(error);
                }
            }, "mcdirect-test-server");
            server.setDaemon(true);
            server.start();
            try (Socket client = new Socket("127.0.0.1", listener.getLocalPort())) {
                client.setSoTimeout(5000);
                ByteBuffer requestBuf = direct(64 + requestPayload.length);
                IntegratedServerBridge.writeDirectFrame(
                        requestBuf, IntegratedServerBridge.KIND_PROTOCOL767_PACKET,
                        requestPayload);
                ByteBuffer responseBuf = direct(64 + responsePayload.length);
                DirectFrame frame = IntegratedServerBridge.exchangeFrameOverLoopback(
                        requestBuf, client.getInputStream(), client.getOutputStream(), responseBuf);
                check(frame.kind() == IntegratedServerBridge.KIND_PALETTED_SECTION_SNAPSHOT,
                        "loopback response kind");
                check(Arrays.equals(frame.payload(), responsePayload), "loopback response payload");
                check(responseBuf.order() == ByteOrder.LITTLE_ENDIAN, "response byte order");
                check(responseBuf.position() == 0, "response position reset");
                check(responseBuf.limit() == 16 + responsePayload.length, "response limit");
                byte[] raw = new byte[responseBuf.limit()];
                responseBuf.get(raw);
                ByteBuffer reread = direct(raw.length);
                reread.put(raw);
                reread.flip();
                check(Arrays.equals(
                        IntegratedServerBridge.readDirectFrame(reread).payload(), responsePayload),
                        "response buffer holds a complete frame");
                expectIae(() -> IntegratedServerBridge.exchangeFrameOverLoopback(
                        ByteBuffer.allocate(64).order(ByteOrder.LITTLE_ENDIAN),
                        client.getInputStream(), client.getOutputStream(), responseBuf), "direct");
                expectIae(() -> IntegratedServerBridge.exchangeFrameOverLoopback(
                        requestBuf, client.getInputStream(), client.getOutputStream(),
                        ByteBuffer.allocate(64).order(ByteOrder.LITTLE_ENDIAN)), "direct");
            }
            server.join(5000);
            check(!server.isAlive(), "server thread finished");
            if (serverError.get() != null) {
                throw new AssertionError("loopback server failed", serverError.get());
            }
        }
    }

    private static void outboundCrc() throws Exception {
        byte[] payload = {1, 2, 3, 4, 5};
        ByteBuffer request = direct(64 + payload.length);
        IntegratedServerBridge.writeDirectFrame(request,
                IntegratedServerBridge.KIND_PROTOCOL767_PACKET, payload);
        int position = request.position();
        int limit = request.limit();
        ByteBuffer wireView = view(request);
        wireView.flip();
        byte[] expectedWire = new byte[position];
        wireView.get(expectedWire);

        ByteBuffer corrupt = direct(request.capacity());
        corrupt.put(expectedWire);
        corrupt.put(IntegratedServerBridge.DIRECT_FRAME_HEADER_SIZE, (byte) (payload[0] ^ 0x5a));
        ByteArrayOutputStream corruptSink = new ByteArrayOutputStream();
        expectIae(() -> IntegratedServerBridge.exchangeFrameOverLoopback(corrupt,
                new ByteArrayInputStream(new byte[0]), corruptSink, direct(64)),
                "CRC-32");
        check(corruptSink.size() == 0, "corrupt outbound CRC wrote no bytes");
        check(corrupt.position() == position, "corrupt request position preserved");
        check(corrupt.limit() == limit, "corrupt request limit preserved");

        ByteArrayOutputStream shortSink = new ByteArrayOutputStream();
        expectIae(() -> IntegratedServerBridge.exchangeFrameOverLoopback(view(request),
                new ByteArrayInputStream(new byte[0]), shortSink, direct(8)),
                "responseBuf");
        check(shortSink.size() == 0, "short response buffer wrote no bytes");

        byte[] reply = sectionPayload(256);
        ByteBuffer replyFrame = direct(64 + reply.length);
        IntegratedServerBridge.writeDirectFrame(replyFrame,
                IntegratedServerBridge.KIND_TICK_HEARTBEAT, reply);
        replyFrame.flip();
        byte[] replyWire = new byte[replyFrame.remaining()];
        replyFrame.get(replyWire);
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        ByteBuffer responseTarget = direct(64 + reply.length);
        DirectFrame decoded = IntegratedServerBridge.exchangeFrameOverLoopback(
                view(request), new ByteArrayInputStream(replyWire), sink, responseTarget);
        check(Arrays.equals(sink.toByteArray(), expectedWire), "valid outbound bytes written");
        check(decoded.kind() == IntegratedServerBridge.KIND_TICK_HEARTBEAT,
                "in-memory response kind");
        check(Arrays.equals(decoded.payload(), reply), "in-memory response payload");
        check(responseTarget.position() == 0, "in-memory response position reset");
    }

    public static void main(String[] args) throws Exception {
        switch (args[0]) {
            case "roundtrip" -> roundtrip();
            case "failclosed" -> failClosed();
            case "loopback" -> loopback();
            case "outboundcrc" -> outboundCrc();
            default -> throw new IllegalArgumentException("unknown mode " + args[0]);
        }
        System.out.println(args[0].toUpperCase() + "_OK checks=" + checks);
    }
}
'''

    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-direct-frame-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.build_a = cls.root / "build-a"
        cls.build_b = cls.root / "build-b"
        for folder in (cls.build_a, cls.build_b):
            subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                            "--out-dir", str(folder)], check=True, capture_output=True)
        cls.jar = cls.build_a / "mc-spine-integrated.jar"
        cls.harness = cls.root / "DirectFrameHarness.java"
        cls.harness.write_text(cls.HARNESS)
        subprocess.run(["javac", "-J-Djava.io.tmpdir=" + tempfile.gettempdir(),
                        "-J-XX:-UsePerfData", "--release", "21", "-cp", str(cls.jar),
                        "-d", str(cls.root), str(cls.harness)], check=True, capture_output=True)

    def run_harness(self, mode):
        return subprocess.run(
            ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
             "-cp", os.pathsep.join([str(self.root), str(self.jar)]),
             "DirectFrameHarness", mode],
            capture_output=True, text=True, timeout=30)

    def test_direct_frame_roundtrip_all_kinds(self):
        result = self.run_harness("roundtrip")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ROUNDTRIP_OK checks=18")

    def test_direct_frame_fail_closed(self):
        result = self.run_harness("failclosed")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "FAILCLOSED_OK checks=14")

    def test_loopback_frame_exchange(self):
        result = self.run_harness("loopback")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "LOOPBACK_OK checks=9")

    def test_outbound_crc_rejected_before_write(self):
        result = self.run_harness("outboundcrc")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "OUTBOUNDCRC_OK checks=10")

    def test_integrated_jar_rebuild_is_byte_identical(self):
        first = self.jar
        second = self.build_b / "mc-spine-integrated.jar"
        digest = hashlib.sha256(first.read_bytes()).hexdigest()
        self.assertEqual(digest, hashlib.sha256(second.read_bytes()).hexdigest())
        with zipfile.ZipFile(first) as archive:
            names = set(archive.namelist())
            self.assertIn("net/mcspine/integrated/IntegratedServerBridge$DirectFrame.class", names)
            self.assertFalse(any(name.startswith(("net/fabricmc/", "net/neoforged/"))
                                 for name in names))
            for item in archive.infolist():
                self.assertEqual(item.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((item.external_attr >> 16) & 0o777, 0o644)
                self.assertEqual(item.create_system, 3)


KERNEL_LIB_NAME = "libmc_spine_kernel.so"
MESH_LIB_NAME = "libmc_spine_client_mesh.so"
_NATIVE = {}


def native_libraries():
    """Build the client-kernel/client-port cdylibs once. Returns {name: (path|None, reason)}.

    MC_SPINE_TEST_KERNEL_LIBRARY may name a prebuilt libmc_spine_kernel.so instead of the
    cargo build. A failed build yields (None, first failure) so tests report it.
    """
    if _NATIVE:
        return _NATIVE
    override = os.environ.get("MC_SPINE_TEST_KERNEL_LIBRARY")
    for crate, name in (("client-kernel", KERNEL_LIB_NAME), ("client-port", MESH_LIB_NAME)):
        if name == KERNEL_LIB_NAME and override:
            _NATIVE[name] = (Path(override), "")
            continue
        result = subprocess.run(
            ["cargo", "build", "--manifest-path", str(ROOT / crate / "Cargo.toml"), "--release",
             "-j", "2", "--offline", "--locked"], capture_output=True, text=True, timeout=290)
        library = ROOT / crate / "target/release" / name
        if result.returncode == 0 and library.is_file():
            _NATIVE[name] = (library, "")
        else:
            _NATIVE[name] = (None, f"{crate} cargo build failed: {result.stderr.strip()[-400:]}")
    return _NATIVE


def require_native(case, name):
    library, reason = native_libraries()[name]
    if library is None:
        case.skipTest(reason)
    return library


def compile_c_library(directory, name, source):
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "lib.c").write_text(source)
    subprocess.run(["cc", "-shared", "-fPIC", "-o", str(directory / name),
                    str(directory / "lib.c")], check=True, capture_output=True)
    return directory / name


class NativeBridgeTest(unittest.TestCase):
    """Java NativeKernelBridge/NativeMeshBridge against the real C ABI libraries."""

    HARNESS = r'''import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Path;
import java.util.Arrays;
import net.mcspine.integrated.NativeBridgeException;
import net.mcspine.integrated.NativeKernelBridge;
import net.mcspine.integrated.NativeMeshBridge;

public final class NativeBridgeHarness {
    static int checks;

    static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
        checks++;
    }

    static ByteBuffer direct(int size) {
        return ByteBuffer.allocateDirect(size).order(ByteOrder.nativeOrder());
    }

    static NativeBridgeException expect(Runnable action, String fragment) {
        try {
            action.run();
        } catch (NativeBridgeException error) {
            check(error.getMessage().contains(fragment), "message '" + error.getMessage() + "' lacks '" + fragment + "'");
            return error;
        }
        throw new AssertionError("expected NativeBridgeException containing " + fragment);
    }

    static ByteBuffer aabb(double... values) {
        ByteBuffer buffer = direct(values.length * 8);
        for (double value : values) {
            buffer.putDouble(value);
        }
        return buffer.flip();
    }

    static void kernel(Path library) throws Exception {
        NativeKernelBridge bridge = NativeKernelBridge.open(library);
        NativeKernelBridge.Palette palette = bridge.newPalette(0);
        check(palette.get(0) == 0 && palette.nonAir() == 0, "air palette");
        byte[] uniform = palette.writeWire();
        palette.set(5, 1);
        palette.set(4095, 1);
        check(palette.get(5) == 1 && palette.get(4) == 0, "palette set/get");
        check(palette.nonAir() == 2, "non air count");
        byte[] wire = palette.writeWire();
        check(wire.length > uniform.length && Arrays.equals(wire, palette.writeWire()), "wire output");
        expect(() -> palette.get(4096), "status 2");
        expect(() -> palette.set(0, 26684), "status 4");
        palette.close();
        palette.close();
        expect(() -> palette.get(0), "closed");
        try (NativeKernelBridge.Palette other = bridge.newPalette(1)) {
            check(other.get(100) == 1 && other.nonAir() == 4096, "single-state palette");
        }

        ByteBuffer body = aabb(0, 0, 0, 1, 1, 1);
        ByteBuffer floor = aabb(-1, -1, -1, 2, 0, 2);
        NativeKernelBridge.Collision hit = bridge.collide(body, aabb(0, -1, 0), false, 0f, floor, 1);
        check(hit.dx() == 0 && hit.dy() == 0 && hit.dz() == 0, "landing delta " + hit);
        check((hit.flags() & 2) != 0, "vertical flag " + hit);
        NativeKernelBridge.Collision free = bridge.collide(body, aabb(0.5, 0, 0.25), true, 0f, null, 0);
        check(free.dx() == 0.5 && free.dz() == 0.25 && free.flags() == 0, "free movement " + free);
        check(body.position() == 0 && body.remaining() == 48, "caller buffer position untouched");

        ByteBuffer valid = direct(4).put(new byte[] {10, 0, 0, 0}).flip();
        check(bridge.nbtValidateNamed(valid) == 0, "valid nbt");
        ByteBuffer truncated = direct(3).put(new byte[] {10, 0, 0}).flip();
        check(bridge.nbtValidateNamed(truncated) == 5, "invalid nbt");

        expect(() -> bridge.collide(ByteBuffer.allocate(48), aabb(0, 0, 0), false, 0f, null, 0), "direct ByteBuffer");
        expect(() -> bridge.nbtValidateNamed(ByteBuffer.wrap(new byte[] {10, 0, 0, 0})), "direct ByteBuffer");
        ByteBuffer big = direct(128).order(ByteOrder.nativeOrder());
        ByteBuffer misaligned = big.position(4).slice().order(ByteOrder.nativeOrder());
        expect(() -> bridge.collide(misaligned, aabb(0, 0, 0), false, 0f, null, 0), "aligned");
        expect(() -> bridge.collide(aabb(0, 0, 0), aabb(0, 0, 0), false, 0f, null, 0), "too small");
        expect(() -> bridge.collide(body, aabb(0, 0, 0), false, 0f, null, 1), "shapes");

        NativeKernelBridge.Palette survivor = bridge.newPalette(0);
        bridge.close();
        bridge.close();
        check(!bridge.isOpen(), "closed");
        expect(() -> bridge.newPalette(0), "closed");
        expect(() -> survivor.get(0), "closed");
        survivor.close();
    }

    static void mesh(Path library) throws Exception {
        NativeMeshBridge bridge = NativeMeshBridge.open(library);
        ByteBuffer blocks = direct(4096 * 16);
        int cell = (1 * 256 + 1 * 16 + 1) * 16;
        blocks.putInt(cell, 1).put(cell + 12, (byte) 1).put(cell + 13, (byte) 0x3f);
        ByteBuffer blockLight = direct(2048);
        ByteBuffer skyLight = direct(2048);
        check(bridge.meshSection(blocks, blockLight, skyLight, 0, null, 0) == 6, "quad count query");
        ByteBuffer quads = direct(6 * 84);
        check(bridge.meshSection(blocks, blockLight, skyLight, 0, quads, 6) == 6, "quads");
        boolean[] seen = new boolean[6];
        for (int i = 0; i < 6; i++) {
            check(quads.getInt(i * 84 + 48) == 1, "quad state");
            seen[quads.get(i * 84 + 77)] = true;
        }
        for (boolean face : seen) {
            check(face, "all six directions meshed");
        }
        NativeBridgeException small = expect(() -> bridge.meshSection(blocks, blockLight, skyLight, 0,
                direct(5 * 84), 5), "status 2");
        check(small.status() == 2 && small.getMessage().contains("required 6"), "required count");
        check(bridge.packVertices(quads, 6, null, 0) == 24, "vertex count query");
        ByteBuffer vertices = direct(24 * 16);
        check(bridge.packVertices(quads, 6, vertices, 24) == 24, "vertices");
        check(vertices.getInt(4) == 1, "vertex state id");
        expect(() -> bridge.packVertices(quads, 6, direct(16), 1), "status 2");
        expect(() -> bridge.meshSection(ByteBuffer.allocate(4096 * 16), blockLight, skyLight, 0, null, 0),
                "direct ByteBuffer");
        expect(() -> bridge.meshSection(blocks, blockLight, skyLight, 0, null, 3), "output buffer");
        expect(() -> bridge.meshSection(blocks, direct(16), skyLight, 0, null, 0), "too small");
        bridge.close();
        bridge.close();
        expect(() -> bridge.meshSection(blocks, blockLight, skyLight, 0, null, 0), "closed");
    }

    static void failClosed(Path directory) {
        String kernel = "libmc_spine_kernel.so";
        String mesh = "libmc_spine_client_mesh.so";
        expect(() -> NativeKernelBridge.open(null), "absolute");
        expect(() -> NativeKernelBridge.open(Path.of(kernel)), "absolute");
        expect(() -> NativeKernelBridge.open(directory.resolve("missing/../missing/" + kernel)), "normalized");
        expect(() -> NativeKernelBridge.open(directory.resolve("missing").resolve(kernel)), "missing");
        expect(() -> NativeKernelBridge.open(directory.resolve("wrongname/libother.so")), "must be named");
        expect(() -> NativeKernelBridge.open(directory.resolve("junk").resolve(kernel)), "cannot load");
        expect(() -> NativeKernelBridge.open(directory.resolve("writable").resolve(kernel)), "writable");
        expect(() -> NativeKernelBridge.open(directory.resolve("wrongabi").resolve(kernel)), "ABI 2");
        expect(() -> NativeMeshBridge.open(directory.resolve("wrongabi").resolve(mesh)), "ABI 2");
        expect(() -> NativeKernelBridge.open(directory.resolve("crosswired").resolve(kernel)), "lacks symbol");
        expect(() -> NativeMeshBridge.open(directory.resolve("crosswired").resolve(mesh)), "lacks symbol");
        expect(() -> NativeKernelBridge.open(directory.resolve("partial").resolve(kernel)),
                "lacks symbol mc_spine_kernel_palette_new_single");
    }

    public static void main(String[] args) throws Exception {
        switch (args[0]) {
            case "kernel" -> kernel(Path.of(args[1]));
            case "mesh" -> mesh(Path.of(args[1]));
            case "failclosed" -> failClosed(Path.of(args[1]));
            default -> throw new IllegalArgumentException("unknown mode " + args[0]);
        }
        System.out.println(args[0].toUpperCase() + "_OK checks=" + checks);
    }
}
'''

    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-native-bridge-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.jar_dir = cls.root / "jar"
        subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                        "--out-dir", str(cls.jar_dir)], check=True, capture_output=True)
        cls.jar = cls.jar_dir / "mc-spine-integrated.jar"
        harness = cls.root / "NativeBridgeHarness.java"
        harness.write_text(cls.HARNESS)
        subprocess.run(["javac", "-J-Djava.io.tmpdir=" + tempfile.gettempdir(),
                        "-J-XX:-UsePerfData", "--release", "21", "-cp", str(cls.jar),
                        "-d", str(cls.root), str(harness)], check=True, capture_output=True)
        fixtures = cls.root / "fixtures"
        cls.fixtures = fixtures
        abi = "unsigned %s(void) { return %d; }\n"
        kernel_abi, mesh_abi = "mc_spine_kernel_abi_version", "mc_spine_mesh_abi_version"
        for folder in ("missing", "wrongname"):
            (fixtures / folder).mkdir(parents=True, exist_ok=True)
        (fixtures / "wrongname/libother.so").write_bytes(b"x")
        (fixtures / "junk").mkdir(parents=True, exist_ok=True)
        (fixtures / "junk" / KERNEL_LIB_NAME).write_bytes(b"not an ELF object")
        writable = compile_c_library(fixtures / "writable", KERNEL_LIB_NAME, abi % (kernel_abi, 1))
        writable.chmod(0o666)
        compile_c_library(fixtures / "wrongabi", KERNEL_LIB_NAME, abi % (kernel_abi, 2))
        compile_c_library(fixtures / "wrongabi", MESH_LIB_NAME, abi % (mesh_abi, 2))
        compile_c_library(fixtures / "crosswired", KERNEL_LIB_NAME, abi % (mesh_abi, 1))
        compile_c_library(fixtures / "crosswired", MESH_LIB_NAME, abi % (kernel_abi, 1))
        compile_c_library(fixtures / "partial", KERNEL_LIB_NAME, abi % (kernel_abi, 1))

    def run_harness(self, *arguments):
        return subprocess.run(
            ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
             "--enable-native-access=ALL-UNNAMED",
             "-cp", os.pathsep.join([str(self.root), str(self.jar)]),
             "NativeBridgeHarness", *arguments],
            capture_output=True, text=True, timeout=60)

    def test_kernel_palette_collide_nbt_roundtrip(self):
        result = self.run_harness("kernel", str(require_native(self, KERNEL_LIB_NAME)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("KERNEL_OK", result.stdout)

    def test_mesh_section_and_vertex_packing_roundtrip(self):
        result = self.run_harness("mesh", str(require_native(self, MESH_LIB_NAME)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("MESH_OK", result.stdout)

    def test_loaders_fail_closed(self):
        result = self.run_harness("failclosed", str(self.fixtures))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("FAILCLOSED_OK", result.stdout)

    def test_jar_contains_native_bridge_and_no_stubs_and_is_reproducible(self):
        second = self.root / "jar-second"
        subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                        "--out-dir", str(second)], check=True, capture_output=True)
        self.assertEqual(self.jar.read_bytes(), (second / "mc-spine-integrated.jar").read_bytes())
        with zipfile.ZipFile(self.jar) as archive:
            names = set(archive.namelist())
        for name in ("NativeBridgeException", "NativeKernelBridge", "NativeKernelBridge$Palette",
                     "NativeKernelBridge$Collision", "NativeLibrary", "NativeMeshBridge"):
            self.assertIn(f"net/mcspine/integrated/{name}.class", names)
        self.assertFalse(any(name.startswith(("net/fabricmc/", "net/neoforged/")) for name in names))


LIFECYCLE_FAKES = {
    "net/fabricmc/api/ClientModInitializer.java": """package net.fabricmc.api;
public interface ClientModInitializer { void onInitializeClient(); }
""",
    "net/fabricmc/fabric/api/event/Event.java": """package net.fabricmc.fabric.api.event;
public class Event<T> {
    public final java.util.List<T> handlers = new java.util.ArrayList<>();
    public void register(T handler) { handlers.add(handler); }
}
""",
    "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientLifecycleEvents.java": """package net.fabricmc.fabric.api.client.event.lifecycle.v1;
import net.fabricmc.fabric.api.event.Event;
public final class ClientLifecycleEvents {
    public static final Event<ClientStopping> CLIENT_STOPPING = new Event<>();
    @FunctionalInterface public interface ClientStopping { void onClientStopping(Object client); }
}
""",
    "net/fabricmc/fabric/api/client/networking/v1/ClientPlayConnectionEvents.java": """package net.fabricmc.fabric.api.client.networking.v1;
import net.fabricmc.fabric.api.event.Event;
public final class ClientPlayConnectionEvents {
    public static final Event<Disconnect> DISCONNECT = new Event<>();
    @FunctionalInterface public interface Disconnect { void onPlayDisconnect(Object handler, Object client); }
}
""",
    "net/neoforged/fml/common/Mod.java": """package net.neoforged.fml.common;
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)
public @interface Mod { String value(); }
""",
    "net/neoforged/bus/api/Event.java": """package net.neoforged.bus.api;
public abstract class Event { }
""",
    "net/neoforged/bus/api/IEventBus.java": """package net.neoforged.bus.api;
public interface IEventBus { <T extends Event> void addListener(java.util.function.Consumer<T> consumer); }
""",
    "net/neoforged/neoforge/common/NeoForge.java": """package net.neoforged.neoforge.common;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;
import net.neoforged.bus.api.Event;
import net.neoforged.bus.api.IEventBus;
public class NeoForge {
    public static final List<Consumer<?>> LISTENERS = new ArrayList<>();
    public static final IEventBus EVENT_BUS = new IEventBus() {
        public <T extends Event> void addListener(Consumer<T> consumer) { LISTENERS.add(consumer); }
    };
    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void post(Event event) {
        for (Consumer listener : LISTENERS) {
            try { listener.accept(event); } catch (ClassCastException ignored) { }
        }
    }
}
""",
    "net/neoforged/neoforge/event/GameShuttingDownEvent.java": """package net.neoforged.neoforge.event;
public class GameShuttingDownEvent extends net.neoforged.bus.api.Event { }
""",
    "net/neoforged/neoforge/client/event/ClientPlayerNetworkEvent.java": """package net.neoforged.neoforge.client.event;
public abstract class ClientPlayerNetworkEvent extends net.neoforged.bus.api.Event {
    public static class LoggingOut extends ClientPlayerNetworkEvent { }
}
""",
    "LifecycleHarness.java": """import java.nio.file.Files;
import java.nio.file.Path;
import net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientLifecycleEvents;
import net.fabricmc.fabric.api.client.networking.v1.ClientPlayConnectionEvents;
import net.mcspine.integrated.FabricIntegratedMod;
import net.mcspine.integrated.NeoForgeIntegratedMod;
import net.neoforged.neoforge.client.event.ClientPlayerNetworkEvent;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.GameShuttingDownEvent;

public final class LifecycleHarness {
    public static void main(String[] args) throws Exception {
        String loader = args[0];
        String event = args[1];
        int hooks;
        if (loader.equals("fabric")) {
            new FabricIntegratedMod().onInitializeClient();
            hooks = ClientLifecycleEvents.CLIENT_STOPPING.handlers.size()
                    + ClientPlayConnectionEvents.DISCONNECT.handlers.size();
        } else {
            new NeoForgeIntegratedMod();
            hooks = NeoForge.LISTENERS.size();
        }
        System.out.println("HOOKS " + hooks);
        Path info = Path.of(System.getProperty("mc.spine.integrated.gameDir"), "saves",
                System.getProperty("mc.spine.integrated.world"), "server.json");
        if (hooks == 0) {
            System.out.println("INERT " + Files.exists(info));
            return;
        }
        String json = Files.readString(info);
        long pid = Long.parseLong(json.replaceAll("(?s).*\\"pid\\": ?([0-9]+).*", "$1"));
        if (!ProcessHandle.of(pid).map(ProcessHandle::isAlive).orElse(false)) {
            throw new AssertionError("server not running before hook");
        }
        switch (event) {
            case "stopping" -> ClientLifecycleEvents.CLIENT_STOPPING.handlers.forEach(h -> h.onClientStopping(null));
            case "disconnect" -> ClientPlayConnectionEvents.DISCONNECT.handlers.forEach(h -> h.onPlayDisconnect(null, null));
            case "logout" -> NeoForge.post(new ClientPlayerNetworkEvent.LoggingOut());
            case "shutdown" -> NeoForge.post(new GameShuttingDownEvent());
            default -> throw new IllegalArgumentException(event);
        }
        long deadline = System.nanoTime() + 6_000_000_000L;
        while (ProcessHandle.of(pid).map(ProcessHandle::isAlive).orElse(false) && System.nanoTime() < deadline) {
            Thread.sleep(20);
        }
        System.out.println("STOPPED_BY_HOOK " + !ProcessHandle.of(pid).map(ProcessHandle::isAlive).orElse(false));
        System.out.flush();
    }
}
""",
}


class ThreeLauncherEasyInstallTests(unittest.TestCase):
    """Pins the 5 thin-JAR 3-launcher easy-install contracts (zero python3 runtime dep)."""

    def test_bootstrapper_self_install_and_auto_chmod_after_sha256(self):
        with tempfile.TemporaryDirectory(prefix="mc-spine-easy-inst-") as td:
            root = Path(td)
            stage = root / "stage"
            subprocess.run(
                [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
                 "--out-dir", str(stage), "--mc-version", "1.21.1", "--build-jar"],
                check=True, capture_output=True, text=True,
            )
            jar = stage / "mc-spine-bootstrapper.jar"
            self.assertLess(jar.stat().st_size, 32 * 1024)
            with zipfile.ZipFile(jar) as zf:
                self.assertNotIn("integrated_bridge.py", zf.namelist())

            mc_dir = root / "dot_minecraft"
            mc_dir.mkdir()
            (mc_dir / "launcher_profiles.json").write_text(
                json.dumps({"profiles": {"existing": {"name": "Vanilla"}}, "version": 3}, indent=2)
            )
            res = subprocess.run(
                ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
                 "-jar", str(jar), "--install", "--minecraft-dir", str(mc_dir)],
                capture_output=True, text=True,
            )
            self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
            self.assertTrue((mc_dir / "libraries/net/mcspine/bootstrapper/1.0.0/bootstrapper-1.0.0.jar").is_file())
            self.assertTrue((mc_dir / "versions/mc-spine-1.21.1/mc-spine-1.21.1.json").is_file())
            profiles = json.loads((mc_dir / "launcher_profiles.json").read_text())
            self.assertIn("existing", profiles["profiles"])
            self.assertIn("mc-spine-1.21.1", profiles["profiles"])

            # Non-executable client binary: bad SHA fails closed WITHOUT setting +x; good SHA restores +x and runs.
            client = root / "client-stub"
            client.write_text("#!/bin/sh\nexit 0\n")
            client.chmod(0o644)
            good_sha = hashlib.sha256(client.read_bytes()).hexdigest()
            bad = subprocess.run(
                ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
                 "-jar", str(jar), "--client-bin", str(client),
                 "--client-sha256", "0" * 64, "--gameDir", str(root)],
                capture_output=True, text=True,
            )
            self.assertEqual(bad.returncode, 2)
            self.assertFalse(os.access(client, os.X_OK))
            ok = subprocess.run(
                ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
                 "-jar", str(jar), "--client-bin", str(client),
                 "--client-sha256", good_sha, "--gameDir", str(root)],
                capture_output=True, text=True,
            )
            self.assertEqual(ok.returncode, 0, ok.stderr + ok.stdout)
            self.assertTrue(os.access(client, os.X_OK))

    def test_mrpack_client_bin_config_and_deterministic_prism_zip(self):
        with tempfile.TemporaryDirectory(prefix="mc-spine-easy-bundles-") as td:
            root = Path(td)
            stage = root / "stage"
            subprocess.run(
                [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
                 "--out-dir", str(stage), "--mc-version", "1.21.1", "--build-jar"],
                check=True, capture_output=True, text=True,
            )
            jar = stage / "mc-spine-bootstrapper.jar"
            fake_bin = root / "mc-spine-client"
            fake_bin.write_text("#!/bin/sh\nexit 0\n")
            fake_bin.chmod(0o755)

            out_a, out_b = root / "out-a", root / "out-b"
            for out in (out_a, out_b):
                subprocess.run(
                    [sys.executable, str(TOOLS / "export_launcher_bundle.py"),
                     "--out-dir", str(out), "--format", "all", "--mc-version", "1.21.1",
                     "--loader", "fabric", "--loader-version", "0.16.9",
                     "--bootstrap-jar", str(jar), "--client-bin", str(fake_bin),
                     "--prism-zip"],
                    check=True, capture_output=True, text=True,
                )
            prism_a = out_a / "mc-spine-prism-1.21.1.zip"
            prism_b = out_b / "mc-spine-prism-1.21.1.zip"
            self.assertEqual(prism_a.read_bytes(), prism_b.read_bytes())
            with zipfile.ZipFile(prism_a) as zf:
                self.assertIn("patches/net.mcspine.bootstrapper.json", zf.namelist())
                self.assertIn(".minecraft/bin/mc-spine-client", zf.namelist())
                for info in zf.infolist():
                    self.assertEqual(info.date_time, (1980, 1, 1, 0, 0, 0))

            mrpack = out_a / "mc-spine-1.21.1.mrpack"
            with zipfile.ZipFile(mrpack) as zf:
                names = set(zf.namelist())
                self.assertIn("overrides/bin/mc-spine-client", names)
                self.assertIn("overrides/config/mc-spine-integrated.properties", names)
                cfg = zf.read("overrides/config/mc-spine-integrated.properties").decode()
                self.assertIn("enabled=true", cfg)
                self.assertIn("serverBin=bin/mc-spine-client", cfg)
                import io
                with zipfile.ZipFile(io.BytesIO(zf.read("overrides/mods/mc-spine-integrated.jar"))) as mod_jar:
                    fabric = json.loads(mod_jar.read("fabric.mod.json"))
                    self.assertEqual(fabric["license"], "LGPL-3.0-or-later")


if __name__ == "__main__":
    unittest.main()
