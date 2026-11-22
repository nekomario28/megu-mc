"""Cross-OS installer checks driven through the real compiled bootstrapper jar.

Simulates Windows, macOS and Linux by overriding -Dos.name/-Duser.home and the
APPDATA environment variable, exercising Official Launcher parity (version JSON,
launcher_profiles.json + launcher_profiles_microsoft_store.json, .bak backups,
idempotent re-install, --no-profile, --uninstall) plus the NeoForge/Fabric-style
--install-server staging (run.sh LF + 0o755, run.bat CRLF, no eula=true).
"""
import hashlib
import json
import os
from datetime import datetime, timezone
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / "tools/client_launcher"
MAX_JAR = 32 * 1024
ISO_FORMATS = ("%Y-%m-%dT%H:%M:%S%z",)


def iso8601(value):
    """Parse the ISO-8601 stamps the installer writes (with +00:00 or Z)."""
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


class InstallerCrossOsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mc-spine-xos-installer-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        for name in ("jar-a", "jar-b"):
            result = subprocess.run(
                [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
                 "--out-dir", str(cls.root / name), "--build-jar"],
                capture_output=True, text=True)
            if result.returncode != 0:
                raise RuntimeError("bootstrapper build failed: " + result.stderr)
        cls.jar = cls.root / "jar-a" / "mc-spine-bootstrapper.jar"

    def java(self, *args, os_name=None, user_home=None, env=None, check=False):
        command = ["java", "-Djava.io.tmpdir=" + tempfile.gettempdir(),
                   "-XX:-UsePerfData"]
        if os_name:
            command.append(f"-Dos.name={os_name}")
        if user_home:
            command.append(f"-Duser.home={user_home}")
        command += ["-jar", str(self.jar), *args]
        full_env = dict(os.environ, MC_SPINE_HEADLESS="1")
        full_env.pop("MC_SPINE_CLIENT_BIN", None)
        full_env.pop("MC_SPINE_CLIENT_SHA256", None)
        if env:
            full_env.update(env)
        result = subprocess.run(command, capture_output=True, text=True,
                                env=full_env, timeout=30)
        if check:
            self.assertEqual(result.returncode, 0,
                             f"{command}\nstdout={result.stdout}\nstderr={result.stderr}")
        return result

    @staticmethod
    def read_json(path):
        return json.loads(path.read_text(encoding="utf-8"))

    def assert_profile_entry(self, profiles_file, version_id):
        profiles = self.read_json(profiles_file)["profiles"]
        self.assertIn(version_id, profiles)
        entry = profiles[version_id]
        self.assertEqual(entry["type"], "custom")
        self.assertEqual(entry["lastVersionId"], version_id)
        self.assertEqual(entry["icon"], "Furnace")
        self.assertIn("name", entry)
        iso8601(entry["created"])
        iso8601(entry["lastUsed"])
        return entry

    def assert_version_json(self, mc_dir, version_id, inherits="1.21.1"):
        path = mc_dir / "versions" / version_id / f"{version_id}.json"
        self.assertTrue(path.is_file(), path)
        profile = self.read_json(path)
        self.assertEqual(profile["id"], version_id)
        self.assertEqual(profile["inheritsFrom"], inherits)
        self.assertEqual(profile["type"], "release")
        self.assertEqual(profile["mainClass"], "net.mcspine.launcher.Main")
        self.assertEqual(profile["libraries"],
                         [{"name": "net.mcspine:bootstrapper:1.0.0"}])
        self.assertTrue((mc_dir / "libraries/net/mcspine/bootstrapper/1.0.0"
                         "/bootstrapper-1.0.0.jar").is_file())
        for key in ("releaseTime", "time"):
            self.assertIn(key, profile)
            self.assertEqual(iso8601(profile[key]).tzinfo, timezone.utc)
        return profile

    def test_jar_size_and_deterministic_build(self):
        first = self.root / "jar-a" / "mc-spine-bootstrapper.jar"
        second = self.root / "jar-b" / "mc-spine-bootstrapper.jar"
        self.assertLess(first.stat().st_size, MAX_JAR)
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_windows_appdata_profiles_backup_and_uninstall(self):
        appdata = self.root / "App Data With Spaces"
        mc_dir = appdata / ".minecraft"
        mc_dir.mkdir(parents=True)
        existing = {
            "profiles": {
                "vanilla prof": {
                    "name": "バニラ プロファイル",
                    "type": "custom",
                    "lastVersionId": "1.21.1",
                    "nested": {"a": [1, 2, {"b": "x{y}"}]},
                },
            },
            "selectedProfile": "vanilla prof",
            "version": 3,
        }
        original_std = json.dumps(existing, ensure_ascii=False)
        original_store = '{ "profiles" : { "store one" : { "name" : "Store" } } }'
        (mc_dir / "launcher_profiles.json").write_text(original_std, encoding="utf-8")
        (mc_dir / "launcher_profiles_microsoft_store.json").write_text(
            original_store, encoding="utf-8")

        self.java("--install", os_name="Windows 11",
                  env={"APPDATA": str(appdata)}, check=True)
        version = self.assert_version_json(mc_dir, "mc-spine-1.21.1")
        self.assertNotIn("arguments", version)

        # Both launcher profile files updated, both backups hold pre-edit bytes.
        for name, original in (("launcher_profiles.json", original_std),
                               ("launcher_profiles_microsoft_store.json", original_store)):
            backup = mc_dir / (name + ".bak")
            self.assertTrue(backup.is_file(), name + ".bak")
            self.assertEqual(backup.read_text(encoding="utf-8"), original)
        self.assert_profile_entry(mc_dir / "launcher_profiles.json", "mc-spine-1.21.1")
        self.assert_profile_entry(mc_dir / "launcher_profiles_microsoft_store.json",
                                  "mc-spine-1.21.1")
        std = self.read_json(mc_dir / "launcher_profiles.json")
        self.assertIn("vanilla prof", std["profiles"])
        self.assertEqual(std["profiles"]["vanilla prof"]["nested"]["a"][2]["b"], "x{y}")
        self.assertEqual(std["selectedProfile"], "vanilla prof")
        store = self.read_json(mc_dir / "launcher_profiles_microsoft_store.json")
        self.assertIn("store one", store["profiles"])

        # Idempotent re-install: single entry, other profiles intact.
        self.java("--install", os_name="Windows 11",
                  env={"APPDATA": str(appdata)}, check=True)
        std = self.read_json(mc_dir / "launcher_profiles.json")
        self.assertEqual(list(std["profiles"]).count("mc-spine-1.21.1"), 1)
        self.assertIn("vanilla prof", std["profiles"])
        entry = self.assert_profile_entry(mc_dir / "launcher_profiles.json",
                                          "mc-spine-1.21.1")
        iso8601(entry["lastUsed"])

        # --uninstall removes the version dir and profile entries only.
        self.java("--uninstall", os_name="Windows 11",
                  env={"APPDATA": str(appdata)}, check=True)
        self.assertFalse((mc_dir / "versions" / "mc-spine-1.21.1").exists())
        std = self.read_json(mc_dir / "launcher_profiles.json")
        self.assertNotIn("mc-spine-1.21.1", std["profiles"])
        self.assertIn("vanilla prof", std["profiles"])
        self.assertEqual(std["selectedProfile"], "vanilla prof")
        store = self.read_json(mc_dir / "launcher_profiles_microsoft_store.json")
        self.assertNotIn("mc-spine-1.21.1", store["profiles"])
        self.assertIn("store one", store["profiles"])

    def test_windows_fallback_dir_without_appdata(self):
        home = self.root / "win home"
        mc_dir = home / "AppData" / "Roaming" / ".minecraft"
        env = dict(os.environ)
        env.pop("APPDATA", None)
        self.java("--install", os_name="Windows 11", user_home=str(home),
                  env=env, check=True)
        self.assert_version_json(mc_dir, "mc-spine-1.21.1")

    def test_macos_library_dir_no_profile_flag(self):
        home = self.root / "mac home"
        mc_dir = home / "Library" / "Application Support" / "minecraft"
        self.java("--install", os_name="Mac OS X", user_home=str(home), check=True)
        self.assert_version_json(mc_dir, "mc-spine-1.21.1")
        self.assert_profile_entry(mc_dir / "launcher_profiles.json", "mc-spine-1.21.1")

        # --no-profile installs versions/ + libraries/ but leaves profiles alone.
        home2 = self.root / "mac home 2"
        mc_dir2 = home2 / "Library" / "Application Support" / "minecraft"
        self.java("--install", "--no-profile", "--mc-version", "26.3",
                  os_name="Mac OS X", user_home=str(home2), check=True)
        self.assert_version_json(mc_dir2, "mc-spine-26.3", inherits="26.3")
        self.assertFalse((mc_dir2 / "launcher_profiles.json").exists())
        self.assertFalse((mc_dir2 / "launcher_profiles_microsoft_store.json").exists())

    def test_linux_dot_minecraft_and_flatpak_detection(self):
        home = self.root / "lin home"
        flatpak = home / ".var/app/com.mojang.Minecraft/.minecraft"
        flatpak.mkdir(parents=True)
        # Flatpak dir wins only when ~/.minecraft is absent.
        self.java("--install", os_name="Linux", user_home=str(home), check=True)
        self.assert_version_json(flatpak, "mc-spine-1.21.1")
        self.assertFalse((home / ".minecraft").exists())

        # A present ~/.minecraft always wins over the Flatpak location.
        (home / ".minecraft").mkdir()
        self.java("--install", "--mc-version", "26.3",
                  os_name="Linux", user_home=str(home), check=True)
        self.assert_version_json(home / ".minecraft", "mc-spine-26.3", inherits="26.3")

    def test_install_server_scripts_and_eula_never_forced(self):
        server_dir = self.root / "server dir with spaces"
        self.java("--install-server", str(server_dir), os_name="Linux", check=True)
        run_sh = server_dir / "run.sh"
        run_bat = server_dir / "run.bat"
        readme = server_dir / "README-SERVER.txt"
        for path in (run_sh, run_bat, readme):
            self.assertTrue(path.is_file(), path)
        sh = run_sh.read_bytes()
        self.assertNotIn(b"\r", sh)
        self.assertTrue(sh.startswith(b"#!/bin/sh"))
        self.assertIn(b'"$@"', sh)
        self.assertTrue(stat.S_IMODE(run_sh.stat().st_mode) & 0o111)
        bat = run_bat.read_bytes()
        self.assertIn(b"\r\n", bat)
        self.assertNotIn(b"\nmc", bat.replace(b"\r\n", b""))
        self.assertTrue(bat.startswith(b"@echo off"))
        self.assertIn(b"mc-spine.exe %*", bat)
        self.assertIn("eula", readme.read_text().lower())
        # The installer must never pre-accept the EULA.
        self.assertFalse((server_dir / "eula.txt").exists())
        refused = subprocess.run(["/bin/sh", str(run_sh)], cwd=server_dir,
                                 capture_output=True, text=True, timeout=15,
                                 env={"PATH": "/usr/bin:/bin", "EULA": ""})
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("EULA", refused.stderr)
        self.assertFalse((server_dir / "eula.txt").exists())

    def test_install_server_bin_staging(self):
        fake = self.root / "fake-mc-server"
        fake.write_text("#!/bin/sh\nexit 0\n")
        fake.chmod(0o755)
        server_dir = self.root / "server with bin"
        self.java("--install-server", str(server_dir), "--server-bin", str(fake),
                  check=True)
        staged = server_dir / "mc-spine"
        self.assertTrue(staged.is_file())
        self.assertEqual(staged.read_bytes(), fake.read_bytes())
        self.assertTrue(stat.S_IMODE(staged.stat().st_mode) & 0o111)
        exe_dir = self.root / "server exe"
        fake_exe = self.root / "fake-server.exe"
        fake_exe.write_text("MZ")
        self.java("--install-server", str(exe_dir), "--server-bin", str(fake_exe),
                  check=True)
        self.assertTrue((exe_dir / "mc-spine.exe").is_file())

    def test_install_server_fail_closed(self):
        bad = self.java("--install-server", str(self.root / ".." / "escape"),
                        os_name="Linux")
        self.assertEqual(bad.returncode, 2)
        self.assertIn("traversal", bad.stderr)

        real = self.root / "real-server-dir"
        real.mkdir()
        link = self.root / "link-server-dir"
        link.symlink_to(real, target_is_directory=True)
        bad = self.java("--install-server", str(link), os_name="Linux")
        self.assertEqual(bad.returncode, 2)
        self.assertIn("symlink", bad.stderr)

        planted = self.root / "planted"
        planted.mkdir()
        (planted / "run.sh").symlink_to("/etc/hostname")
        before = Path("/etc/hostname").read_bytes()
        bad = self.java("--install-server", str(planted), os_name="Linux")
        self.assertEqual(bad.returncode, 2)
        self.assertIn("symlink", bad.stderr)
        self.assertEqual(Path("/etc/hostname").read_bytes(), before)

        bad = self.java("--install-server", str(self.root / "s1"),
                        "--server-bin", str(self.root / "missing-bin"))
        self.assertEqual(bad.returncode, 2)
        bad = self.java("--install-server")
        self.assertEqual(bad.returncode, 2)

    def test_install_client_pin_and_fail_closed(self):
        mc_dir = self.root / "pin-mc"
        client = self.root / "pin-client"
        client.write_text("#!/bin/sh\nexit 0\n")
        client.chmod(0o755)
        sha = hashlib.sha256(client.read_bytes()).hexdigest()
        self.java("--install", "--minecraft-dir", str(mc_dir),
                  "--client-bin", str(client), "--client-sha256", sha, check=True)
        profile = self.assert_version_json(mc_dir, "mc-spine-1.21.1")
        self.assertEqual(profile["arguments"]["jvm"],
                         [f"-Dmc.spine.clientBin={client.resolve()}",
                          f"-Dmc.spine.clientSha256={sha}"])

        for extra, needle in (
                (("--mc-version", "bogus"), "unsupported --mc-version"),
                (("--client-sha256", "nothex"), "64 hexadecimal"),
                (("--client-bin", str(client), "--client-sha256", "0" * 64),
                 "SHA-256 mismatch"),
                (("--client-bin", str(self.root / "absent")), "--client-bin"),
                (("--uninstall", "--install"), "conflicts")):
            with self.subTest(extra=extra):
                result = self.java("--install", "--minecraft-dir",
                                   str(self.root / "bad-mc"), *extra)
                self.assertEqual(result.returncode, 2)
                self.assertIn(needle, result.stderr)

    def test_zero_arg_headless_still_installs(self):
        home = self.root / "headless home"
        result = self.java(os_name="Linux", user_home=str(home))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_version_json(home / ".minecraft", "mc-spine-1.21.1")
        self.assert_profile_entry(home / ".minecraft" / "launcher_profiles.json",
                                  "mc-spine-1.21.1")

    def test_gen_profile_windows_client_bin_suffix(self):
        """gen_launcher_profile.py accepts .exe/.bat/.cmd client binaries cross-platform."""
        out = self.root / "gen-win-bin"
        exe = self.root / "mc-spine-client.exe"
        exe.write_text("MZ not posix executable")
        exe.chmod(0o644)
        result = subprocess.run(
            [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
             "--out-dir", str(out), "--client-bin", str(exe)],
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        profile = json.loads(
            (out / "versions/mc-spine-1.21.1/mc-spine-1.21.1.json").read_text())
        self.assertIn("arguments", profile)
        iso8601(profile["releaseTime"])
        iso8601(profile["time"])
        plain = self.root / "plain-client"
        plain.write_text("#!/bin/sh\n")
        plain.chmod(0o644)
        result = subprocess.run(
            [sys.executable, str(TOOLS / "gen_launcher_profile.py"),
             "--out-dir", str(self.root / "gen-bad-bin"), "--client-bin", str(plain)],
            capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("executable", result.stderr)


if __name__ == "__main__":
    unittest.main()
