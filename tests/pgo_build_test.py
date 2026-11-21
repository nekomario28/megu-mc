#!/usr/bin/env python3
"""Hermetic tests for tools/pgo_build.sh (stdlib unittest only).

The script is copied into a throw-away repository root so that its
`cd "$(dirname "$0")/.."` lands in a temp dir.  A stub llvm-profdata is put on
PATH so --dry-run always finds a candidate, and a stub cargo that records any
invocation proves --dry-run builds nothing.

Run: python3 tests/pgo_build_test.py
Override the script location with PGO_BUILD_SH=/path/to/pgo_build.sh.
"""

import glob
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

GOOD_SHA = "670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def"
STALE_SHA = GOOD_SHA
TARGETS = ("server", "client-port", "client-app")
BASH = shutil.which("bash") or "/bin/bash"


def find_script() -> Path:
    env = os.environ.get("PGO_BUILD_SH")
    if env:
        return Path(env).resolve()
    here = Path(__file__).resolve().parent
    for d in (here, *here.parents):
        cand = d / "tools" / "pgo_build.sh"
        if cand.is_file():
            return cand
    raise FileNotFoundError("tools/pgo_build.sh not found above %s; set PGO_BUILD_SH" % here)


SCRIPT = find_script()


def write_exe(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class PgoBuildScriptTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="pgo_build_test_"))
        self.root = self.tmp / "repo"
        (self.root / "tools").mkdir(parents=True)
        self.script = self.root / "tools" / "pgo_build.sh"
        shutil.copy2(SCRIPT, self.script)
        self.stubs = self.tmp / "stubs"
        self.stubs.mkdir()
        self.cargo_marker = self.tmp / "cargo-was-called"
        write_exe(self.stubs / "llvm-profdata", "#!/bin/sh\nexit 0\n")
        write_exe(self.stubs / "cargo", '#!/bin/sh\necho "$@" >> "%s"\nexit 97\n' % self.cargo_marker)
        self.env = dict(os.environ)
        self.env["PATH"] = str(self.stubs) + os.pathsep + self.env.get("PATH", "")
        self.env["LC_ALL"] = "C"

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_script(self, *args, env=None):
        return subprocess.run(
            [BASH, str(self.script), *args],
            cwd=str(self.tmp),
            env=env or self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=60,
        )

    def assertExit(self, proc, code):
        self.assertEqual(
            proc.returncode, code,
            "exit %d != %d\nstdout:\n%s\nstderr:\n%s" % (proc.returncode, code, proc.stdout, proc.stderr),
        )

    def assertNothingBuilt(self):
        self.assertFalse(self.cargo_marker.exists(), "cargo was invoked: %s" % (
            self.cargo_marker.read_text() if self.cargo_marker.exists() else ""))
        self.assertFalse((self.root / "target").exists(), "target/ was created")

    def dry_run(self, *args):
        proc = self.run_script("--dry-run", *args)
        self.assertExit(proc, 0)
        self.assertNothingBuilt()
        self.assertIn("llvm-profdata candidates (priority order):", proc.stdout)
        self.assertIn(str(self.stubs / "llvm-profdata"), proc.stdout)
        return proc.stdout

    # ---- syntax / help ---------------------------------------------------------
    def test_bash_syntax(self):
        proc = subprocess.run([BASH, "-n", str(SCRIPT)], stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, universal_newlines=True)
        self.assertExit(proc, 0)

    def test_help(self):
        for flag in ("--help", "-h"):
            proc = self.run_script(flag)
            self.assertExit(proc, 0)
            for word in ("--out", "--dry-run", "--jobs", "--target", "server", "client-port",
                         "client-app", "all", "--expected-sha", "--native-cpu", "-h, --help"):
                self.assertIn(word, proc.stdout, "%s missing from %s output" % (word, flag))
            self.assertNothingBuilt()

    def test_help_wins_over_later_args(self):
        self.assertExit(self.run_script("--help", "--target", "bogus"), 0)

    # ---- dry-run per target ----------------------------------------------------
    def test_dry_run_default_is_server(self):
        out = self.dry_run()
        self.assertIn("targets: server\n", out)
        self.assertIn("== target server ==", out)
        self.assertNotIn("client-port", out)
        self.assertNotIn("client-app", out)
        self.assertIn("cargo build --release --offline --locked -j 2 --target-dir target/pgo/gen", out)
        self.assertIn("-Cprofile-generate=%s/target/pgo/raw" % self.root.resolve(), out)
        self.assertIn("-Cprofile-use=%s/target/pgo/merged.profdata -Cllvm-args=-pgo-warn-missing-function"
                      % self.root.resolve(), out)
        self.assertIn("overworld-j1, overworld-j8, nofeat-j8, nether-4242, end-0", out)
        self.assertIn("against the 64-chunk seed-0 full dump sha256 of target/pgo/gen/release/mc-spine", out)
        self.assertIn("final binary (server): target/pgo/use/release/mc-spine", out)
        self.assertNotIn("target-cpu=native", out)

    def test_dry_run_server_explicit_matches_default(self):
        self.assertEqual(self.dry_run("--target", "server"), self.dry_run())
        self.assertEqual(self.dry_run("--target=server"), self.dry_run())

    def test_dry_run_client_port(self):
        out = self.dry_run("--target", "client-port")
        self.assertIn("targets: client-port\n", out)
        self.assertNotIn("== target server ==", out)
        self.assertNotIn("== target client-app ==", out)
        self.assertIn("cargo run --manifest-path client-port/Cargo.toml --release --offline --locked -j 2 "
                      "--target-dir target/pgo/client-port/gen --bin mesh_bench -- 500", out)
        self.assertIn("-Cprofile-generate=%s/target/pgo/client-port/raw" % self.root.resolve(), out)
        self.assertIn("-Cprofile-use=%s/target/pgo/client-port/merged.profdata" % self.root.resolve(), out)
        self.assertIn("cargo build --manifest-path client-port/Cargo.toml --release --offline --locked -j 2 "
                      "--target-dir target/pgo/client-port/use --lib --bin mesh_bench", out)
        self.assertIn("target/pgo/client-port/use/release/mesh_bench 50 must print '2037 quads, 8148 vertices'", out)
        self.assertIn("final binary (client-port): target/pgo/client-port/use/release/mesh_bench", out)

    def test_dry_run_client_app(self):
        out = self.dry_run("--target", "client-app")
        self.assertIn("targets: client-app\n", out)
        self.assertNotIn("== target server ==", out)
        self.assertNotIn("== target client-port ==", out)
        self.assertIn("cargo test --manifest-path client-app/Cargo.toml --release --offline --locked -j 2 "
                      "--target-dir target/pgo/client-app/gen", out)
        self.assertIn("-Cprofile-generate=%s/target/pgo/client-app/raw" % self.root.resolve(), out)
        self.assertIn("-Cprofile-use=%s/target/pgo/client-app/merged.profdata" % self.root.resolve(), out)
        self.assertIn("cargo build --manifest-path client-app/Cargo.toml --release --offline --locked -j 2 "
                      "--target-dir target/pgo/client-app/use --bin mc-spine-client", out)
        self.assertIn("final binary (client-app): target/pgo/client-app/use/release/mc-spine-client", out)

    def test_dry_run_all_runs_every_target_in_order(self):
        out = self.dry_run("--target", "all")
        self.assertIn("targets: server client-port client-app\n", out)
        idx = [out.index("== target %s ==" % t) for t in TARGETS]
        self.assertEqual(idx, sorted(idx))
        for t in TARGETS:
            self.assertEqual(out.count("== target %s ==" % t), 1)
        self.assertIn("--bin mesh_bench -- 500", out)
        self.assertIn("cargo test --manifest-path client-app/Cargo.toml", out)
        self.assertIn("against the 64-chunk seed-0 full dump sha256 of target/pgo/gen/release/mc-spine", out)

    # ---- --native-cpu ----------------------------------------------------------
    def test_native_cpu_all_targets(self):
        for target in TARGETS + ("all",):
            with self.subTest(target=target):
                out = self.dry_run("--target", target, "--native-cpu")
                self.assertIn("native cpu: on (-Ctarget-cpu=native)", out)
                n_targets = 3 if target == "all" else 1
                # every instrumented and every optimized build carries the flag
                self.assertEqual(out.count("-Ctarget-cpu=native'"), 2 * n_targets, out)
                self.assertNotIn("-Ctarget-cpu=native -C", out)
        out = self.dry_run("--native-cpu")
        self.assertIn("-Cprofile-generate=%s/target/pgo/raw -Ctarget-cpu=native'" % self.root.resolve(), out)
        self.assertIn("-Cllvm-args=-pgo-warn-missing-function -Ctarget-cpu=native'", out)

    def test_native_cpu_off_by_default(self):
        for target in TARGETS + ("all",):
            out = self.dry_run("--target", target)
            self.assertIn("native cpu: off", out)
            self.assertNotIn("target-cpu", out)

    # ---- --expected-sha --------------------------------------------------------
    def test_expected_sha_server_and_all(self):
        sha = "ab" * 32
        for args in (("--expected-sha", sha), ("--expected-sha=" + sha,),
                     ("--target", "all", "--expected-sha", sha)):
            with self.subTest(args=args):
                out = self.dry_run(*args)
                self.assertIn("against --expected-sha %s" % sha, out)
                self.assertNotIn("full dump sha256 of target/pgo/gen", out)

    def test_expected_sha_uppercase_normalized(self):
        out = self.dry_run("--expected-sha", ("AB" * 32))
        self.assertIn("against --expected-sha %s" % ("ab" * 32), out)

    def test_expected_sha_with_all_options(self):
        out = self.dry_run("--target", "all", "--native-cpu", "--expected-sha", GOOD_SHA,
                           "--jobs", "6", "--out", "dist")
        self.assertIn("-j 6", out)
        self.assertNotIn("-j 2", out)
        self.assertIn("against --expected-sha %s" % GOOD_SHA, out)
        self.assertIn("final binary (server): dist/mc-spine", out)
        self.assertIn("final binary (client-port): dist/mesh_bench", out)
        self.assertIn("final binary (client-app): dist/mc-spine-client", out)

    def test_malformed_expected_sha_exit_2(self):
        bad = ["", "a" * 63, "a" * 65, "g" * 64, "0x" + "a" * 62, " " + "a" * 63, "a" * 63 + " ",
               "a" * 64 + "\n", "\n" + "a" * 64, "a" * 32 + "-" + "a" * 31, "a" * 64 + "a" * 64]
        for value in bad:
            for form in (("--expected-sha", value), ("--expected-sha=" + value,)):
                with self.subTest(form=form):
                    proc = self.run_script("--dry-run", *form)
                    self.assertExit(proc, 2)
                    self.assertIn("tools/pgo_build.sh:", proc.stderr)
                    self.assertNotIn("llvm-profdata candidates", proc.stdout)
                    self.assertNothingBuilt()

    def test_expected_sha_missing_value_exit_2(self):
        proc = self.run_script("--dry-run", "--expected-sha")
        self.assertExit(proc, 2)
        self.assertIn("--expected-sha requires a value", proc.stderr)

    def test_expected_sha_rejected_for_client_only_targets(self):
        for target in ("client-port", "client-app"):
            proc = self.run_script("--dry-run", "--target", target, "--expected-sha", GOOD_SHA)
            self.assertExit(proc, 2)
            self.assertIn("--expected-sha only applies", proc.stderr)
            self.assertNothingBuilt()

    def test_no_stale_hardcoded_digest(self):
        text = SCRIPT.read_text()
        self.assertNotIn(STALE_SHA, text)
        self.assertNotIn("EXPECT_FULL_SHA_256", text)

    # ---- fail-closed --target --------------------------------------------------
    def test_invalid_target_exit_2(self):
        for value in ("bogus", "", "Server", "SERVER", "client", "client_port", "clientport",
                      "all,server", "server ", " server", "client-port\n", "*"):
            for form in (("--target", value), ("--target=" + value,)):
                with self.subTest(form=form):
                    proc = self.run_script("--dry-run", *form)
                    self.assertExit(proc, 2)
                    self.assertIn("tools/pgo_build.sh:", proc.stderr)
                    self.assertNotIn("llvm-profdata candidates", proc.stdout)
                    self.assertNothingBuilt()

    def test_invalid_target_exit_2_without_dry_run(self):
        proc = self.run_script("--target", "bogus")
        self.assertExit(proc, 2)
        self.assertIn("unknown --target 'bogus'", proc.stderr)
        self.assertNothingBuilt()

    def test_target_missing_value_exit_2(self):
        proc = self.run_script("--dry-run", "--target")
        self.assertExit(proc, 2)
        self.assertIn("--target requires a value", proc.stderr)

    # ---- backward compatibility -------------------------------------------------
    def test_legacy_flags(self):
        out = self.dry_run("--jobs", "5")
        self.assertIn("-j 5 --target-dir target/pgo/gen", out)
        self.assertIn("-j 5 --target-dir target/pgo/use", out)
        self.assertEqual(self.dry_run("--jobs=5"), out)
        out = self.dry_run("--out", "/opt/mc-spine-pgo")
        self.assertIn("final binary (server): /opt/mc-spine-pgo", out)
        self.assertEqual(self.dry_run("--out=/opt/mc-spine-pgo"), out)
        out = self.dry_run("--out", "x.bin", "--jobs", "3", "--native-cpu", "--target", "client-port")
        self.assertIn("final binary (client-port): x.bin", out)
        self.assertIn("-j 3", out)

    def test_unknown_argument_exit_2(self):
        for arg in ("--bogus", "server", "--target-cpu=native", "-x"):
            proc = self.run_script("--dry-run", arg)
            self.assertExit(proc, 2)
            self.assertIn("unknown argument: %s" % arg, proc.stderr)
            self.assertIn("Usage:", proc.stderr)

    def test_legacy_missing_values_fail(self):
        for flag in ("--out", "--jobs"):
            proc = self.run_script("--dry-run", flag)
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("%s requires a" % flag, proc.stderr)
            self.assertNothingBuilt()

    @unittest.skipIf(glob.glob("/usr/bin/llvm-profdata*") + glob.glob("/usr/local/bin/llvm-profdata*"),
                     "a system llvm-profdata is installed; cannot hide it")
    def test_missing_llvm_profdata_exit_1(self):
        bindir = self.tmp / "minbin"
        bindir.mkdir()
        for tool in ("dirname", "basename", "tr", "find", "sort", "cat"):
            src = shutil.which(tool)
            if src:
                os.symlink(src, str(bindir / tool))
        env = {"PATH": str(bindir), "HOME": str(self.tmp), "RUSTUP_HOME": str(self.tmp / "no-rustup")}
        proc = self.run_script("--dry-run", env=env)
        self.assertExit(proc, 1)
        self.assertIn("rustup component add llvm-tools", proc.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
