#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Regression tests for ANSI-color handling in tools/clippy_ratchet.sh."""
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
echo "CARGO_TERM_COLOR=${CARGO_TERM_COLOR-}" >> "$FAKE_CARGO_LOG"
if [[ $# -ne 3 || "$1" != clippy || "$2" != --all-targets || "$3" != --locked ]]; then
  echo "unexpected args: $*" >&2
  exit 9
fi
for i in $(seq 1 "${FAKE_WARNINGS:-0}"); do
  printf '\\x1b[33mwarning\\x1b[0m: fake diagnostic %s\\n' "$i"
done
if [[ "${FAKE_COMPACT_WARNING:-0}" == 1 ]]; then
  printf '\\x1b[33mwarning\\x1b[0m:compact diagnostic\\n'
fi
exit 0
"""


class ClippyRatchetTest(unittest.TestCase):
    def run_ratchet(self, tmp, baseline, warnings, compact=False, with_baseline=True):
        tools = tmp / "tools"
        tools.mkdir()
        (tools / "clippy_ratchet.sh").write_bytes(SCRIPT.read_bytes())
        if with_baseline:
            (tools / "clippy-warning-baseline.txt").write_text(
                "# test baseline\n%d\n" % baseline,
                encoding="utf-8",
            )
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        fake_cargo = bin_dir / "cargo"
        fake_cargo.write_text(FAKE_CARGO, encoding="utf-8")
        fake_cargo.chmod(fake_cargo.stat().st_mode | stat.S_IXUSR)
        env = dict(os.environ)
        env["PATH"] = "%s%s%s" % (bin_dir, os.pathsep, env.get("PATH", ""))
        env["CARGO_TERM_COLOR"] = "always"
        env["FAKE_WARNINGS"] = str(warnings)
        env["FAKE_COMPACT_WARNING"] = "1" if compact else "0"
        env["FAKE_CARGO_LOG"] = str(tmp / "cargo.env.log")
        return subprocess.run(
            ["bash", str(tools / "clippy_ratchet.sh")],
            cwd=tmp,
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def test_colored_warnings_are_counted_and_breach_the_baseline(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            run = self.run_ratchet(tmp, baseline=2, warnings=3)
            self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
            self.assertIn("clippy ratchet: 3 warnings, baseline 2", run.stderr)
            self.assertNotIn("clippy ratchet: 0 warnings", run.stdout + run.stderr)

    def test_colored_warnings_at_baseline_pass_and_color_is_forced_off(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            run = self.run_ratchet(tmp, baseline=3, warnings=3)
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            self.assertIn("clippy ratchet: 3 warnings, baseline 3", run.stdout)
            self.assertEqual(
                (tmp / "cargo.env.log").read_text(encoding="utf-8").strip(),
                "CARGO_TERM_COLOR=never",
            )

    def test_colored_warning_without_space_after_colon_is_counted(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            run = self.run_ratchet(tmp, baseline=0, warnings=0, compact=True)
            self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
            self.assertIn("clippy ratchet: 1 warnings, baseline 0", run.stderr)

    def test_missing_baseline_file_fails_loudly(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp = Path(tmp_dir)
            run = self.run_ratchet(tmp, baseline=0, warnings=0, with_baseline=False)
            self.assertEqual(run.returncode, 2)
            self.assertIn("missing tools/clippy-warning-baseline.txt", run.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
