#!/usr/bin/env python3
"""Hermetic tests for the opt-in mini-JRE builder (tools/build_mini_jre.py).

Every case runs against a temporary directory. The end-to-end cases drive a
fake `jlink` executable — a tiny Python script that only understands the argv
shape the tool emits — so no real JRE is ever produced here and the whole
suite finishes in well under a second. When a real `jlink` exists on the host
PATH, one additional case runs a lightweight `jlink --version` probe; it never
writes an image.

Run: python3 tests/mini_jre_test.py
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "build_mini_jre.py"
SPEC = importlib.util.spec_from_file_location("build_mini_jre", SCRIPT)
builder = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(builder)

FAKE_JLINK = '''#!/usr/bin/env python3
import os
import sys
from pathlib import Path

log = os.environ.get("FAKE_JLINK_LOG")
if log:
    with open(log, "a") as handle:
        handle.write(" ".join(sys.argv[1:]) + "\\n")

args = sys.argv[1:]
if "--version" in args:
    print("99.0.0-fake")
    sys.exit(0)
if "--help" in args:
    text = "--compress <c>  compress all resources in the output image\\n"
    if os.environ.get("FAKE_JLINK_HELP_ZIP", "1") == "1":
        text += "    allowed values: zip-{0-9}\\n"
    sys.stdout.write(text)
    sys.exit(0)

compress = next((a for a in args if a.startswith("--compress=")), None)
if (os.environ.get("FAKE_JLINK_REJECT_ZIP6") == "1"
        and compress == "--compress=zip-6"):
    sys.stderr.write("Error: unknown compress plugin zip-6\\n")
    sys.exit(3)
if os.environ.get("FAKE_JLINK_FAIL") == "1":
    sys.stderr.write("simulated jlink failure\\n")
    sys.exit(4)

output = Path(args[args.index("--output") + 1])
(output / "bin").mkdir(parents=True)
(output / "lib").mkdir(parents=True)
(output / "bin" / "java").write_text("#!/bin/sh\\n")
os.chmod(output / "bin" / "java", 0o755)
(output / "lib" / "modules").write_bytes(b"fake-jimage-blob")
(output / "release").write_text('JAVA_VERSION="99"\\n')
sys.exit(0)
'''

MINIMAL_MODULES = ["java.base", "java.logging", "jdk.unsupported"]
NEOFORGE_MODULES = [
    "java.base", "java.logging", "java.management", "java.naming",
    "java.xml", "java.sql", "java.instrument", "jdk.unsupported",
    "jdk.crypto.ec",
]
EXPECTED_FLAGS_32_256 = [
    "-Djava.awt.headless=true",
    "-Xms32m",
    "-Xmx256m",
    "-XX:+UseSerialGC",
    "-XX:MaxMetaspaceSize=96m",
    "-XX:+ExitOnOutOfMemoryError",
    "-XX:-UsePerfData",
]


class MiniJreTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="mini-jre-test-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.out = self.root / "mini-jre"
        self.staging = self.root / ".mini-jre.jlink-staging"
        self.log = self.root / "jlink-argv.log"
        self.fake = self.root / "fake-jlink"
        self.fake.write_text(FAKE_JLINK)
        self.fake.chmod(0o755)

    def cli(self, *extra, env=None):
        merged = dict(os.environ)
        if env:
            merged.update(env)
        return subprocess.run(
            [sys.executable, str(SCRIPT), *extra],
            capture_output=True, text=True, timeout=60, check=False, env=merged,
        )

    def fake_env(self, **extra):
        env = {"FAKE_JLINK_LOG": str(self.log)}
        env.update(extra)
        return env

    def test_profiles_match_curated_module_sets(self):
        self.assertEqual(list(builder.PROFILES["minimal"]), MINIMAL_MODULES)
        self.assertEqual(
            list(builder.PROFILES["neoforge-delegate"]), NEOFORGE_MODULES)

    def test_jlink_argv_exact_shape(self):
        argv = builder.build_jlink_argv(
            "/opt/jdk/bin/jlink", builder.PROFILES["minimal"],
            Path("/tmp/x/.out.jlink-staging"))
        self.assertEqual(argv, [
            "/opt/jdk/bin/jlink",
            "--add-modules", "java.base,java.logging,jdk.unsupported",
            "--strip-debug",
            "--no-man-pages",
            "--no-header-files",
            "--compress=zip-6",
            "--output", "/tmp/x/.out.jlink-staging",
        ])

    def test_jlink_argv_compress_override(self):
        argv = builder.build_jlink_argv(
            "jlink", builder.PROFILES["minimal"], Path("/s"), compress="2")
        self.assertIn("--compress=2", argv)
        self.assertNotIn("--compress=zip-6", argv)

    def test_delegate_flags_exact_list(self):
        self.assertEqual(builder.delegate_flags(32, 256), EXPECTED_FLAGS_32_256)

    def test_delegate_jvm_argv_prepends_java_binary(self):
        argv = builder.build_delegate_jvm_argv(self.out, 64, 512)
        self.assertEqual(Path(argv[0]), self.out / "bin" / "java")
        self.assertEqual(argv[1:], builder.delegate_flags(64, 512))
        self.assertIn("-Xms64m", argv)
        self.assertIn("-Xmx512m", argv)

    def test_delegate_flags_reject_bad_heap(self):
        for xms, xmx in ((256, 128), (16, 256), (32, 8192), (0, 0),
                         (-1, 256), (33, 32)):
            with self.subTest(xms=xms, xmx=xmx):
                with self.assertRaises(builder.UsageError):
                    builder.delegate_flags(xms, xmx)
        with self.assertRaises(builder.UsageError):
            builder.delegate_flags(32, 256, gc_mode="g1")

    def test_delegate_flags_accept_bounds_edges(self):
        self.assertIn("-Xms32m", builder.delegate_flags(32, 32))
        self.assertIn("-Xmx4096m", builder.delegate_flags(4096, 4096))

    def test_cli_heap_bounds_exit_2(self):
        for extra in (("--xms-mib", "300", "--xmx-mib", "100"),
                      ("--xms-mib", "16"),
                      ("--xmx-mib", "8192"),
                      ("--xms-mib", "abc")):
            with self.subTest(extra=extra):
                proc = self.cli("--out", str(self.out), "--dry-run", *extra)
                self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_out_dir_traversal_rejected(self):
        proc = self.cli("--out", str(self.root / ".." / "escape"), "--dry-run")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        proc = self.cli("--out", "../escape", "--dry-run")
        self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_out_dir_nul_and_control_chars_rejected(self):
        for raw in ("bad\x00path", "bad\x01path", "bad\x7fpath", "  ", ""):
            with self.subTest(raw=repr(raw)):
                with self.assertRaises(builder.UsageError):
                    builder.validate_out_dir(raw)
        proc = self.cli("--out", "bad\x01path", "--dry-run")
        self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_out_dir_symlink_escape_rejected(self):
        elsewhere = self.root / "elsewhere"
        elsewhere.mkdir()
        link = self.root / "linked-out"
        link.symlink_to(elsewhere, target_is_directory=True)
        with self.assertRaises(builder.UsageError):
            builder.validate_out_dir(str(link))
        proc = self.cli("--out", str(link), "--dry-run")
        self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_dry_run_json_reports_plan_and_writes_nothing(self):
        proc = self.cli(
            "--out", str(self.out), "--profile", "neoforge-delegate",
            "--jlink", str(self.fake), "--dry-run", "--json",
            env=self.fake_env())
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = json.loads(proc.stdout)
        self.assertEqual(report["schema"], "mc-spine-mini-jre/1")
        self.assertTrue(report["dry_run"])
        self.assertEqual(report["profile"], "neoforge-delegate")
        self.assertEqual(report["modules"], NEOFORGE_MODULES)
        self.assertEqual(report["jlink"]["bin"], str(self.fake))
        self.assertTrue(report["jlink"]["available"])
        self.assertEqual(report["jlink"]["compress"], "zip-6")
        argv = report["jlink_argv"]
        self.assertEqual(argv[0], str(self.fake))
        self.assertEqual(argv[1:3],
                         ["--add-modules", ",".join(NEOFORGE_MODULES)])
        self.assertIn("--compress=zip-6", argv)
        self.assertEqual(argv[-2:], ["--output", str(self.staging)])
        self.assertEqual(report["delegate_flags"], EXPECTED_FLAGS_32_256)
        self.assertEqual(report["delegate_argv"][1:], EXPECTED_FLAGS_32_256)
        self.assertFalse(self.out.exists())
        self.assertFalse(self.staging.exists())

    def test_dry_run_reports_missing_jlink_without_failing(self):
        empty_bin = self.root / "empty-bin"
        empty_bin.mkdir()
        proc = self.cli("--out", str(self.out), "--dry-run", "--json",
                        env={"PATH": str(empty_bin)})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = json.loads(proc.stdout)
        self.assertIsNone(report["jlink"]["bin"])
        self.assertFalse(report["jlink"]["available"])
        self.assertEqual(report["jlink_argv"][0], "jlink")

    def test_explicit_missing_jlink_is_usage_error(self):
        proc = self.cli("--out", str(self.out),
                        "--jlink", str(self.root / "no-such-jlink"))
        self.assertEqual(proc.returncode, 2, proc.stderr)

    def test_end_to_end_fake_jlink_stages_and_installs(self):
        proc = self.cli("--out", str(self.out), "--jlink", str(self.fake),
                        "--json", env=self.fake_env())
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue((self.out / "bin" / "java").is_file())
        self.assertTrue((self.out / "lib" / "modules").is_file())
        self.assertFalse(self.staging.exists())
        manifest = json.loads(
            (self.out / "mini-jre-manifest.json").read_text())
        self.assertEqual(manifest["schema"], "mc-spine-mini-jre/1")
        self.assertEqual(manifest["profile"], "minimal")
        self.assertEqual(manifest["modules"], MINIMAL_MODULES)
        self.assertEqual(manifest["delegate_flags"], EXPECTED_FLAGS_32_256)
        self.assertEqual(manifest["compress"], "zip-6")
        self.assertEqual(manifest["xms_mib"], 32)
        self.assertEqual(manifest["xmx_mib"], 256)
        self.assertEqual(manifest["gc_mode"], "serial")
        self.assertGreater(manifest["size_bytes"], 0)
        report = json.loads(proc.stdout)
        self.assertEqual(report["manifest"], manifest)
        argv_line = self.log.read_text().splitlines()[-1]
        for flag in ("--add-modules", "--strip-debug", "--no-man-pages",
                     "--no-header-files", "--compress=zip-6", "--output"):
            self.assertIn(flag, argv_line)

    def test_end_to_end_neoforge_profile_manifest(self):
        proc = self.cli("--out", str(self.out),
                        "--profile", "neoforge-delegate",
                        "--jlink", str(self.fake),
                        env=self.fake_env())
        self.assertEqual(proc.returncode, 0, proc.stderr)
        manifest = json.loads(
            (self.out / "mini-jre-manifest.json").read_text())
        self.assertEqual(manifest["profile"], "neoforge-delegate")
        self.assertEqual(manifest["modules"], NEOFORGE_MODULES)

    def test_jlink_failure_cleans_staging_and_leaves_no_out(self):
        proc = self.cli("--out", str(self.out), "--jlink", str(self.fake),
                        env=self.fake_env(FAKE_JLINK_FAIL="1"))
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertFalse(self.out.exists())
        self.assertFalse(self.staging.exists())

    def test_runtime_compress_fallback_retries_at_level_2(self):
        proc = self.cli("--out", str(self.out), "--jlink", str(self.fake),
                        env=self.fake_env(FAKE_JLINK_REJECT_ZIP6="1"))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        build_lines = [line for line in self.log.read_text().splitlines()
                       if "--output" in line]
        self.assertEqual(len(build_lines), 2)
        self.assertIn("--compress=zip-6", build_lines[0])
        self.assertIn("--compress=2", build_lines[1])
        manifest = json.loads(
            (self.out / "mini-jre-manifest.json").read_text())
        self.assertEqual(manifest["compress"], "2")

    def test_help_probe_fallback_uses_compress_2(self):
        proc = self.cli("--out", str(self.out), "--jlink", str(self.fake),
                        env=self.fake_env(FAKE_JLINK_HELP_ZIP="0"))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        build_lines = [line for line in self.log.read_text().splitlines()
                       if "--output" in line]
        self.assertEqual(len(build_lines), 1)
        self.assertIn("--compress=2", build_lines[0])
        self.assertNotIn("zip-6", build_lines[0])

    def test_existing_out_dir_is_refused_not_clobbered(self):
        self.out.mkdir()
        sentinel = self.out / "keep-me.txt"
        sentinel.write_text("precious")
        proc = self.cli("--out", str(self.out), "--jlink", str(self.fake),
                        env=self.fake_env())
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertEqual(sentinel.read_text(), "precious")
        self.assertFalse(self.staging.exists())

    def test_leftover_staging_dir_is_refused(self):
        self.staging.mkdir()
        proc = self.cli("--out", str(self.out), "--jlink", str(self.fake),
                        env=self.fake_env())
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertTrue(self.staging.exists())
        self.assertFalse(self.out.exists())

    def test_source_uses_no_shell(self):
        source = SCRIPT.read_text()
        self.assertNotIn("shell=True", source)
        self.assertNotIn("os.system", source)
        self.assertNotIn("os.popen", source)

    @unittest.skipUnless(shutil.which("jlink"), "no real jlink on host PATH")
    def test_real_jlink_version_probe(self):
        jlink = shutil.which("jlink")
        proc = subprocess.run([jlink, "--version"], capture_output=True,
                              text=True, timeout=10, check=False)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertRegex(proc.stdout + proc.stderr, r"\d+")
        self.assertIn(builder.detect_compress(Path(jlink)), {"zip-6", "2"})


if __name__ == "__main__":
    unittest.main()
