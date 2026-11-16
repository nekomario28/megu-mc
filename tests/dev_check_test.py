#!/usr/bin/env python3
"""The preflight's own contract: what it reports, what it must never do.

Everything here runs against a synthetic repository root in a temporary directory, so
the cases are hermetic: no Cargo, no JDK, no java archive, no generated game data and
no network. The probes (`rustup`, `javac`, `depsgx`) are fakes on PATH, so what these
tests pin is the preflight's plumbing and its status/exit contract — a fake tool proves
the wiring, never the real tool. The real tools are exercised by running
`tools/dev_check.py` against a real checkout and a tracked-only export by hand; that
run is recorded in the review evidence, not here.

The negative controls matter most: several cases show the same fixture going green,
then red once one input is removed, and green again when it is restored. A checker
that cannot report a missing input would pass all of the "healthy" cases too.
"""
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve()
ROOT = HERE.parents[1]
SCRIPT = ROOT / "tools/dev_check.py"

PINNED_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
STATUS_LINE = re.compile(r"^\[([A-Z_]+)\s*\]\s+(\S+)\s+(.*)$")

CARGO_SHIM = """#!/bin/sh
echo "cargo $*" >> "$MARKER_DIR/compile-attempts"
exit 0
"""

CARGO_MARKER = """#!/bin/sh
: > "$MARKER_DIR/compile-attempts"
echo "this tool must not run"
exit 0
"""

RUSTUP_SHIM = """#!/bin/sh
if [ -n "${FAKE_RUSTUP_SLEEP:-}" ]; then sleep "$FAKE_RUSTUP_SLEEP"; fi
if [ -n "${FAKE_RUSTUP_EXIT:-}" ]; then echo "rustup exploded" >&2; exit "$FAKE_RUSTUP_EXIT"; fi
printf '%s\\n' ${FAKE_TOOLCHAINS:-}
exit 0
"""

JAVAC_SHIM = """#!/bin/sh
echo "${FAKE_JAVAC_VERSION:-javac 21.0.1}"
exit 0
"""

NOOP_SHIM = """#!/bin/sh
exit 0
"""


