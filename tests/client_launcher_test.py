"""Real Java 21/bootstrapper, Prism, Modrinth and native bridge checks."""
from concurrent.futures import ThreadPoolExecutor
import configparser
from contextlib import contextmanager
import gzip
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from threading import Thread
import time
import unittest
import zipfile
import tomllib

from wire_smoke import status


ROOT = Path(__file__).resolve().parents[1]
JAVA_EXECUTABLE = (
    shutil.which("java")
    or str(Path(tempfile.gettempdir()) / "megu-test-jdk" / "bin" /
           ("java.exe" if os.name == "nt" else "java"))
)
TOOLS = ROOT / "tools/client_launcher"


class LauncherTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="megu-client-launcher-")
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
    if os.environ.get('TEST_INTEGRATED_PROTOCOL'):
        sys.path.insert(0, os.environ['TEST_WIRE_MODULE'])
        from wire_smoke import status
        status(int(port))
    else:
        with socket.create_connection((host, int(port)), timeout=1):
            pass
Path(os.environ['TEST_NATIVE_LOG']).write_text(json.dumps(sys.argv[1:]))
if os.environ.get('TEST_NATIVE_ENV_LOG'):
    Path(os.environ['TEST_NATIVE_ENV_LOG']).write_text(json.dumps({
        'MEGU_LAUNCHER_ACCESS_TOKEN': os.environ.get('MEGU_LAUNCHER_ACCESS_TOKEN'),
        'MEGU_TOKEN_ENV': os.environ.get('MEGU_TOKEN_ENV'),
        'MEGU_HANDOFF_JSON': os.environ.get('MEGU_HANDOFF_JSON'),
        'MEGU_VANILLA_JAR': os.environ.get('MEGU_VANILLA_JAR'),
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
host = os.environ['MEGU_BIND_ADDR']
port = int(os.environ['MEGU_PORT'])
with socket.socket() as listener:
    time.sleep(0.2)
    listener.bind((host, port))
    listener.listen()
    Path('server.json').write_text(json.dumps({
        'pid': os.getpid(), 'port': port, 'bind': host, 'cwd': os.getcwd(),
        'world': os.environ['MEGU_WORLD_DIR'],
        'rcon': os.environ.get('MEGU_RCON_PORT'),
        'vanilla_jar': os.environ.get('MEGU_VANILLA_JAR'),
    }))
    print(f'megu stub listening {host}:{port} (test)', flush=True)
    while True:
        conn, _ = listener.accept()
        conn.close()
""")
        cls.server.chmod(0o755)
        cls.generate(cls.jar_dir, "--build-jar")
        cls.native_path = cls.root / "native-path"
        cls.native_path.mkdir()
        if os.name != "nt":
            (cls.native_path / "python3").symlink_to(sys.executable)

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
        env = dict(os.environ, MEGU_CLIENT_BIN=str(self.stub), MEGU_OFFLINE="1", TEST_NATIVE_LOG=str(log))
        env.pop("MEGU_DETACH", None)
        env.pop("MEGU_HANDOFF_MANIFEST", None)
        env.pop("MEGU_HANDOFF_FILE", None)
        env.pop("MEGU_VANILLA_JAR", None)
        if extra_env:
            env.update(extra_env)
        # In detach mode the native child inherits stderr, so capturing that
        # pipe would make subprocess.run wait for the child after Java exits.
        stderr = subprocess.DEVNULL if "--megu-detach" in args else subprocess.PIPE
        result = subprocess.run(args, env=env, stdout=subprocess.DEVNULL,
                                stderr=stderr, text=True, timeout=20)
        return result, log

    def java(self, *args, extra_env=None):
        return self.invocation("java", "-Djava.io.tmpdir=" + tempfile.gettempdir(),
                               "-XX:-UsePerfData",
                               "-jar", str(self.jar_dir / "megu-bootstrapper.jar"),
                               *args, extra_env=extra_env)

    def integrated_fixture(self):
        game = self.root / self._testMethodName / "game with spaces"
        env = {"MEGU_SERVER_BIN": str(self.server), "MEGU_RCON_PORT": "25575",
               "MEGU_INTEGRATED_WORLD": "Loopback World", "TEST_INTEGRATED_CONNECT": "1"}
        return game, game / "saves" / env["MEGU_INTEGRATED_WORLD"], env

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
        first = self.jar_dir / "megu-bootstrapper.jar"
        second = other / "megu-bootstrapper.jar"
        self.assertLess(first.stat().st_size, 32 * 1024)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        version = other / "versions/megu-1.21.1/megu-1.21.1.json"
        profile = json.loads(version.read_text())
        self.assertEqual(profile["mainClass"], "net.megu.launcher.Main")
        self.assertEqual(profile["inheritsFrom"], "1.21.1")
        self.assertEqual(profile["id"], "megu-1.21.1")
        self.assertEqual(profile["libraries"], [{"name": "net.megu:bootstrapper:1.0.0"}])
        library = other / "libraries/net/megu/bootstrapper/1.0.0/bootstrapper-1.0.0.jar"
        self.assertEqual(library.read_bytes(), second.read_bytes())
        self.assertEqual(profile["arguments"]["jvm"],
                         [f"-Dmegu.clientBin={self.stub.resolve()}"])

    def test_game_arguments_preserved_verbatim_and_exit_code(self):
        args = ("--username", "Steve", "--uuid", "00000000-0000-0000-0000-000000000001",
                "--accessToken", "private-secret", "--userType", "msa", "--version", "1.21.1",
                "--gameDir", "/folder with spaces", "--assetsDir", "/other/assets",
                "--assetIndex", "17", "--width", "1920", "--height", "1080",
                "--quickPlayMultiplayer", "test.example:25565", "--server", "localhost",
                "--port", "25565", "--unknownFlag", "value with spaces")
        manifest = self.root / "handoff.json"
        env_log = self.root / "handoff-env.json"
        result, log = self.java(*args, extra_env={"MEGU_HANDOFF_MANIFEST": str(manifest),
                                                  "TEST_NATIVE_ENV_LOG": str(env_log),
                                                  "TEST_NATIVE_EXIT": "37"})
        self.assertEqual(result.returncode, 37, result.stderr)
        self.assertEqual(json.loads(log.read_text()), list(args))
        self.assertFalse(manifest.exists())
        handoff = json.loads(json.loads(env_log.read_text())["MEGU_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "wait")
        self.assertEqual(handoff["argumentCount"], len(args))
        self.assertNotIn("private-secret", json.dumps(handoff))

    def test_handoff_manifest_file_requires_explicit_legacy_flag(self):
        args = ("--username", "Steve", "--port", "25565")
        manifest = self.root / "legacy-handoff.json"
        result, _ = self.java(*args, extra_env={"MEGU_HANDOFF_MANIFEST": str(manifest)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(manifest.exists())
        result, _ = self.java("--megu-handoff-file", *args,
                              extra_env={"MEGU_HANDOFF_MANIFEST": str(manifest)})
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(manifest.read_text())
        self.assertEqual(summary["mode"], "wait")
        self.assertEqual(summary["argumentCount"], len(args))
        self.assertGreater(summary["pid"], 0)
        manifest.unlink()
        cli_manifest = self.root / "cli-handoff.json"
        result, _ = self.java(f"--megu-handoff-manifest={cli_manifest}", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(cli_manifest.read_text())["mode"], "wait")

    def test_launch_leaves_zero_handoff_temp_files(self):
        game = self.root / "handoff game dir"
        game.mkdir()
        env_log = self.root / "handoff-zero-env.json"
        watched = [game, Path(tempfile.gettempdir())]
        before = {d: set(d.glob(".megu-handoff-*")) for d in watched}
        result, log = self.java("--username", "Steve", "--gameDir", str(game),
                                "--accessToken", "private-secret",
                                extra_env={"TEST_NATIVE_ENV_LOG": str(env_log),
                                           "MEGU_HANDOFF_MANIFEST": str(game / "handoff.json"),
                                           "MEGU_HANDOFF_FILE": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()),
                         ["--username", "Steve", "--gameDir", str(game),
                          "--accessToken", "private-secret"])
        for directory in watched:
            self.assertEqual(before[directory], set(directory.glob(".megu-handoff-*")),
                             f"unexpected .megu-handoff-* files in {directory}")
        self.assertFalse((game / "handoff.json").exists())
        handoff = json.loads(json.loads(env_log.read_text())["MEGU_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "wait")
        self.assertEqual(handoff["argumentCount"], 6)
        self.assertNotIn("private-secret", json.dumps(handoff))

    def test_detach_does_not_wait_and_removes_only_private_flag(self):
        started = time.monotonic()
        result, log = self.java("--username", "Steve", "--megu-detach", "--port", "25565",
                                extra_env={"TEST_NATIVE_SLEEP": "2.0"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 1.5)
        for _ in range(100):
            if log.is_file():
                break
            time.sleep(0.01)
        self.assertEqual(json.loads(log.read_text()), ["--username", "Steve", "--port", "25565"])

    def test_detach_environment(self):
        result, log = self.java("--uuid", "xyz", extra_env={"MEGU_DETACH": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--uuid", "xyz"])

    def test_game_directory_binary_resolution(self):
        folder = self.root / "game with spaces" / "bin"
        folder.mkdir(parents=True)
        import shutil
        shutil.copyfile(self.stub, folder / "megu-client")
        (folder / "megu-client").chmod(0o755)
        result, log = self.java("--gameDir", str(folder.parent), "--username", "Alex",
                                extra_env={"MEGU_CLIENT_BIN": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--gameDir", str(folder.parent),
                                                      "--username", "Alex"])

    def test_missing_native_binary_has_exit_code_two(self):
        result, _ = self.java("--username", "Steve",
                              extra_env={"MEGU_CLIENT_BIN": str(self.root / "absent")})
        self.assertEqual(result.returncode, 2)
        self.assertIn("not found", result.stderr)

    def release_archive(self, extra=None, include_server=True):
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode="w:gz", format=tarfile.USTAR_FORMAT) as tar:
            members = [("release/prefix/bin/megu-client-gpu", self.stub)]
            if include_server:
                members.append(("release/prefix/bin/megu", self.server))
            for name, source in members:
                payload = source.read_bytes()
                entry = tarfile.TarInfo(name)
                entry.size = len(payload)
                entry.mode = 0o644  # The launcher sets executable permissions.
                tar.addfile(entry, io.BytesIO(payload))
            if extra is not None:
                tar.addfile(extra)
        return data.getvalue()

    @contextmanager
    def local_release(self, archive=None, *, checksums=None, tag="v0.1.0-alpha.5", archive_redirect=None):
        arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "arm64",
                "aarch64": "arm64"}.get(platform.machine().lower())
        target = {"Linux": "linux", "Darwin": "macos"}.get(platform.system())
        if arch is None or target is None:
            self.skipTest("requires a release target that runs POSIX executable stubs")
        archive = self.release_archive() if archive is None else archive
        name = f"megu-{tag}-{target}-{arch}.tar.gz"
        sums_path = f"/releases/download/{tag}/SHA256SUMS-all.txt"
        archive_path = f"/releases/download/{tag}/{name}"
        if checksums is None:
            checksums = f"{hashlib.sha256(archive).hexdigest()}  {{name}}\n"
        routes = {sums_path: checksums.format(name=name).encode(), archive_path: archive}
        requests = []

        class ReleaseHandler(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                if archive_redirect is not None and self.path == archive_path:
                    self.send_response(302)
                    self.send_header("Location", archive_redirect)
                    self.end_headers()
                    return
                body = routes.get(self.path)
                if body is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        with ThreadingHTTPServer(("127.0.0.1", 0), ReleaseHandler) as server:
            thread = Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02})
            thread.start()
            try:
                yield (f"http://127.0.0.1:{server.server_port}/releases/download/{tag}",
                       requests, [sums_path, archive_path])
            finally:
                server.shutdown()
                thread.join()

    def prism_java(self, game, base_url, *args, vanilla=None, jvm=(), extra_env=None):
        jar = self.jar_dir / "megu-bootstrapper.jar"
        classpath = os.pathsep.join(str(path) for path in (jar, vanilla) if path is not None)
        home = game.parent / "test-home"
        return self.invocation(
            shutil.which("java"), "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
            f"-Duser.home={home}", *jvm, "-cp", classpath, "net.megu.launcher.Main",
            "--username", "Alex", "--version", "1.21.1", "--gameDir", str(game),
            "--assetsDir", str(game.parent / "assets"), "--assetIndex", "17",
            "--uuid", "00000000-0000-0000-0000-000000000000", "--accessToken", "test-token",
            "--userType", "msa", "--versionType", "release", *args,
            extra_env={"MEGU_CLIENT_BIN": "", "MEGU_CLIENT_SHA256": "", "MEGU_OFFLINE": "0",
                       "MEGU_NO_AUTO_FETCH": "0", "MEGU_RELEASE_BASE_URL": base_url,
                       "PATH": str(self.native_path), **(extra_env or {})})

    def assert_no_staged_binaries(self, game):
        bin_dir = game / "bin"
        self.assertFalse((bin_dir / "megu-client-gpu").exists())
        self.assertFalse((bin_dir / "megu").exists())
        self.assertFalse(list(bin_dir.glob(".megu-bootstrap-*")))
        self.assertFalse(list(bin_dir.glob("*.part")))

    def test_prism_first_launch_downloads_verified_binaries_and_reuses_them(self):
        game = self.root / self._testMethodName / "instance with spaces" / "minecraft"
        vanilla = game.parents[1] / "libraries/com/mojang/minecraft/1.21.1/minecraft-1.21.1-client.jar"
        vanilla.parent.mkdir(parents=True)
        with zipfile.ZipFile(vanilla, "w"):
            pass
        env_log = vanilla.parent / "env.json"
        env = {"TEST_NATIVE_EXIT": "37", "TEST_NATIVE_ENV_LOG": str(env_log)}
        with self.local_release(tag="v0.1.0-alpha.8") as (url, requests, expected):
            first, log = self.prism_java(game, url, vanilla=vanilla, extra_env=env)
            self.assertEqual(first.returncode, 37, first.stderr)
            self.assertIn("Downloading native client", first.stderr)
            self.assertIn("Verified SHA-256", first.stderr)
            self.assertIn("Launching megu-client-gpu", first.stderr)
            self.assertNotIn("test-token", first.stderr)
            self.assertEqual(requests, expected)
            self.assertIn(str(game), json.loads(log.read_text()))
            self.assertEqual(json.loads(env_log.read_text())["MEGU_VANILLA_JAR"], str(vanilla))
            for name, source in (("megu-client-gpu", self.stub), ("megu", self.server)):
                installed = game / "bin" / name
                self.assertEqual(installed.read_bytes(), source.read_bytes())
                self.assertEqual(stat.S_IMODE(installed.stat().st_mode), 0o755)

            requests.clear()
            second, _ = self.prism_java(game, url, vanilla=vanilla, extra_env=env)
            self.assertEqual(second.returncode, 37, second.stderr)
            self.assertNotIn("Downloading", second.stderr)
            self.assertEqual(requests, [])

    def test_prism_bootstrap_installs_once_for_concurrent_launches(self):
        game = self.root / self._testMethodName / "minecraft"
        with self.local_release() as (url, requests, expected):
            with ThreadPoolExecutor(max_workers=2) as pool:
                launches = [pool.submit(self.prism_java, game, url) for _ in range(2)]
                for launch in launches:
                    result, _ = launch.result()
                    self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(requests, expected)

    def test_prism_bootstrap_recovers_an_interrupted_install_with_only_the_server(self):
        game = self.root / self._testMethodName / "minecraft"
        bin_dir = game / "bin"
        bin_dir.mkdir(parents=True)
        (bin_dir / ".megu-bootstrap.lock").touch()
        server = bin_dir / "megu"
        server.write_text("#!/bin/sh\nexit 19\n")
        server.chmod(0o755)
        with self.local_release() as (url, requests, expected):
            result, _ = self.prism_java(game, url)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(requests, expected)
            self.assertEqual(server.read_bytes(), self.server.read_bytes())

    def test_prism_bootstrap_rejects_archive_sha256_mismatch_and_can_retry(self):
        game = self.root / self._testMethodName / "minecraft"
        checksums = f"{'0' * 64}  {{name}}\n"
        with self.local_release(checksums=checksums) as (url, requests, expected):
            result, log = self.prism_java(game, url)
            self.assertEqual(result.returncode, 2)
            self.assertIn("native archive SHA-256 mismatch", result.stderr)
            self.assertIn("megu-installer", result.stderr)
            self.assertNotIn("Verified SHA-256", result.stderr)
            self.assertEqual(requests, expected)
            self.assertFalse(log.exists())
            self.assert_no_staged_binaries(game)
        with self.local_release() as (url, requests, expected):
            result, _ = self.prism_java(game, url)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(requests, expected)

    def test_prism_bootstrap_rejects_unsafe_tar_members_after_valid_binaries(self):
        symlink = tarfile.TarInfo("release/link")
        symlink.type = tarfile.SYMTYPE
        symlink.linkname = "megu-client-gpu"
        for index, (entry, message) in enumerate((
                (tarfile.TarInfo("../evil"), "unsafe path"),
                (tarfile.TarInfo("/evil"), "unsafe path"),
                (symlink, "unsupported entry type"))):
            with self.subTest(entry=entry.name):
                game = self.root / self._testMethodName / str(index) / "minecraft"
                with self.local_release(self.release_archive(entry)) as (url, requests, expected):
                    result, _ = self.prism_java(game, url)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Verified SHA-256", result.stderr)
                    self.assertIn(message, result.stderr)
                    self.assertEqual(requests, expected)
                    self.assert_no_staged_binaries(game)
                    self.assertFalse((game / "evil").exists())

    def test_prism_bootstrap_requires_exact_checksum_and_both_binaries(self):
        game = self.root / self._testMethodName / "minecraft"
        checksums = f"{'0' * 64}  {{name}}.bak\n"
        with self.local_release(checksums=checksums) as (url, requests, expected):
            result, _ = self.prism_java(game, url)
            self.assertEqual(result.returncode, 2)
            self.assertIn("missing SHA-256", result.stderr)
            self.assertEqual(requests, expected[:1])
            self.assert_no_staged_binaries(game)

        with self.local_release(self.release_archive(include_server=False)) as (url, requests, expected):
            result, _ = self.prism_java(game, url)
            self.assertEqual(result.returncode, 2)
            self.assertIn("native archive missing megu", result.stderr)
            self.assertEqual(requests, expected)
            self.assert_no_staged_binaries(game)

    def test_prism_bootstrap_rejects_oversized_tar_entry_before_reading_payload(self):
        game = self.root / self._testMethodName / "minecraft"
        header = tarfile.TarInfo("release/big-file")
        header.size = 256 * 1024 * 1024 + 1
        archive = gzip.compress(header.tobuf(format=tarfile.USTAR_FORMAT))
        with self.local_release(archive) as (url, requests, expected):
            result, _ = self.prism_java(game, url)
            self.assertEqual(result.returncode, 2)
            self.assertIn("oversized entry in native tar", result.stderr)
            self.assertEqual(requests, expected)
            self.assert_no_staged_binaries(game)

    def test_prism_bootstrap_rejects_non_loopback_redirect(self):
        game = self.root / self._testMethodName / "minecraft"
        with self.local_release(archive_redirect="http://example.invalid:80/asset") as (url, requests, expected):
            result, _ = self.prism_java(game, url)
            self.assertEqual(result.returncode, 2)
            self.assertIn("blocked host or protocol", result.stderr)
            self.assertEqual(requests, expected)
            self.assert_no_staged_binaries(game)

    def test_prism_bootstrap_network_error_reports_manual_install(self):
        game = self.root / self._testMethodName / "minecraft"
        with self.local_release() as (url, requests, _):
            result, _ = self.prism_java(game, url + "/absent")
            self.assertEqual(result.returncode, 2)
            self.assertIn("fetching SHA256SUMS-all.txt returned HTTP 404", result.stderr)
            self.assertIn("megu-installer", result.stderr)
            self.assertEqual(len(requests), 1)
            self.assert_no_staged_binaries(game)

    def test_prism_bootstrap_respects_offline_and_explicit_client_paths(self):
        game = self.root / self._testMethodName / "minecraft"
        missing = str(game / "missing")
        with self.local_release() as (url, requests, _):
            for args, jvm, env in (
                    ((), (), {"MEGU_OFFLINE": "1"}),
                    ((), (), {"MEGU_NO_AUTO_FETCH": "1"}),
                    (("--client-bin", missing), (), {}),
                    ((), (), {"MEGU_CLIENT_BIN": missing}),
                    ((), (f"-Dmegu.clientBin={missing}",), {})):
                with self.subTest(args=args, jvm=jvm, env=env):
                    result, _ = self.prism_java(game, url, *args, jvm=jvm, extra_env=env)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("native client not found", result.stderr)
            self.assertEqual(requests, [])
            self.assertFalse((game / "bin").exists())

    @unittest.skipIf(os.name == "nt", "uses POSIX executable stubs")
    def test_launcher_passes_official_classpath_jar_to_both_integrated_children(self):
        game, world, env = self.integrated_fixture()
        vanilla = game.parent / "versions/1.21.1/1.21.1.jar"
        vanilla.parent.mkdir(parents=True)
        with zipfile.ZipFile(vanilla, "w"):
            pass
        env_log = vanilla.parent / "env.json"
        env.update(MEGU_CLIENT_BIN=str(self.stub), TEST_NATIVE_ENV_LOG=str(env_log), MEGU_OFFLINE="1")
        for override in (None, str(game.parent / "explicit.jar"), ""):
            with self.subTest(override=override):
                if override is not None:
                    env["MEGU_VANILLA_JAR"] = override
                result, _ = self.prism_java(game, "", "--megu-integrated", "--version=megu-1.21.1",
                                             vanilla=vanilla, extra_env=env)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = str(vanilla) if override is None else override
                self.assertEqual(json.loads(env_log.read_text())["MEGU_VANILLA_JAR"], expected)
                self.assertEqual(json.loads((world / "server.json").read_text())["vanilla_jar"], expected)

    @unittest.skipIf(os.name == "nt", "uses executable shell stubs")
    def test_prism_discovers_standard_unix_client_before_path(self):
        root = self.root / "prism-binary-discovery"
        home = root / "home"
        game = root / "instance/.minecraft"
        bindir = root / "path"
        env = {"MEGU_CLIENT_BIN": "", "MEGU_CLIENT_SHA256": "", "PATH": str(bindir)}
        java = [shutil.which("java"), "-Djava.io.tmpdir=" + tempfile.gettempdir(),
                "-XX:-UsePerfData", "-Dos.name=Linux", f"-Duser.home={home}"]
        command = ["-jar", str(self.jar_dir / "megu-bootstrapper.jar"),
                   "--gameDir", str(game)]
        candidates = [game / "bin", home / ".minecraft/bin", home / ".local/share/megu/bin",
                      home / ".local/bin", bindir]
        for index, directory in enumerate(candidates):
            directory.mkdir(parents=True)
            binary = directory / "megu-client"
            binary.write_text(f"#!/bin/sh\nexit {10 + index}\n")
            binary.chmod(0o755)

        self.assertEqual(self.invocation(*java, *command, extra_env=env)[0].returncode, 10)
        (candidates[0] / "megu-client").unlink()
        absent = str(root / "absent")
        for jvm, flags, overrides in (
                ([], ["--client-bin", absent], {}),
                ([], [], {"MEGU_CLIENT_BIN": absent}),
                ([f"-Dmegu.clientBin={absent}"], [], {})):
            with self.subTest(jvm=jvm, flags=flags, overrides=overrides):
                result, _ = self.invocation(*java, *jvm, *command, *flags,
                                            extra_env={**env, **overrides})
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("not found", result.stderr)
        result, _ = self.invocation(*java, *command, "--client-sha256", "0" * 64, extra_env=env)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("SHA-256 mismatch", result.stderr)

        for index, directory in enumerate(candidates[1:], start=11):
            with self.subTest(directory=directory):
                result, _ = self.invocation(*java, *command, extra_env=env)
                self.assertEqual(result.returncode, index, result.stderr)
                # Standard-directory discovery should skip non-executable files.
                (directory / "megu-client").chmod(0o644)

    @unittest.skipIf(os.name == "nt", "uses a simulated Windows OS and POSIX file modes")
    def test_prism_discovers_windows_localappdata_client(self):
        root = self.root / "prism-windows-discovery"
        appdata = root / "appdata"
        env = {"MEGU_CLIENT_BIN": "", "PATH": str(root / "empty-path"),
               "APPDATA": str(root / "roaming"), "LOCALAPPDATA": str(appdata)}
        for directory in ("bin", "current"):
            with self.subTest(directory=directory):
                installed = appdata / "Megu" / directory / "megu-client-gpu.exe"
                installed.parent.mkdir(parents=True)
                installed.write_bytes(b"Windows executable stub")
                installed.chmod(0o755)
                # Stop at SHA-256 verification: a Windows binary cannot run on this host.
                result, _ = self.invocation(
                    shutil.which("java"), "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
                    "-Dos.name=Windows 11", f"-Duser.home={root / 'home'}",
                    "-jar", str(self.jar_dir / "megu-bootstrapper.jar"), "--gameDir", str(root / "game"),
                    "--client-sha256", "0" * 64, extra_env=env)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("SHA-256 mismatch", result.stderr)
                installed.unlink()

    def test_integrated_injects_loopback_and_stops_on_client_exit(self):
        game, world, env = self.integrated_fixture()
        manifest = game / "handoff.json"
        env_log = game.parent / "handoff-env.json"
        env.update(MEGU_HANDOFF_MANIFEST=str(manifest), TEST_NATIVE_EXIT="37",
                   TEST_NATIVE_ENV_LOG=str(env_log))
        result, log = self.java("--megu-handoff-file", "--megu-integrated",
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
        handoff = json.loads(json.loads(env_log.read_text())["MEGU_HANDOFF_JSON"])
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
        env["MEGU_CLIENT_BIN"] = str(broken)
        result, log = self.java("--megu-integrated", f"--gameDir={game}", extra_env=env)
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
        handoff = json.loads(json.loads(env_log.read_text())["MEGU_HANDOFF_JSON"])
        self.assertEqual(handoff["mode"], "integrated")
        self.assertEqual(handoff["endpoint"], endpoint)
        self.assertEqual(handoff["argumentCount"], 4)

    def test_integrated_stops_when_java_wrapper_exits_abruptly(self):
        game, world, env = self.integrated_fixture()
        client_log = game.parent / "client.json"
        env = dict(os.environ, **env, MEGU_CLIENT_BIN=str(self.stub),
                   TEST_NATIVE_LOG=str(client_log), TEST_NATIVE_SLEEP="30", MEGU_DETACH="0")
        env.pop("MEGU_HANDOFF_MANIFEST", None)
        command = ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData", "-jar",
                   str(self.jar_dir / "megu-bootstrapper.jar"),
                   "--megu-integrated", "--gameDir", str(game)]
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
        wrapper = str(TOOLS / "megu-wrapper")
        args = ("--username", "Steve", "--uuid", "0-1-2", "--unknown", "as-is")
        result, log = self.invocation(wrapper, "/usr/bin/java", "-Xmx4G", "-Dfoo=bar",
                                      "-cp", "a.jar:b.jar", "--add-opens",
                                      "java.base/java.lang=ALL-UNNAMED", "-XX:+UseG1GC",
                                      "net.minecraft.client.main.Main", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), list(args))

    def test_wrapper_jar_mode_and_native_exit_code(self):
        result, log = self.invocation(str(TOOLS / "megu-wrapper"), "java", "-Xms1G",
                                      "-jar", "minecraft.jar", "--username", "Alex",
                                      extra_env={"TEST_NATIVE_EXIT": "9"})
        self.assertEqual(result.returncode, 9, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--username", "Alex"])

    def test_java_integrated_real_server_and_native_protocol_handshake(self):
        server = ROOT / "target/debug/megu"
        if not server.is_file():
            self.skipTest("cargo build --offline --locked required for real protocol handshake")
        game = self.root / "cf4-real-integrated" / "game"
        game.mkdir(parents=True)
        env = {"MEGU_SERVER_BIN": str(server), "MEGU_SERVER_HOME": str(ROOT),
               "TEST_INTEGRATED_CONNECT": "1", "TEST_INTEGRATED_PROTOCOL": "1",
               "TEST_WIRE_MODULE": str(ROOT / "tests")}
        result, log = self.java("--megu-integrated", "--gameDir", str(game),
                                "--username", "Steve", "--accessToken", "super-secret-token",
                                "--version", "1.21.1", "--assetIndex", "17", extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("super-secret-token", result.stderr or "")
        args = json.loads(log.read_text())
        self.assertIn("--quickPlayMultiplayer", args)
        self.assertEqual(args[args.index("--accessToken") + 1], "super-secret-token")
        world = game / "saves/megu"
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
            JAVA_EXECUTABLE,
            "-Djava.io.tmpdir=" + tempfile.gettempdir(),
            "-Dmegu.clientSha256=" + valid_sha256,
            "-XX:-UsePerfData", "-jar",
            str(self.jar_dir / "megu-bootstrapper.jar"),
            "--accessToken", "super-secret-token",
            extra_env={"MEGU_CLIENT_SHA256": "0" * 64})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("super-secret-token", result.stderr or "")
        self.assertTrue(log.exists())

        result, log = self.invocation(
            JAVA_EXECUTABLE,
            "-Djava.io.tmpdir=" + tempfile.gettempdir(),
            "-Dmegu.clientSha256=" + "0" * 64,
            "-XX:-UsePerfData", "-jar",
            str(self.jar_dir / "megu-bootstrapper.jar"),
            "--accessToken", "super-secret-token",
            extra_env={"MEGU_CLIENT_SHA256": valid_sha256})
        self.assertEqual(result.returncode, 2)
        self.assertIn("SHA-256 mismatch", result.stderr)
        self.assertNotIn("super-secret-token", result.stderr)
        self.assertFalse(log.exists())

    def test_windows_exe_names_resolve_in_gamedir_bin_and_path(self):
        import shutil
        cases = []
        for index, (rel, name) in enumerate((
                ("", "megu-client.exe"), ("bin", "megu.exe"),
                ("bin", "megu-client-gpu.exe"), ("", "megu.exe"))):
            folder = self.root / f"exe-game-{index}" / ("gamedir with spaces" if index % 2 else "gamedir")
            target = folder / rel / name if rel else folder / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.stub, target)
            target.chmod(0o755)
            cases.append((folder, target))
        for folder, target in cases:
            with self.subTest(binary=target.name):
                result, log = self.java("--gameDir", str(folder), "--username", "Steve",
                                        extra_env={"MEGU_CLIENT_BIN": ""})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(log.read_text()),
                                 ["--gameDir", str(folder), "--username", "Steve"])
        bindir = self.root / "exe-path-bin"
        bindir.mkdir()
        shutil.copyfile(self.stub, bindir / "megu.exe")
        (bindir / "megu.exe").chmod(0o755)
        env_path = str(bindir) + os.pathsep + os.environ.get("PATH", "")
        result, log = self.invocation(
            "java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData",
            f"-Duser.home={self.root / 'exe-path-home'}",
            "-jar", str(self.jar_dir / "megu-bootstrapper.jar"), "--username", "PATHUser",
            extra_env={"MEGU_CLIENT_BIN": "", "PATH": env_path, "APPDATA": "", "LOCALAPPDATA": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()), ["--username", "PATHUser"])

    def test_token_env_flag_and_env_var_strip_access_token(self):
        env_log = self.root / "token-env.json"
        extra = {"TEST_NATIVE_ENV_LOG": str(env_log)}
        for label, token_args, env_extra in (
                ("flag-space", ("--accessToken", "flag-secret-token"), {"MEGU_TOKEN_ENV": ""}),
                ("flag-equals", ("--accessToken=equals-secret-token",), {"MEGU_TOKEN_ENV": ""}),
                ("env-var", ("--accessToken", "envvar-secret-token"), {"MEGU_TOKEN_ENV": "1"})):
            with self.subTest(mode=label):
                env_log.unlink(missing_ok=True)
                args = ["--username", "Steve", "--uuid", "u-1", *token_args,
                        "--gameDir", "/tmp/game dir", "--assetsDir", "/tmp/äöü assets",
                        "--assetIndex", "26.3"]
                if label != "env-var":
                    args.insert(0, "--megu-token-env")
                result, log = self.java(*args, extra_env={**extra, **env_extra})
                self.assertEqual(result.returncode, 0, result.stderr)
                forwarded = json.loads(log.read_text())
                self.assertNotIn("--accessToken", forwarded)
                self.assertNotIn("--megu-token-env", forwarded)
                self.assertEqual(forwarded, ["--username", "Steve", "--uuid", "u-1",
                                             "--gameDir", "/tmp/game dir",
                                             "--assetsDir", "/tmp/äöü assets",
                                             "--assetIndex", "26.3"])
                seen = json.loads(env_log.read_text())
                self.assertEqual(seen["MEGU_LAUNCHER_ACCESS_TOKEN"],
                                 {"flag-space": "flag-secret-token",
                                  "flag-equals": "equals-secret-token",
                                  "env-var": "envvar-secret-token"}[label])
                self.assertNotIn("secret-token", result.stderr or "")

    def test_access_token_stays_in_argv_without_token_env(self):
        env_log = self.root / "token-env-default.json"
        result, log = self.java("--username", "Steve", "--accessToken", "argv-secret-token",
                                extra_env={"TEST_NATIVE_ENV_LOG": str(env_log),
                                           "MEGU_TOKEN_ENV": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text()),
                         ["--username", "Steve", "--accessToken", "argv-secret-token"])
        self.assertIsNone(json.loads(env_log.read_text())["MEGU_LAUNCHER_ACCESS_TOKEN"])

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
        version = profile_dir / "versions/megu-26.3/megu-26.3.json"
        profile = json.loads(version.read_text())
        self.assertEqual(profile["inheritsFrom"], "26.3")
        self.assertEqual(profile["id"], "megu-26.3")
        self.assertEqual(profile["mainClass"], "net.megu.launcher.Main")
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
        cls.temporary = tempfile.TemporaryDirectory(prefix="megu-cf4-export-")
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
        cls.integrated = cls.root / "megu-integrated.jar"
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
        self.assertEqual(command[0], str((output / "megu-wrapper").resolve()))
        self.assertTrue(os.access(command[0], os.X_OK))
        self.assertEqual(command[1:], ["--client-sha256",
                                       hashlib.sha256(self.native.read_bytes()).hexdigest()])
        self.assertEqual((output / ".minecraft/mods/megu-integrated.jar").read_bytes(),
                         self.integrated.read_bytes())
        log = self.root / "prism-native.json"
        env = dict(os.environ, TEST_NATIVE_LOG=str(log))
        env.pop("MEGU_CLIENT_BIN", None)
        result = subprocess.run([*command, JAVA_EXECUTABLE, "-Xmx2G",
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
        name = "megu-1.21.1.mrpack"
        self.assertEqual((first / name).read_bytes(), (second / name).read_bytes())
        with zipfile.ZipFile(first / name) as archive:
            self.assertEqual(archive.namelist(), ["modrinth.index.json",
                             "overrides/mods/megu-integrated.jar"])
            index = json.loads(archive.read("modrinth.index.json"))
            self.assertEqual(index["formatVersion"], 1)
            self.assertEqual(index["game"], "minecraft")
            self.assertEqual(index["dependencies"], {"minecraft": "1.21.1",
                                                       "neoforge": "21.1.215"})
            self.assertEqual(index["files"], [])
            contents = archive.read("overrides/mods/megu-integrated.jar")
            self.assertEqual(contents, self.integrated.read_bytes())
            meta = index["megu"]["bundledOverrides"]["mods/megu-integrated.jar"]
            self.assertEqual(meta["fileSize"], len(contents))
            self.assertEqual(meta["hashes"], {
                "sha1": hashlib.sha1(contents).hexdigest(),
                "sha512": hashlib.sha512(contents).hexdigest()})
            for item in archive.infolist():
                self.assertEqual(item.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((item.external_attr >> 16) & 0o777, 0o644)
                self.assertEqual(item.create_system, 3)

    def test_wrapper_and_exporter_fail_closed(self):
        wrapper = str(TOOLS / "megu-wrapper")
        env = dict(os.environ, MEGU_CLIENT_BIN=str(self.root / "missing"))
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
        mesh = ROOT / "client-port/target/release/libmegu_client_mesh.so"
        if not mesh.is_file():
            self.skipTest("build client-port release cdylib to test ABI packaging")
        output = self.root / "mesh-bundle"
        self.export(output, "--format", "all", "--loader", "fabric",
                    "--loader-version", "0.16.10", "--integrated-jar",
                    str(self.integrated), "--mesh-library", str(mesh))
        self.assertEqual((output / "prism/native/libmegu_client_mesh.so").read_bytes(),
                         mesh.read_bytes())
        with zipfile.ZipFile(output / "megu-1.21.1.mrpack") as archive:
            content = archive.read("overrides/megu/native/libmegu_client_mesh.so")
            index = json.loads(archive.read("modrinth.index.json"))
            expected = index["megu"]["bundledOverrides"]["megu/native/libmegu_client_mesh.so"]
            self.assertEqual(expected["hashes"]["sha512"], hashlib.sha512(content).hexdigest())
            self.assertEqual(content, mesh.read_bytes())
        invalid_lib = self.root / "not-a-library.so"
        invalid_lib.write_bytes(b"missing ABI")
        invalid = self.export(self.root / "invalid-mesh", "--format", "prism",
                              "--mesh-library", str(invalid_lib), check=False)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("megu_mesh_abi_version", invalid.stderr)


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
        with zipfile.ZipFile(first / "megu-1.21.1.mrpack") as archive:
            self.assertEqual(archive.namelist(), [
                "modrinth.index.json",
                f"overrides/megu/native/{MESH_LIB_NAME}",
                f"overrides/megu/native/{KERNEL_LIB_NAME}",
                "overrides/mods/megu-integrated.jar"])
            index = json.loads(archive.read("modrinth.index.json"))
            content = archive.read(f"overrides/megu/native/{KERNEL_LIB_NAME}")
            self.assertEqual(content, kernel.read_bytes())
            meta = index["megu"]["bundledOverrides"][f"megu/native/{KERNEL_LIB_NAME}"]
            self.assertEqual(meta["fileSize"], len(content))
            self.assertEqual(meta["hashes"], {"sha1": hashlib.sha1(content).hexdigest(),
                                              "sha512": hashlib.sha512(content).hexdigest()})
            for item in archive.infolist():
                self.assertEqual(item.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((item.external_attr >> 16) & 0o777, 0o644)

    def test_kernel_library_fails_closed(self):
        fixtures = self.root / "kernel-fixtures"
        wrong_abi = compile_c_library(fixtures / "abi", KERNEL_LIB_NAME,
                                      "unsigned megu_kernel_abi_version(void) { return 2; }\n")
        no_symbol = compile_c_library(fixtures / "symbol", KERNEL_LIB_NAME,
                                      "unsigned megu_mesh_abi_version(void) { return 1; }\n")
        junk = fixtures / "junk.so"
        junk.write_bytes(b"missing ABI")
        for index, (library, reason) in enumerate((
                (wrong_abi, "unsupported kernel C ABI version 2, expected 1"),
                (no_symbol, "megu_kernel_abi_version"),
                (junk, "megu_kernel_abi_version"),
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
        jar = destination / "megu-bootstrapper.jar"
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
        self.assertIn("megu 26.3", config["General"]["name"])

    def test_mrpack_mc_version_263(self):
        output = self.root / "mrpack-263"
        self.export(output, "--format", "mrpack", "--loader", "fabric",
                    "--loader-version", "0.16.10", "--mc-version", "26.3",
                    "--integrated-jar", str(self.integrated))
        bundle = output / "megu-26.3.mrpack"
        self.assertTrue(bundle.is_file())
        with zipfile.ZipFile(bundle) as archive:
            index = json.loads(archive.read("modrinth.index.json"))
        self.assertEqual(index["dependencies"]["minecraft"], "26.3")
        self.assertEqual(index["name"], "megu 26.3")
        self.assertIn("26.3", index["summary"])

    def assert_prism_bootstrap(self, pack, patch, mc_version, preceding_components, jvm_args=None):
        requires = [{"uid": "net.minecraft", "equals": mc_version}]
        self.assertEqual(pack, {"formatVersion": 1, "components": [
            *preceding_components,
            {"uid": "net.megu.bootstrapper", "version": "1.0.0", "cachedName": "Megu",
             "cachedVersion": "1.0.0", "cachedRequires": requires},
        ]})
        expected_patch = {
            "formatVersion": 1, "uid": "net.megu.bootstrapper", "name": "Megu",
            "version": "1.0.0", "order": 5, "requires": requires,
            "mainClass": "net.megu.launcher.Main",
            "libraries": [{"name": "net.megu:bootstrapper:1.0.0", "MMC-hint": "local",
                           "MMC-filename": "bootstrapper-1.0.0.jar"}],
        }
        if jvm_args is not None:
            expected_patch["+jvmArgs"] = jvm_args
        self.assertEqual(patch, expected_patch)

    def test_prism_release_zip_layout_and_reproducibility(self):
        jar = self.build_bootstrap_jar(self.root / "prism-release-stage")
        args = ("--format", "prism", "--mc-version", "1.21.1", "--bootstrap-jar", str(jar),
                "--prism-zip")
        output = self.root / "prism-release"
        self.export(output, *args)
        path = output / "megu-prism-1.21.1.zip"
        first = path.read_bytes()
        with zipfile.ZipFile(path) as archive:
            names = archive.namelist()
            self.assertFalse([name for name in names if name.lower().endswith(".zip")], names)
            self.assertIsNone(archive.testzip())
            self.assertEqual(names, ["instance.cfg", "libraries/bootstrapper-1.0.0.jar",
                                     "mmc-pack.json", "patches/net.megu.bootstrapper.json"])
            self.assertEqual(archive.read("libraries/bootstrapper-1.0.0.jar"), jar.read_bytes())
            self.assert_prism_bootstrap(
                json.loads(archive.read("mmc-pack.json")),
                json.loads(archive.read("patches/net.megu.bootstrapper.json")), "1.21.1",
                [{"uid": "net.minecraft", "version": "1.21.1", "important": True}])
        # Reusing the output directory must not include the old archive or other root ZIPs.
        (output / "stale.ZIP").write_bytes(first)
        self.export(output, *args)
        self.assertEqual(path.read_bytes(), first)
        other = self.root / "prism-release-again"
        self.export(other, *args)
        self.assertEqual((other / path.name).read_bytes(), first)

    def test_prism_bootstrap_jar_native_patch_export(self):
        staging = self.root / "bootstrap-staging"
        jar = self.build_bootstrap_jar(staging)
        output = self.root / "prism bootstrap patch"
        digest = hashlib.sha256(self.native.read_bytes()).hexdigest()
        args = ("--format", "prism", "--mc-version", "26.3", "--loader", "fabric",
                "--loader-version", "0.16.10", "--integrated-jar", str(self.integrated),
                "--bootstrap-jar", str(jar), "--client-bin", str(self.native),
                "--client-sha256", digest)
        self.export(output, *args)
        patch = json.loads((output / "patches/net.megu.bootstrapper.json").read_text())
        pack = json.loads((output / "mmc-pack.json").read_text())
        self.assert_prism_bootstrap(pack, patch, "26.3", [
            {"uid": "net.minecraft", "version": "26.3", "important": True},
            {"uid": "net.fabricmc.fabric-loader", "version": "0.16.10"},
        ], [f"-Dmegu.clientSha256={digest}"])
        self.assertEqual((output / "libraries/bootstrapper-1.0.0.jar").read_bytes(), jar.read_bytes())
        self.assertFalse((output / "libraries/net").exists())
        config = configparser.ConfigParser(interpolation=None)
        config.read(output / "instance.cfg")
        self.assertNotIn("WrapperCommand", config["General"])
        self.assertEqual(config["General"]["OverrideCommands"], "false")
        bundled = output / ".minecraft/bin/megu-client"
        self.assertEqual(bundled.read_bytes(), self.native.read_bytes())
        self.assertTrue(os.access(bundled, os.X_OK))
        self.assertFalse((output / "megu-wrapper").exists())
        # Export stays reproducible.
        again = self.root / "prism bootstrap patch again"
        self.export(again, *args)
        self.assertEqual((again / "patches/net.megu.bootstrapper.json").read_bytes(),
                         (output / "patches/net.megu.bootstrapper.json").read_bytes())

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
        cls.temporary = tempfile.TemporaryDirectory(prefix="megu-integrated-test-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.output = cls.root / "build-a"
        cls.jar = cls.output / "megu-integrated.jar"
        for folder in (cls.output, cls.root / "build-b"):
            subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                            "--out-dir", str(folder)], check=True, capture_output=True)
        cls.mock = cls.root / "mock-megu"
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
host = os.environ['MEGU_BIND_ADDR']
port = int(os.environ['MEGU_PORT'])
with socket.socket() as listener:
    listener.bind((host, port))
    listener.listen()
    Path('server.json').write_text(json.dumps({
        'pid': os.getpid(), 'host': host, 'port': port,
        'cwd': os.getcwd(), 'world': os.environ['MEGU_WORLD_DIR'],
        'seed': os.environ['MEGU_SEED'],
        'rcon': os.getenv('MEGU_RCON_PORT'),
        'public': os.getenv('MEGU_ALLOW_PUBLIC_BIND'),
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
                description = b'{"version":{"name":"1.21.1","protocol":767},"description":{"text":"megu"}}'
                body = encode(0) + encode(len(description)) + description
                conn.sendall(encode(len(body)) + body)
            except (OSError, EOFError, ValueError):
                pass
''')
        cls.mock.chmod(0o755)
        harness = cls.root / "ConfiguredBridgeHarness.java"
        harness.write_text('''import net.megu.integrated.IntegratedServerBridge;
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
                 ["-cp", str(self.jar), "net.megu.integrated.IntegratedServerBridge"])
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
        self.fail(f"megu server pid {pid} survived Java shutdown")

    def test_manifest_both_loaders_and_reproducible(self):
        self.assertEqual(self.jar.read_bytes(),
                         (self.root / "build-b/megu-integrated.jar").read_bytes())
        with zipfile.ZipFile(self.jar) as archive:
            entries = set(archive.namelist())
            fabric = json.loads(archive.read("fabric.mod.json"))
            neoforge = tomllib.loads(archive.read("META-INF/neoforge.mods.toml").decode())
            self.assertEqual(fabric["id"], "megu_integrated")
            self.assertEqual(fabric["depends"]["minecraft"], "1.21.1")
            self.assertEqual(fabric["depends"]["java"], ">=21")
            self.assertEqual(fabric["environment"], "client")
            self.assertEqual(fabric["entrypoints"]["client"],
                             ["net.megu.integrated.FabricIntegratedMod"])
            self.assertEqual(neoforge["modLoader"], "javafml")
            self.assertEqual(neoforge["mods"][0]["modId"], "megu_integrated")
            self.assertEqual(neoforge["dependencies"]["megu_integrated"][0]["modId"],
                             "minecraft")
            self.assertIn("net/megu/integrated/IntegratedServerBridge.class", entries)
            self.assertIn("net/megu/integrated/FabricIntegratedMod.class", entries)
            self.assertIn("net/megu/integrated/NeoForgeIntegratedMod.class", entries)
            self.assertFalse(any(name.startswith("net/fabricmc/") or
                                 name.startswith("net/neoforged/") for name in entries))

    def test_mock_protocol_seed_world_and_explicit_close(self):
        game = self.root / "mock-game"
        game.mkdir(exist_ok=True)
        env = dict(os.environ, MEGU_RCON_PORT="25575", MEGU_ALLOW_PUBLIC_BIND="1")
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
                   "-Dmegu.integrated.enabled=true",
                   "-Dmegu.integrated.serverBin=" + str(self.mock),
                   "-Dmegu.integrated.gameDir=" + str(game),
                   "-Dmegu.integrated.world=Configured world",
                   "-Dmegu.integrated.seed=123",
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
        flags = ["-Dmegu.integrated.enabled=true"] if enabled else []
        return game, subprocess.run(
            ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(), "-XX:-UsePerfData", *flags,
             "-Dmegu.integrated.serverBin=" + str(self.mock),
             "-Dmegu.integrated.gameDir=" + str(game),
             "-Dmegu.integrated.world=Lifecycle world",
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
        source.write_text("""import net.megu.integrated.FabricIntegratedMod;
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
             "-Dmegu.integrated.enabled=true",
             "-Dmegu.integrated.serverBin=" + str(self.mock),
             "-Dmegu.integrated.gameDir=" + str(game),
             "-Dmegu.integrated.world=Lifecycle world",
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
        binary = ROOT / "target/debug/megu"
        if not binary.is_file():
            self.skipTest("build target/debug/megu to run the real binary integration test")
        game = self.root / "real-game"
        game.mkdir(exist_ok=True)
        env = dict(os.environ, MEGU_SERVER_HOME=str(ROOT))
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

import net.megu.integrated.IntegratedServerBridge;
import net.megu.integrated.IntegratedServerBridge.DirectFrame;

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
        cls.temporary = tempfile.TemporaryDirectory(prefix="megu-direct-frame-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.build_a = cls.root / "build-a"
        cls.build_b = cls.root / "build-b"
        for folder in (cls.build_a, cls.build_b):
            subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                            "--out-dir", str(folder)], check=True, capture_output=True)
        cls.jar = cls.build_a / "megu-integrated.jar"
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
        second = self.build_b / "megu-integrated.jar"
        digest = hashlib.sha256(first.read_bytes()).hexdigest()
        self.assertEqual(digest, hashlib.sha256(second.read_bytes()).hexdigest())
        with zipfile.ZipFile(first) as archive:
            names = set(archive.namelist())
            self.assertIn("net/megu/integrated/IntegratedServerBridge$DirectFrame.class", names)
            self.assertFalse(any(name.startswith(("net/fabricmc/", "net/neoforged/"))
                                 for name in names))
            for item in archive.infolist():
                self.assertEqual(item.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((item.external_attr >> 16) & 0o777, 0o644)
                self.assertEqual(item.create_system, 3)


KERNEL_LIB_NAME = "libmegu_kernel.so"
MESH_LIB_NAME = "libmegu_client_mesh.so"
_NATIVE = {}


def native_libraries():
    """Build the client-kernel/client-port cdylibs once. Returns {name: (path|None, reason)}.

    MEGU_TEST_KERNEL_LIBRARY may name a prebuilt libmegu_kernel.so instead of the
    cargo build. A failed build yields (None, first failure) so tests report it.
    """
    if _NATIVE:
        return _NATIVE
    override = os.environ.get("MEGU_TEST_KERNEL_LIBRARY")
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
import net.megu.integrated.NativeBridgeException;
import net.megu.integrated.NativeKernelBridge;
import net.megu.integrated.NativeMeshBridge;

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
        String kernel = "libmegu_kernel.so";
        String mesh = "libmegu_client_mesh.so";
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
                "lacks symbol megu_kernel_palette_new_single");
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
        cls.temporary = tempfile.TemporaryDirectory(prefix="megu-native-bridge-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.jar_dir = cls.root / "jar"
        subprocess.run([sys.executable, str(TOOLS / "build_integrated_mod.py"),
                        "--out-dir", str(cls.jar_dir)], check=True, capture_output=True)
        cls.jar = cls.jar_dir / "megu-integrated.jar"
        harness = cls.root / "NativeBridgeHarness.java"
        harness.write_text(cls.HARNESS)
        subprocess.run(["javac", "-J-Djava.io.tmpdir=" + tempfile.gettempdir(),
                        "-J-XX:-UsePerfData", "--release", "21", "-cp", str(cls.jar),
                        "-d", str(cls.root), str(harness)], check=True, capture_output=True)
        fixtures = cls.root / "fixtures"
        cls.fixtures = fixtures
        abi = "unsigned %s(void) { return %d; }\n"
        kernel_abi, mesh_abi = "megu_kernel_abi_version", "megu_mesh_abi_version"
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
        self.assertEqual(self.jar.read_bytes(), (second / "megu-integrated.jar").read_bytes())
        with zipfile.ZipFile(self.jar) as archive:
            names = set(archive.namelist())
        for name in ("NativeBridgeException", "NativeKernelBridge", "NativeKernelBridge$Palette",
                     "NativeKernelBridge$Collision", "NativeLibrary", "NativeMeshBridge"):
            self.assertIn(f"net/megu/integrated/{name}.class", names)
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
import net.megu.integrated.FabricIntegratedMod;
import net.megu.integrated.NeoForgeIntegratedMod;
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
        Path info = Path.of(System.getProperty("megu.integrated.gameDir"), "saves",
                System.getProperty("megu.integrated.world"), "server.json");
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
        with tempfile.TemporaryDirectory(prefix="megu-easy-inst-") as td:
            root = Path(td)
            stage = root / "stage"
            subprocess.run(
                [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
                 "--out-dir", str(stage), "--mc-version", "1.21.1", "--build-jar"],
                check=True, capture_output=True, text=True,
            )
            jar = stage / "megu-bootstrapper.jar"
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
            self.assertTrue((mc_dir / "libraries/net/megu/bootstrapper/1.0.0/bootstrapper-1.0.0.jar").is_file())
            self.assertTrue((mc_dir / "versions/megu-1.21.1/megu-1.21.1.json").is_file())
            profiles = json.loads((mc_dir / "launcher_profiles.json").read_text())
            self.assertIn("existing", profiles["profiles"])
            self.assertIn("megu-1.21.1", profiles["profiles"])

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
        with tempfile.TemporaryDirectory(prefix="megu-easy-bundles-") as td:
            root = Path(td)
            stage = root / "stage"
            subprocess.run(
                [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
                 "--out-dir", str(stage), "--mc-version", "1.21.1", "--build-jar"],
                check=True, capture_output=True, text=True,
            )
            jar = stage / "megu-bootstrapper.jar"
            fake_bin = root / "megu-client"
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
            prism_a = out_a / "megu-prism-1.21.1.zip"
            prism_b = out_b / "megu-prism-1.21.1.zip"
            self.assertEqual(prism_a.read_bytes(), prism_b.read_bytes())
            with zipfile.ZipFile(prism_a) as zf:
                self.assertIn("patches/net.megu.bootstrapper.json", zf.namelist())
                self.assertIn(".minecraft/bin/megu-client", zf.namelist())
                for info in zf.infolist():
                    self.assertEqual(info.date_time, (1980, 1, 1, 0, 0, 0))

            mrpack = out_a / "megu-1.21.1.mrpack"
            with zipfile.ZipFile(mrpack) as zf:
                names = set(zf.namelist())
                self.assertIn("overrides/bin/megu-client", names)
                self.assertIn("overrides/config/megu-integrated.properties", names)
                cfg = zf.read("overrides/config/megu-integrated.properties").decode()
                self.assertIn("enabled=true", cfg)
                self.assertIn("serverBin=bin/megu-client", cfg)
                import io
                with zipfile.ZipFile(io.BytesIO(zf.read("overrides/mods/megu-integrated.jar"))) as mod_jar:
                    fabric = json.loads(mod_jar.read("fabric.mod.json"))
                    self.assertEqual(fabric["license"], "LGPL-3.0-or-later")


if __name__ == "__main__":
    unittest.main()
