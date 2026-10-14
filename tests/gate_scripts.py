#!/usr/bin/env python3
"""Exercise the mutation gate's exit contract without compiling mutant binaries."""
import hashlib
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run_case(cargo_output, cargo_status, *, missing_anchor=False, leftover=False):
    with tempfile.TemporaryDirectory(prefix="spine-gate-") as tmp:
        root = Path(tmp)
        (root / "tools").mkdir()
        (root / "src").mkdir()
        (root / "bin").mkdir()
        shutil.copy2(ROOT / "tools/mutation_check.sh", root / "tools/mutation_check.sh")
        for name in ("main.rs", "feature.rs"):
            shutil.copy2(ROOT / "src" / name, root / "src" / name)
        if missing_anchor:
            # This anchor is a required mutation in the real manifest.
            path = root / "src/main.rs"
            text = path.read_text()
            anchor = "        if let Some(slots) = chests().lock().unwrap().remove(&(x, y, z)) {"
            assert text.count(anchor) == 1
            path.write_text(text.replace(anchor, "        if let Some(slots) = None::<HashMap<i16, Stack>> {", 1))
        if leftover:
            (root / "src/main.rs.mutation-backup").write_text("unrestored source")
        before = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        cargo = root / "bin/cargo"
        cargo.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(cargo_output) + "\nexit " + str(cargo_status) + "\n")
        cargo.chmod(0o755)
        env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"])
        result = subprocess.run(["bash", "tools/mutation_check.sh", "--offline"], cwd=root,
                                env=env, capture_output=True, text=True, timeout=60)
        after = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        assert before == after, "the mutation gate must restore source bytes on every outcome"
        return result.returncode, result.stdout + result.stderr


def main():
    failed = "running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored"
    cases = [
        ("assertion catches mutation", failed, 101, {}, True, "proved"),
        ("missing anchor", failed, 101, {"missing_anchor": True}, False, "SKIP"),
        ("ignored test", "test result: ok. 0 passed; 0 failed; 1 ignored", 0, {}, False, "PARK"),
        ("zero matching tests", "running 0 tests\ntest result: ok. 0 passed; 0 failed", 0, {}, False, "matched no test"),
        ("mutant stays green", "running 1 test\ntest result: ok. 1 passed; 0 failed", 0, {}, False, "stayed green"),
        ("compile failure", "error[E0425]: missing function", 101, {}, False, "did not compile"),
        ("leftover mutation", failed, 101, {"leftover": True}, False, "refusing to run"),
    ]
    for name, output, status, kwargs, success, marker in cases:
        rc, log = run_case(output, status, **kwargs)
        assert (rc == 0) == success, f"{name}: unexpected exit {rc}\n{log}"
        assert marker in log, f"{name}: failure reason is not visible\n{log}"
        print(f"gate self-test: {name} PASS")


if __name__ == "__main__":
    main()