class Fixture:
    """A synthetic mc-spine-shaped tree plus the fake tools the probes will find."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="mc-spine-devcheck-"))
        self.root = self.tmp / "repo"
        self.home = self.tmp / "home"
        self.bin = self.tmp / "bin"
        self.marker = self.tmp / "markers"
        self.elsewhere = self.tmp / "elsewhere"
        for folder in (self.root / "src", self.root / "tools", self.root / "tests/data",
                       self.root / "assets", self.root / "datapacks/vanilla", self.home,
                       self.bin, self.marker, self.elsewhere):
            folder.mkdir(parents=True, exist_ok=True)

        shutil.copy2(SCRIPT, self.root / "tools/dev_check.py")
        (self.root / "Cargo.toml").write_text('[package]\nname = "mc-spine"\n')
        (self.root / "rust-toolchain.toml").write_text(
            '# pinned\n[toolchain]\nchannel = "1.98.0"\ncomponents = ["rustfmt", "clippy"]\n')
        (self.root / "src/lib.rs").write_text(
            '/// The fixture the crate cannot compile without.\n'
            'pub const DATA: &str = include_str!("../tests/data/case.tsv");\n')
        (self.root / "tests/data/case.tsv").write_text("a\tb\n1\t2\n")
        (self.root / "assets/feature_order_1211.json").write_text("{}\n")
        (self.root / "datapacks/vanilla/pack.mcmeta").write_text('{"pack": {}}\n')
        (self.root / "config_packets.bin").write_bytes(b"packets")
        # The real module is parsed for two literals; it is never imported.
        (self.root / "tools/vanilla_java.py").write_text(
            '"""stub for the fixture"""\n'
            'from pathlib import Path\n'
            'import os\n'
            f'JAR_SHA1 = "{PINNED_SHA1}"\n'
            'DEFAULT_JAR = Path(os.environ.get("MC_SPINE_VANILLA_SERVER_JAR", str(\n'
            '    Path.home() / ".cache/mc-spine-audit/load/vanilla/server.jar")))\n')
        # The jar-side generator documents the JDK it needs; dev_check reads that number.
        (self.root / "tools/vanilla_data").mkdir()
        (self.root / "tools/vanilla_data/gen_vanilla_data.sh").write_text(
            "# Needs Java 21 (java on PATH) and python3.\n")

        self.write_shim("rustup", RUSTUP_SHIM)
        self.write_shim("javac", JAVAC_SHIM)
        self.write_shim("java", NOOP_SHIM)
        self.write_shim("cargo", CARGO_MARKER)
        self.write_shim("rustc", CARGO_MARKER)

    def write_shim(self, name, body):
        path = self.bin / name
        path.write_text(body)
        path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)

    def env(self, **extra):
        # A minimal PATH: the host's real rustup/java/cargo must never be reached.
        environment = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "HOME": str(self.home),
            "LANG": "C.UTF-8",
            "MARKER_DIR": str(self.marker),
            "FAKE_TOOLCHAINS": "1.98.0-x86_64-unknown-linux-gnu",
        }
        for key, value in extra.items():
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = value
        return environment

    def run(self, *args, cwd=None, env=None, timeout=120):
        proc = subprocess.run([sys.executable, str(self.root / "tools/dev_check.py"), *args],
                              cwd=str(cwd or self.elsewhere), env=env or self.env(),
                              capture_output=True, text=True, timeout=timeout)
        return proc.returncode, proc.stdout + proc.stderr

    def statuses(self, output):
        found = {}
        for line in output.splitlines():
            match = STATUS_LINE.match(line)
            if match:
                found[match.group(2)] = match.group(1)
        return found

    def snapshot(self):
        state = {}
        for path in sorted(self.root.rglob("*")):
            info = path.stat()
            content = path.read_bytes() if path.is_file() else b""
            state[str(path.relative_to(self.root))] = (info.st_mode, info.st_mtime_ns, content)
        return state

    def cleanup(self):
        shutil.rmtree(self.tmp, ignore_errors=True)


class DevCheckContract(unittest.TestCase):
    def setUp(self):
        self.fixture = Fixture()
        self.addCleanup(self.fixture.cleanup)

    # ---- the healthy shape -------------------------------------------------------------

    def test_healthy_tracked_shape_is_ready_and_says_what_it_did(self):
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertIn("READY_FOR_PYTHON_CHECKS", output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["python"], "OK")
        self.assertEqual(statuses["toolchain_pin"], "OK")
        self.assertEqual(statuses["toolchain_installed"], "OK")
        self.assertEqual(statuses["build_inputs"], "OK")
        self.assertEqual(statuses["runtime_data"], "OK")
        self.assertEqual(statuses["jdk"], "OK")
        # Optional native capabilities are reported missing, not silently green.
        self.assertEqual(statuses["vanilla_jar"], "MISSING")
        self.assertEqual(statuses["depsgx"], "MISSING")
        self.assertEqual(statuses["crate_cache"], "NOT_CHECKED")
        self.assertIn("route jar/native oracle: NOT READY", output)
        # Every check states whether it executed something.
        self.assertEqual(output.count("ran:") + output.count("not run:"),
                         len(statuses), output)
        # A preflight is not a suite result, and must not pretend to be one.
        self.assertNotIn("all tests pass", output.lower())
        self.assertIn("a green preflight is not a green suite", output)

    def test_missing_optional_capability_does_not_fail_pure_tools(self):
        env = self.fixture.env(FAKE_TOOLCHAINS="")
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 0, output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["toolchain_installed"], "MISSING")
        self.assertEqual(statuses["python"], "OK")
        self.assertIn("route python/fixtures: READY", output)
        self.assertIn("route rust build/test: NOT READY", output)

    # ---- negative controls: the checker can report a hole -------------------------------

    def test_missing_included_file_turns_the_same_fixture_red_then_green_again(self):
        target = self.fixture.root / "tests/data/case.tsv"
        saved = target.read_bytes()
        target.unlink()
        code, output = self.fixture.run()
        self.assertEqual(code, 1, output)
        self.assertIn("BLOCKED", output)
        self.assertEqual(self.fixture.statuses(output)["build_inputs"], "MISSING")
        self.assertIn("tests/data/case.tsv", output)
        self.assertIn("the crate will not compile", output)
        # Restoring the single input restores green: the red was the input, not the fixture.
        target.write_bytes(saved)
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["build_inputs"], "OK")

    def test_missing_runtime_data_turns_the_checkout_red(self):
        (self.fixture.root / "config_packets.bin").unlink()
        code, output = self.fixture.run()
        self.assertEqual(code, 1, output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["runtime_data"], "MISSING")
        self.assertEqual(statuses["build_inputs"], "OK", "only the removed input is reported")
        self.assertIn("config_packets.bin", output)

    def test_absent_pinned_compiler_is_reported_not_assumed(self):
        env = self.fixture.env(FAKE_TOOLCHAINS="1.91.0-x86_64-unknown-linux-gnu")
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 0, output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["toolchain_installed"], "MISSING")
        self.assertIn("rustup toolchain install 1.98.0", output)
        self.assertIn("route rust build/test: NOT READY", output)

    def test_wrong_archive_is_an_error_and_blocks(self):
        archive = self.fixture.tmp / "server.jar"
        archive.write_bytes(b"not the pinned build")
        env = self.fixture.env(MC_SPINE_VANILLA_SERVER_JAR=str(archive))
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 1, output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["vanilla_jar"], "ERROR")
        self.assertIn("do not run", output)

    # ---- probes: bounded, never a false OK ----------------------------------------------

    def test_probe_timeout_is_never_reported_as_ok(self):
        env = self.fixture.env(FAKE_RUSTUP_SLEEP="5")
        code, output = self.fixture.run("--timeout-ms", "200", env=env)
        self.assertEqual(code, 0, output)  # an optional probe timing out is not a repo fault
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["toolchain_installed"], "NOT_CHECKED")
        self.assertIn("timed out", output)
        self.assertIn("a timed-out probe is not an installed toolchain", output)

    def test_failing_probe_is_not_checked_rather_than_ok(self):
        env = self.fixture.env(FAKE_RUSTUP_EXIT="3")
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["toolchain_installed"], "NOT_CHECKED")
        self.assertIn("rustup exploded", output)

    def test_all_four_statuses_can_appear_in_one_report(self):
        archive = self.fixture.tmp / "server.jar"
        archive.write_bytes(b"wrong")
        env = self.fixture.env(MC_SPINE_VANILLA_SERVER_JAR=str(archive), FAKE_RUSTUP_EXIT="2")
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 1, output)
        self.assertEqual(set(self.fixture.statuses(output).values()),
                         {"OK", "MISSING", "NOT_CHECKED", "ERROR"})

    # ---- safety: anchoring, no mutation, no compile, no execution of repository code ---

    def test_wrong_cwd_still_anchors_the_repository(self):
        for where in (self.fixture.elsewhere, Path("/")):
            code, output = self.fixture.run(cwd=where)
            self.assertEqual(code, 0, output)
            self.assertIn(f"repo: {self.fixture.root.resolve()}", output)
            self.assertEqual(self.fixture.statuses(output)["runtime_data"], "OK")

    def test_no_mutation_no_compile_no_cargo_and_no_repository_code_executed(self):
        # tools/vanilla_java.py and the shims would leave markers if anything executed them.
        (self.fixture.root / "tools/vanilla_java.py").write_text(
            'import os\n'
            'open(os.environ["MARKER_DIR"] + "/vanilla_java-imported", "w").close()\n'
            f'JAR_SHA1 = "{PINNED_SHA1}"\n'
            'DEFAULT_JAR = Path.home() / ".cache/mc-spine-audit/load/vanilla/server.jar"\n')
        self.fixture.write_shim("depsgx", """#!/bin/sh
