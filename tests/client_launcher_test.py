"""Real Java 21/bootstrapper and Prism wrapper integration checks."""
import json
import os
from pathlib import Path
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
Path(os.environ['TEST_NATIVE_LOG']).write_text(json.dumps(sys.argv[1:]))
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
        result, log = self.java(*args, extra_env={"MC_SPINE_HANDOFF_MANIFEST": str(manifest),
                                                  "TEST_NATIVE_EXIT": "37"})
        self.assertEqual(result.returncode, 37, result.stderr)
        self.assertEqual(json.loads(log.read_text()), list(args))
        summary = json.loads(manifest.read_text())
        self.assertEqual(summary["mode"], "wait")
        self.assertEqual(summary["argumentCount"], len(args))
        self.assertGreater(summary["pid"], 0)
        self.assertNotIn("private-secret", manifest.read_text())

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
        env.update(MC_SPINE_HANDOFF_MANIFEST=str(manifest), TEST_NATIVE_EXIT="37")
        result, log = self.java("--mc-spine-integrated", "--gameDir", str(game),
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
        result, log = self.invocation(sys.executable, str(TOOLS / "integrated_bridge.py"),
                                      "--game-dir", str(game), "--world", world.name,
                                      "--", str(self.stub), "--username", "Alex", extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()),
                         ["--username", "Alex", "--quickPlayMultiplayer",
                          self.assert_server_stopped(world)])

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


if __name__ == "__main__":
    unittest.main()
