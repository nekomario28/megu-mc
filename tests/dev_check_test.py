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
import collections
import importlib.util
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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

    # ---- readiness is derived from the checks a route needs -----------------------------

    def test_missing_included_file_blocks_the_rust_route_even_with_the_compiler_installed(self):
        target = self.fixture.root / "tests/data/case.tsv"
        saved = target.read_bytes()
        target.unlink()
        code, output = self.fixture.run()
        self.assertEqual(code, 1, output)
        self.assertEqual(self.fixture.statuses(output)["toolchain_installed"], "OK",
                         "the compiler is installed; that alone must not make the route ready")
        # The include target is also a runtime-data root, so both checks name it.
        self.assertIn("route rust build/test: NOT READY (build_inputs MISSING", output)
        # No command is offered for a route that cannot run.
        self.assertNotIn("cargo test --offline", output)
        self.assertIn("Rust is not READY here: build_inputs", output)
        # Restoring the one input restores READY, so the red was the input.
        target.write_bytes(saved)
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertIn("route rust build/test: READY", output)
        self.assertIn("crate cache NOT_CHECKED", output)

    def test_absent_toolchain_pin_blocks_the_rust_route(self):
        (self.fixture.root / "rust-toolchain.toml").unlink()
        code, output = self.fixture.run()
        self.assertEqual(code, 1, output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["toolchain_pin"], "MISSING")
        self.assertIn("route rust build/test: NOT READY (toolchain_pin MISSING", output)

    # ---- the JDK version must be parsed, not guessed ------------------------------------

    def test_the_documented_jdk_major_is_ok(self):
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["jdk"], "OK")
        self.assertIn("javac 21.0.1", output)

    def test_jdk_output_without_a_javac_version_stays_not_checked(self):
        # Exit 0 with nothing parseable, and exit 0 with a number that is not the version
        # (the old check took the first digits anywhere in the output and called it OK).
        for label, version in (("empty output", None),
                               ("digits only in a warning", "warning: 21 deprecated options"),
                               ("no digits at all", "not a version")):
            with self.subTest(label):
                if version is None:
                    self.fixture.write_shim("javac", "#!/bin/sh\nexit 0\n")
                    env = self.fixture.env()
                else:
                    self.fixture.write_shim("javac", JAVAC_SHIM)
                    env = self.fixture.env(FAKE_JAVAC_VERSION=version)
                code, output = self.fixture.run(env=env)
                self.assertEqual(code, 0, output)  # an optional route, so not a repo fault
                statuses = self.fixture.statuses(output)
                self.assertEqual(statuses["jdk"], "NOT_CHECKED", output)
                self.assertIn("route jar/native oracle: NOT READY", output)

    def test_jdk_mismatched_major_stays_not_checked(self):
        env = self.fixture.env(FAKE_JAVAC_VERSION="javac 27.0.1")
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 0, output)
        statuses = self.fixture.statuses(output)
        self.assertEqual(statuses["jdk"], "NOT_CHECKED")
        self.assertIn("documents Java 21", output)
        self.assertIn("javac 27", output)
        self.assertIn("route jar/native oracle: NOT READY (vanilla_jar MISSING, "
                      "jdk NOT_CHECKED)", output)

    def test_jdk_requirement_that_cannot_be_read_stays_not_checked(self):
        generator = self.fixture.root / "tools/vanilla_data/gen_vanilla_data.sh"
        saved = generator.read_text()
        generator.write_text("# this text names no Java major\n")
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["jdk"], "NOT_CHECKED")
        self.assertIn("requirement unreadable", output)
        # An absent requirement source is the same unknown, not an OK.
        generator.unlink()
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["jdk"], "NOT_CHECKED")
        self.assertIn("gen_vanilla_data.sh is absent", output)
        generator.write_text(saved)
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["jdk"], "OK")

    # ---- the advertised exit status covers a bad environment ---------------------------

    def test_malformed_timeout_env_exits_two_without_a_traceback(self):
        env = self.fixture.env(MC_SPINE_DEV_CHECK_TIMEOUT_MS="soon")
        code, output = self.fixture.run(env=env)
        self.assertEqual(code, 2, output)
        self.assertIn("MC_SPINE_DEV_CHECK_TIMEOUT_MS", output)
        self.assertNotIn("Traceback", output)

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


