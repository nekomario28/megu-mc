"""Real Java 21/bootstrapper and Prism wrapper integration checks."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / "tools/client_launcher"


class LauncherTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-client-launcher-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.temp_java = cls.root / "java-tmp"
        cls.temp_java.mkdir()
        cls.jar_dir = cls.root / "profile-one"
        cls.stub = cls.root / "native-client"
        cls.stub.write_text("""#!/usr/bin/env python3
import json
import os
import sys
import time
from pathlib import Path
Path(os.environ['TEST_NATIVE_LOG']).write_text(json.dumps(sys.argv[1:]))
time.sleep(float(os.environ.get('TEST_NATIVE_SLEEP', '0')))
sys.exit(int(os.environ.get('TEST_NATIVE_EXIT', '0')))
""")
        cls.stub.chmod(0o755)
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
        return self.invocation("java", "-Djava.io.tmpdir=" + str(self.temp_java),
                               "-jar", str(self.jar_dir / "mc-spine-bootstrapper.jar"),
                               *args, extra_env=extra_env)

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


if __name__ == "__main__":
    unittest.main()
