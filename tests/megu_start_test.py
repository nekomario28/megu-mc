#!/usr/bin/env python3
"""Operator onboarding, modpack safety, and error diagnostics regression tests."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "megu_start.py"
SPEC = importlib.util.spec_from_file_location("megu_start", SCRIPT)
megu_start = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(megu_start)


def pack_index():
    def entry(name):
        return {
            "path": f"mods/{name}-1.21.1.jar",
            "hashes": {"sha1": "a" * 40},
            "env": {"client": "required", "server": "required"},
            "downloads": [f"https://example.invalid/{name}.jar"],
            "fileSize": 4096,
        }

    return {
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "1.0",
        "name": "Create and optimizations",
        "files": [entry(name) for name in ("create", "sodium", "lithium")],
        "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.93"},
    }


class MeguStartTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="megu-start-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.server = self.base / "server"
        self.env_patch = mock.patch.dict(os.environ, {
            "MC_SPINE_PORT": "", "MC_SPINE_BIND_ADDR": "", "MC_SPINE_VERSION_DIR": "",
        })
        self.env_patch.start()
        self.addCleanup(self.env_patch.stop)

    def cli(self, *options, directory=None, env=None):
        command = [sys.executable, str(SCRIPT), "--dir", str(directory or self.server),
                   *options]
        environment = os.environ.copy()
        if env:
            environment.update(env)
        return subprocess.run(command, shell=False, capture_output=True, text=True,
                              env=environment, timeout=25, check=False)

    def pack(self, *, extra_members=()):
        path = self.base / "example.mrpack"
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("modrinth.index.json", json.dumps(pack_index()))
            for name, content in extra_members:
                archive.writestr(name, content)
        return path

    def test_first_run_requires_explicit_eula_in_english(self):
        process = self.cli("--dry-run", "--lang", "en")
        self.assertEqual(process.returncode, 2, process.stderr)
        self.assertEqual((self.server / "eula.txt").read_text(), "eula=false\n")
        self.assertTrue((self.server / "server.properties").is_file())
        self.assertEqual(len(process.stdout.strip().splitlines()), 2)
        self.assertIn("--accept-eula", process.stdout)
        self.assertIn("MinecraftEULA", process.stdout)

    def test_first_run_requires_explicit_eula_in_japanese(self):
        process = self.cli("--lang", "ja", "--dry-run")
        self.assertEqual(process.returncode, 2)
        self.assertEqual(len(process.stdout.strip().splitlines()), 2)
        self.assertIn("同意", process.stdout)
        self.assertIn("--accept-eula", process.stdout)

    def test_auto_language_uses_japanese_locale(self):
        process = self.cli("--dry-run", env={"LC_ALL": "ja_JP.UTF-8", "LANG": "en_US.UTF-8"})
        self.assertEqual(process.returncode, 2)
        self.assertIn("同意", process.stdout)

    def test_explicit_language_overrides_locale(self):
        process = self.cli("--dry-run", "--lang", "en",
                           env={"LC_ALL": "ja_JP.UTF-8"})
        self.assertEqual(process.returncode, 2)
        self.assertIn("EULA has not been accepted", process.stdout)

    def test_accepted_eula_json_plan_and_configuration_preservation(self):
        self.server.mkdir()
        original = ("# operator settings\nserver-port=24444\nserver-ip=127.0.0.1\n"
                    "rcon.password=very-secret-value\nvelocity.secret=velocity-value\n"
                    "admin_token=administrator-value\n")
        (self.server / "server.properties").write_text(original)
        process = self.cli("--accept-eula", "--dry-run", "--json", "--lang", "en")
        self.assertEqual(process.returncode, 0, process.stderr)
        report = json.loads(process.stdout)
        self.assertEqual(report["schema"], "megu-start-plan/1")
        self.assertTrue(report["ready"])
        self.assertTrue(report["dry_run"])
        self.assertEqual(report["bind_address"], "127.0.0.1")
        self.assertEqual(report["port"], 24444)
        self.assertEqual(report["mc_version"], "1.21.1")
        self.assertIsInstance(report["launch_argv"], list)
        self.assertTrue(report["launch_argv"])
        self.assertEqual(report["action_items"], [])
        for key in ("rcon.password", "velocity.secret", "admin_token"):
            self.assertEqual(report["server_properties"][key], "[REDACTED]")
        for secret in ("very-secret-value", "velocity-value", "administrator-value"):
            self.assertNotIn(secret, process.stdout)
        self.assertEqual((self.server / "server.properties").read_text(), original)
        self.assertEqual((self.server / "eula.txt").read_text(), "eula=true\n")

    def test_existing_eula_true_does_not_need_accept_flag(self):
        self.server.mkdir()
        original = "# agreed earlier\neula=true\n"
        (self.server / "eula.txt").write_text(original)
        process = self.cli("--dry-run", "--json")
        self.assertEqual(process.returncode, 0, process.stdout)
        self.assertTrue(json.loads(process.stdout)["ready"])
        self.assertEqual((self.server / "eula.txt").read_text(), original)

    def test_explicit_accept_updates_only_eula_setting(self):
        self.server.mkdir()
        (self.server / "eula.txt").write_text("# keep this\neula=false\n")
        process = self.cli("--accept-eula", "--dry-run", "--json")
        self.assertEqual(process.returncode, 0)
        self.assertEqual((self.server / "eula.txt").read_text(),
                         "# keep this\neula=true\n")

    def test_eula_gate_wins_over_bad_existing_port(self):
        self.server.mkdir()
        (self.server / "server.properties").write_text("server-port=not-a-number\n")
        process = self.cli("--dry-run", "--lang", "en")
        self.assertEqual(process.returncode, 2)
        self.assertIn("EULA", process.stdout)

    def test_valid_modpack_skips_performance_mods(self):
        pack = self.pack()
        process = self.cli("--accept-eula", "--dry-run", "--json", "--lang", "en",
                           "--modpack", str(pack))
        self.assertEqual(process.returncode, 0, process.stdout)
        report = json.loads(process.stdout)
        self.assertEqual(report["skipped_perf_mods"], ["lithium", "sodium"])
        self.assertEqual(report["native_mods"], ["create"])
        self.assertFalse(report["ready"])
        self.assertIn("mods have not been installed", " ".join(report["action_items"]))

    def test_modpack_version_mismatch_has_exit_two(self):
        process = self.cli("--accept-eula", "--dry-run", "--json", "--lang", "en",
                           "--modpack", str(self.pack()), "--mc-version", "26.3")
        self.assertEqual(process.returncode, 2, process.stdout)
        report = json.loads(process.stdout)
        self.assertEqual(report["skipped_perf_mods"], ["lithium", "sodium"])
        self.assertFalse(report["ready"])
        self.assertIn("different Minecraft version", " ".join(report["action_items"]))

    def test_traversal_in_archive_is_rejected_before_preflight(self):
        pack = self.pack(extra_members=[("../outside.jar", b"not a real mod")])
        process = self.cli("--accept-eula", "--dry-run", "--json", "--lang", "en",
                           "--modpack", str(pack))
        self.assertEqual(process.returncode, 1)
        report = json.loads(process.stdout)
        self.assertFalse(report["ready"])
        self.assertIn("unsafe path", " ".join(report["action_items"]))
        self.assertFalse((self.base / "outside.jar").exists())

    def test_corrupt_zip_is_rejected(self):
        pack = self.base / "broken.mrpack"
        pack.write_bytes(b"not a zip archive")
        process = self.cli("--accept-eula", "--dry-run", "--json", "--modpack", str(pack))
        self.assertEqual(process.returncode, 1)
        self.assertFalse(json.loads(process.stdout)["ready"])

    def test_manifest_path_traversal_is_rejected(self):
        document = pack_index()
        document["files"][0]["path"] = "mods/../../escape.jar"
        archive_path = self.base / "manifest-traversal.mrpack"
        with zipfile.ZipFile(archive_path, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("modrinth.index.json", json.dumps(document))
        process = self.cli("--accept-eula", "--dry-run", "--json",
                           "--modpack", str(archive_path))
        self.assertEqual(process.returncode, 1)
        self.assertFalse(json.loads(process.stdout)["ready"])

    def test_archive_symlink_is_rejected(self):
        pack = self.base / "linked.mrpack"
        with zipfile.ZipFile(pack, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("modrinth.index.json", json.dumps(pack_index()))
            entry = zipfile.ZipInfo("mods/linked.jar")
            entry.create_system = 3
            entry.external_attr = (0o120777 << 16)
            archive.writestr(entry, "../target")
        process = self.cli("--accept-eula", "--dry-run", "--json", "--modpack", str(pack))
        self.assertEqual(process.returncode, 1)

    def test_plain_text_version_mismatch(self):
        process = self.cli("--accept-eula", "--dry-run", "--lang", "ja",
                           "--modpack", str(self.pack()), "--mc-version", "26.3")
        self.assertEqual(process.returncode, 2)
        self.assertIn("バージョン", process.stdout)

    def test_invalid_operator_path_with_nul(self):
        with contextlib.redirect_stdout(io.StringIO()):
            result = megu_start.main(["--dir", "bad\x00path", "--dry-run"])
        self.assertEqual(result, 1)
        with self.assertRaises(ValueError):
            megu_start.checked_path("mods/\x00bad", "mod input")

    def test_directory_traversal_is_rejected_without_creating_it(self):
        candidate = self.base / "server" / ".." / "other"
        process = self.cli("--accept-eula", "--dry-run", directory=candidate)
        self.assertEqual(process.returncode, 1)
        self.assertFalse((self.base / "other").exists())

    def test_operator_directory_symlink_is_rejected(self):
        target = self.base / "actual"
        target.mkdir()
        alias = self.base / "shortcut"
        alias.symlink_to(target, target_is_directory=True)
        process = self.cli("--accept-eula", "--dry-run", directory=alias)
        self.assertEqual(process.returncode, 1)
        self.assertFalse((target / "eula.txt").exists())

    def test_port_override_in_plan(self):
        process = self.cli("--accept-eula", "--dry-run", "--json", "--port", "25566")
        self.assertEqual(process.returncode, 0)
        self.assertEqual(json.loads(process.stdout)["port"], 25566)
        self.assertIn("server-port=25566", (self.server / "server.properties").read_text())

    def test_empty_mods_folder_passes_preflight(self):
        installed = self.base / "installed-mods"
        installed.mkdir()
        process = self.cli("--accept-eula", "--dry-run", "--json",
                           "--mods-dir", str(installed))
        self.assertEqual(process.returncode, 0, process.stdout)
        report = json.loads(process.stdout)
        self.assertTrue(report["ready"])
        self.assertEqual(report["native_mods"], [])

    def test_missing_mods_folder_rejected(self):
        process = self.cli("--accept-eula", "--dry-run", "--json",
                           "--mods-dir", str(self.base / "absent"))
        self.assertEqual(process.returncode, 1)
        self.assertFalse(json.loads(process.stdout)["ready"])

    def test_invalid_port_rejected(self):
        process = self.cli("--accept-eula", "--dry-run", "--json", "--port", "70000")
        self.assertEqual(process.returncode, 1)
        self.assertFalse(self.server.exists())

    def test_missing_generated_data_has_exact_generation_command(self):
        output = io.StringIO()
        with mock.patch.object(megu_start, "data_ready", return_value=False):
            with contextlib.redirect_stdout(output):
                status = megu_start.main(["--dir", str(self.server), "--accept-eula",
                                          "--dry-run", "--json"])
        self.assertEqual(status, 2)
        report = json.loads(output.getvalue())
        self.assertFalse(report["ready"])
        self.assertIn("bash tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar",
                      report["action_items"][-1])

    def test_alternate_version_requires_explicit_data_directory(self):
        process = self.cli("--accept-eula", "--dry-run", "--json", "--mc-version", "26.3")
        self.assertEqual(process.returncode, 2)
        data = self.base / "version-pack"
        data.mkdir()
        (data / "block_states.json").write_text("{}")
        process = self.cli("--dry-run", "--json", "--mc-version", "26.3",
                           env={"MC_SPINE_VERSION_DIR": str(data)})
        self.assertEqual(process.returncode, 0)
        self.assertTrue(json.loads(process.stdout)["ready"])

    def test_preflight_uses_argv_and_explicit_no_shell(self):
        pack = self.pack()
        report = {"schema": "mc-spine-modpack-preflight/1", "readiness": "ready_native",
                  "skipped_perf_mods": ["sodium"], "native_mods": [],
                  "client_only_mods": [], "needs_inspection_mods": []}
        result = mock.Mock(returncode=0, stdout=json.dumps(report), stderr="")
        with mock.patch.object(megu_start.subprocess, "run", return_value=result) as run:
            with mock.patch.object(megu_start.subprocess, "Popen", side_effect=AssertionError(
                    "dry-run started a server")):
                with contextlib.redirect_stdout(io.StringIO()):
                    status = megu_start.main(["--dir", str(self.server), "--accept-eula",
                                              "--dry-run", "--modpack", str(pack)])
        self.assertEqual(status, 0)
        run.assert_called_once()
        self.assertIsInstance(run.call_args.args[0], list)
        self.assertIs(run.call_args.kwargs["shell"], False)
        self.assertIn("--json", run.call_args.args[0])

    def test_diagnose_error_for_six_categories_in_both_languages(self):
        cases = {
            "Address already in use (os error 98)": ("port", "ポート"),
            "EULA must be accepted": ("EULA", "同意"),
            "Mixin apply failed": ("mod", "Mod"),
            "serde_json invalid save file": ("save", "保存"),
            "missing generated data": ("data", "データ"),
            "UnsupportedClassVersionError class file version 65": ("Java", "Java"),
        }
        for error, (english, japanese) in cases.items():
            for lang, expected in (("en", english), ("ja", japanese)):
                with self.subTest(error=error, lang=lang):
                    message = megu_start.diagnose_error(error + " STACK-SECRET", lang)
                    self.assertEqual(len(message.splitlines()), 2)
                    self.assertIn(expected.lower(), message.lower())
                    self.assertNotIn("STACK-SECRET", message)

    def test_unknown_error_uses_generic_diagnosis_without_raw_stack(self):
        message = megu_start.diagnose_error("sensitive-content in stack trace", "en")
        self.assertEqual(len(message.splitlines()), 2)
        self.assertNotIn("sensitive-content", message)


if __name__ == "__main__":
    unittest.main()