def load_dev_check_module():
    """Import tools/dev_check.py in-process so collaborators can be mocked.

    The subprocess cases above pin the end-to-end contract; importing the module
    pins the new disk-headroom diagnostic deterministically, without depending on
    how much free space the test host happens to have.
    """
    spec = importlib.util.spec_from_file_location("dev_check_under_test", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


FAKE_USAGE = collections.namedtuple("usage", ("total", "used", "free"))
GIB = 1 << 30


class WorkspaceInventoryContract(unittest.TestCase):
    """The crate/profile/version-pack inventory is additive and never blocks."""

    def setUp(self):
        self.fixture = Fixture()
        self.addCleanup(self.fixture.cleanup)

    def fill_workspace(self):
        for crate in ("client-port", "client-kernel", "client-app", "mod-offload",
                      "mod-adapter-core", "create-adapter", "proto", "state-store",
                      "server-files"):
            crate_dir = self.fixture.root / crate
            crate_dir.mkdir()
            (crate_dir / "Cargo.toml").write_text(
                f'[package]\nname = "{crate}"\nlicense = "GPL-3.0-or-later"\n')
        manifest = self.fixture.root / "Cargo.toml"
        manifest.write_text(manifest.read_text()
                            + '[profile.fast]\ninherits = "release"\n')
        (self.fixture.root / "tools/vanilla_data/version_pack.py").write_text(
            "#!/usr/bin/env python3\n")

    def test_absent_pieces_are_named_but_do_not_block(self):
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["workspace"], "MISSING")
        self.assertIn("client-port/", output)
        self.assertIn("[profile.fast]", output)
        self.assertIn("version_pack.py", output)

    def test_full_inventory_reports_ok(self):
        self.fill_workspace()
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["workspace"], "OK")
        self.assertIn("9 crates", output)

    def test_one_missing_crate_is_named(self):
        self.fill_workspace()
        shutil.rmtree(self.fixture.root / "proto")
        code, output = self.fixture.run()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.fixture.statuses(output)["workspace"], "MISSING")
        self.assertIn("proto/", output)


class DiskHeadroomContract(unittest.TestCase):
    """shutil.disk_usage is mocked: the verdict is deterministic on any host."""

    def setUp(self):
        self.module = load_dev_check_module()
        self.tmp = Path(tempfile.mkdtemp(prefix="mc-spine-diskcheck-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        (self.tmp / "target").mkdir()
        (self.tmp / "tmp").mkdir()
        self.env = {"CARGO_TARGET_DIR": str(self.tmp / "target"),
                    "TMPDIR": str(self.tmp / "tmp")}

    def headroom(self, free_by_label, extra_env=None):
        def fake_disk_usage(path):
            return FAKE_USAGE(100 * GIB, 0, free_by_label[str(path)])
        env = {"MC_SPINE_DEV_CHECK_DISK_FATAL": ""}
        env.update(self.env)
        env.update(extra_env or {})
        with mock.patch.dict(os.environ, env, clear=False), \
                mock.patch("shutil.disk_usage", side_effect=fake_disk_usage):
            return self.module.check_disk_headroom()

    def test_plenty_of_space_is_a_plain_ok(self):
        result = self.headroom({str(self.tmp / "target"): 40 * GIB,
                                str(self.tmp / "tmp"): 20 * GIB})
        self.assertEqual(result.status, "OK")
        self.assertNotIn("warning", result.summary)
        self.assertIn("40.0 GiB", result.summary)
        self.assertTrue(result.executed.startswith("ran: shutil.disk_usage"))

    def test_under_five_gib_warns_but_stays_ok(self):
        result = self.headroom({str(self.tmp / "target"): 2 * GIB,
                                str(self.tmp / "tmp"): 20 * GIB})
        self.assertEqual(result.status, "OK",
                         "low disk is a warning, never a blocker by default")
        self.assertIn("warning", result.summary)
        self.assertIn("under 5 GiB", result.summary)
        self.assertIn("target", result.summary)
        self.assertIn("CARGO_TARGET_DIR", result.detail)

    def test_low_tmp_is_named_too(self):
        result = self.headroom({str(self.tmp / "target"): 40 * GIB,
                                str(self.tmp / "tmp"): 1 * GIB})
        self.assertEqual(result.status, "OK")
        self.assertIn("warning", result.summary)
        self.assertIn("tmp", result.summary)
        self.assertIn("TMPDIR", result.detail)

    def test_fatal_env_turns_the_warning_into_an_error(self):
        result = self.headroom({str(self.tmp / "target"): 2 * GIB,
                                str(self.tmp / "tmp"): 20 * GIB},
                               extra_env={"MC_SPINE_DEV_CHECK_DISK_FATAL": "1"})
        self.assertEqual(result.status, "ERROR")
        self.assertIn("LOW DISK", result.summary)

    def test_unreadable_path_is_reported_not_raised(self):
        env = dict(self.env)
        with mock.patch.dict(os.environ, env, clear=False), \
                mock.patch("shutil.disk_usage",
                           side_effect=OSError("no such filesystem")):
            result = self.module.check_disk_headroom()
        self.assertEqual(result.status, "OK")
        self.assertIn("unreadable", result.summary)

    def test_report_shows_the_new_check(self):
        fixture = Fixture()
        self.addCleanup(fixture.cleanup)
        code, output = fixture.run()
        self.assertEqual(code, 0, output)
        # OK both when the host has room and when it does not: a warning keeps OK.
        self.assertEqual(fixture.statuses(output)["disk_headroom"], "OK")
        self.assertIn("shutil.disk_usage", output)


if __name__ == "__main__":
    unittest.main()
