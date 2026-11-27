#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Regression tests for tools/clippy_ratchet.sh.

The ratchet lints the root package and every top-level crate directory that
carries a Cargo.toml, comparing each against a per-crate ceiling in
tools/clippy-warning-baseline.txt. These tests drive the script with a fake
`cargo` and synthetic crate trees; they prove ANSI-colored diagnostics are
counted, per-crate ceilings are enforced, and a crate appearing or
disappearing without a baseline edit fails closed.
"""
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "clippy_ratchet.sh"

FAKE_CARGO = """#!/usr/bin/env bash
set -u
# Expected invocation: cargo clippy --manifest-path <m> --all-targets --locked
if [[ $# -ne 5 || "$1" != clippy || "$2" != --manifest-path || "$4" != --all-targets || "$5" != --locked ]]; then
  echo "unexpected args: $*" >&2
  exit 9
fi
manifest="$3"
echo "clippy $manifest" >> "$FAKE_CARGO_LOG"
count=0
if [[ -n "${FAKE_COUNTS_FILE:-}" ]]; then
  count=$(awk -v m="$manifest" '$1 == m { print $2 }' "$FAKE_COUNTS_FILE")
fi
count="${count:-0}"
if [[ "${FAKE_CARGO_FAIL:-0}" == 1 ]]; then
  echo "error: fake clippy failure" >&2
  exit 1
fi
for i in $(seq 1 "$count"); do
  printf '\\x1b[33mwarning\\x1b[0m: fake diagnostic %s\\n' "$i"
done
if [[ "${FAKE_COMPACT_WARNING:-0}" == 1 ]]; then
  printf '\\x1b[33mwarning\\x1b[0m:compact diagnostic\\n'
fi
if [[ "${FAKE_SUMMARY:-0}" == 1 ]]; then
  printf 'warning: `fake` (bin "fake" test) generated %s warnings\\n' "$count"
fi
exit 0
"""

CRATES = ("alpha", "beta")


def baseline_text(entries):
    return "".join(f"{name} {count}\n" for name, count in entries)


class ClippyRatchetTest(unittest.TestCase):
    def make_tree(self, tmp, crates=CRATES, baseline=None, root_manifest=True):
        tools = tmp / "tools"
        tools.mkdir(parents=True, exist_ok=True)
        (tools / "clippy_ratchet.sh").write_bytes(SCRIPT.read_bytes())
        if root_manifest:
            (tmp / "Cargo.toml").write_text("[package]\nname = \"root\"\n", encoding="utf-8")
        for crate in crates:
            directory = tmp / crate
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "Cargo.toml").write_text(
                f"[package]\nname = \"{crate}\"\n", encoding="utf-8")
        if baseline is not None:
            (tools / "clippy-warning-baseline.txt").write_text(
                baseline_text(baseline), encoding="utf-8")

    def run_ratchet(self, tmp, crates=CRATES, baseline=None, counts=None,
                    root_manifest=True, extra_env=None):
        if baseline is None:
            baseline = [(".", 0)] + [(crate, 0) for crate in crates]
        self.make_tree(tmp, crates=crates, baseline=baseline,
                       root_manifest=root_manifest)
        bin_dir = tmp / "bin"
        bin_dir.mkdir(exist_ok=True)
        fake_cargo = bin_dir / "cargo"
        fake_cargo.write_text(FAKE_CARGO, encoding="utf-8")
        fake_cargo.chmod(fake_cargo.stat().st_mode | stat.S_IXUSR)
        counts_file = tmp / "counts.txt"
        counts_file.write_text(
            "".join(f"{manifest} {count}\n" for manifest, count in (counts or {}).items()),
            encoding="utf-8")
        env = dict(os.environ)
        env["PATH"] = "%s%s%s" % (bin_dir, os.pathsep, env.get("PATH", ""))
        env["CARGO_TERM_COLOR"] = "always"
        env["FAKE_COUNTS_FILE"] = str(counts_file)
        env["FAKE_CARGO_LOG"] = str(tmp / "cargo.env.log")
        env.update(extra_env or {})
        return subprocess.run(
            ["bash", str(tmp / "tools" / "clippy_ratchet.sh")],
            cwd=tmp,
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def test_all_crates_at_zero_baseline_pass(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(Path(tmp_dir))
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            for crate in (".", "alpha", "beta"):
                self.assertIn(f"clippy ratchet: {crate}: 0 warnings, baseline 0",
                              run.stdout)
            log = (Path(tmp_dir) / "cargo.env.log").read_text(encoding="utf-8")
            self.assertIn("clippy Cargo.toml", log)
            self.assertIn("clippy alpha/Cargo.toml", log)
            self.assertIn("clippy beta/Cargo.toml", log)

    def test_over_baseline_in_one_crate_fails_and_names_it(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(
                Path(tmp_dir),
                counts={"alpha/Cargo.toml": 2, "beta/Cargo.toml": 1},
            )
            self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
            self.assertIn("clippy ratchet: alpha: 2 warnings, baseline 0", run.stderr)
            self.assertIn("clippy ratchet: beta: 1 warnings, baseline 0", run.stderr)
            self.assertIn("clippy ratchet: .: 0 warnings, baseline 0", run.stdout)

    def test_colored_warnings_are_counted(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            run = self.run_ratchet(tmp, counts={"Cargo.toml": 3},
                                   extra_env={"FAKE_COMPACT_WARNING": "1"})
            self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
            self.assertIn("clippy ratchet: .: 4 warnings, baseline 0", run.stderr)
            self.assertNotIn("clippy ratchet: .: 0 warnings", run.stdout + run.stderr)

    def test_cargo_summary_lines_are_not_counted(self):
        # Real cargo prints "warning: `crate` (target) generated N warnings"
        # per target; counting those would make the ceiling drift with the
        # number of bins/tests a crate happens to have.
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(Path(tmp_dir), extra_env={"FAKE_SUMMARY": "1"})
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            self.assertIn("clippy ratchet: .: 0 warnings, baseline 0", run.stdout)

    def test_under_baseline_passes_and_suggests_lowering(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(
                Path(tmp_dir), baseline=[(".", 2), ("alpha", 1), ("beta", 0)])
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            self.assertIn("lower the '.' ceiling", run.stdout)

    def test_missing_baseline_entry_for_new_crate_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(
                Path(tmp_dir), crates=CRATES + ("gamma",),
                baseline=[(".", 0), ("alpha", 0), ("beta", 0)])
            self.assertEqual(run.returncode, 2)
            self.assertIn("crate 'gamma' has no ceiling", run.stderr)

    def test_stale_baseline_entry_for_removed_crate_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(
                Path(tmp_dir), crates=("alpha",),
                baseline=[(".", 0), ("alpha", 0), ("beta", 0)])
            self.assertEqual(run.returncode, 2)
            self.assertIn("baseline entry 'beta' matches no crate", run.stderr)

    def test_malformed_baseline_line_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            self.make_tree(tmp, baseline=[(".", 0), ("alpha", "x"), ("beta", 0)])
            bin_dir = tmp / "bin"
            bin_dir.mkdir(exist_ok=True)
            fake = bin_dir / "cargo"
            fake.write_text(FAKE_CARGO, encoding="utf-8")
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ)
            env["PATH"] = "%s%s%s" % (bin_dir, os.pathsep, env.get("PATH", ""))
            env["FAKE_COUNTS_FILE"] = str(tmp / "counts.txt")
            (tmp / "counts.txt").write_text("", encoding="utf-8")
            env["FAKE_CARGO_LOG"] = str(tmp / "cargo.env.log")
            run = subprocess.run(
                ["bash", str(tmp / "tools" / "clippy_ratchet.sh")],
                cwd=tmp, env=env, capture_output=True, text=True, timeout=60)
            self.assertEqual(run.returncode, 2)
            self.assertIn("malformed baseline line", run.stderr)

    def test_duplicate_baseline_entry_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            self.make_tree(tmp)
            baseline = tmp / "tools" / "clippy-warning-baseline.txt"
            baseline.write_text(". 0\n. 0\nalpha 0\nbeta 0\n", encoding="utf-8")
            bin_dir = tmp / "bin"
            bin_dir.mkdir(exist_ok=True)
            fake = bin_dir / "cargo"
            fake.write_text(FAKE_CARGO, encoding="utf-8")
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ)
            env["PATH"] = "%s%s%s" % (bin_dir, os.pathsep, env.get("PATH", ""))
            env["FAKE_COUNTS_FILE"] = str(tmp / "counts.txt")
            (tmp / "counts.txt").write_text("", encoding="utf-8")
            env["FAKE_CARGO_LOG"] = str(tmp / "cargo.env.log")
            run = subprocess.run(
                ["bash", str(tmp / "tools" / "clippy_ratchet.sh")],
                cwd=tmp, env=env, capture_output=True, text=True, timeout=60)
            self.assertEqual(run.returncode, 2)
            self.assertIn("duplicate baseline entry", run.stderr)

    def test_clippy_failure_fails_the_gate(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            run = self.run_ratchet(Path(tmp_dir), extra_env={"FAKE_CARGO_FAIL": "1"})
            self.assertEqual(run.returncode, 1)
            self.assertIn("cargo clippy failed", run.stderr)

    def test_missing_baseline_file_fails_loudly(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            self.make_tree(tmp, baseline=None)
            run = subprocess.run(
                ["bash", str(tmp / "tools" / "clippy_ratchet.sh")],
                cwd=tmp, capture_output=True, text=True, timeout=60)
            self.assertEqual(run.returncode, 2)
            self.assertIn("missing tools/clippy-warning-baseline.txt", run.stderr)

    def test_baseline_covers_every_real_crate(self):
        # The shipped baseline must name exactly the crates cargo discovers:
        # a crate added without a ceiling would otherwise ship ungated.
        discovered = {"."}
        for manifest in sorted(ROOT.glob("*/Cargo.toml")):
            discovered.add(manifest.parent.name)
        shipped = set()
        for raw in (ROOT / "tools" / "clippy-warning-baseline.txt").read_text().splitlines():
            line = raw.split("#", 1)[0].strip()
            if line:
                shipped.add(line.split()[0])
        self.assertEqual(discovered, shipped)


if __name__ == "__main__":
    unittest.main(verbosity=2)