: > "$MARKER_DIR/depsgx-executed"
echo "depsgx 0.0.0"
""")
        before = self.fixture.snapshot()
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.snapshot(), before, "the preflight must not touch the tree")
        self.assertFalse((self.fixture.root / "target").exists(), "no build was started")
        for marker in ("compile-attempts", "vanilla_java-imported", "depsgx-executed"):
            self.assertFalse((self.fixture.marker / marker).exists(),
                             f"{marker}: the preflight executed something it must not")
        # depsgx is reported by presence only, and says so.
        self.assertEqual(self.fixture.statuses(output)["depsgx"], "OK")
        self.assertIn("presence only", output)

    def test_an_include_outside_the_repository_is_an_error(self):
        (self.fixture.root / "src/lib.rs").write_text(
            'pub const ESCAPE: &str = include_str!("../../../../etc/hostname");\n')
        code, output = self.fixture.run()
        self.assertEqual(code, 1, output)
        self.assertEqual(self.fixture.statuses(output)["build_inputs"], "ERROR")

    def test_a_directory_that_is_not_a_root_exits_two(self):
        lone = self.fixture.tmp / "not-a-repo"
        (lone / "tools").mkdir(parents=True)
        shutil.copy2(SCRIPT, lone / "tools/dev_check.py")
        proc = subprocess.run([sys.executable, str(lone / "tools/dev_check.py")],
                              cwd=str(self.fixture.elsewhere), capture_output=True, text=True,
                              timeout=120)
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("is not the mc-spine root", proc.stderr)


if __name__ == "__main__":
    unittest.main()
