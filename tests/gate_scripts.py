#!/usr/bin/env python3
"""Exercise gate exit/control contracts in an isolated tree with fake backends."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
CONTROL = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored"


def run_case(cargo_output, cargo_status, *, missing_anchor=False, leftover=False, wire_error=False,
             stale_artifact=False, empty_artifact=False, baseline_error=False, restored_error=False,
             wrong_failure=False):
    with tempfile.TemporaryDirectory(prefix="spine-gate-") as tmp:
        root = Path(tmp)
        for folder in ["tools", "src", "bin", "tests"]:
            (root / folder).mkdir()
        artifact = root / "alternate-target/debug/mc-spine"
        artifact.parent.mkdir(parents=True)
        artifact.write_text("fake executable")
        for name in ["mutation_check.sh", "cargo_executable.py"]:
            shutil.copy2(ROOT / "tools" / name, root / "tools" / name)
        script = root / "tools/mutation_check.sh"
        markers = re.findall(r"tests/(?:client_settings|gameplay)_wire.py '([^']+)'", script.read_text())
        assert len(markers) == 6, "every wire mutation needs an exact failure identity"
        for name, passed in [("client_settings_wire.py", "client settings wire: PASS"),
                             ("gameplay_wire.py", "gameplay wire: PASS")]:
            (root / "tests" / name).write_text(f'''import sys
from pathlib import Path
assert sys.argv[1] == {str(artifact)!r}
if {wire_error!r}:
    print("FileNotFoundError: broken harness"); sys.exit(1)
if not list(Path("src").glob("*.mutation-backup")):
    print({passed!r}); sys.exit(0)
if "test result: FAILED" in {cargo_output!r}:
    print("gameplay wire: FAIL unrelated-assertion" if {wrong_failure!r} else "\\n".join({markers!r}))
else:
    print({cargo_output!r})
sys.exit({cargo_status})
''')
        if stale_artifact:
            text = script.read_text()
            anchor = 'python3 "$filter" "$executable"'
            assert text.count(anchor) == 1
            script.write_text(text.replace(anchor, 'python3 "$filter" target/debug/mc-spine'))
        for name in ("main.rs", "feature.rs"):
            shutil.copy2(ROOT / "src" / name, root / "src" / name)
        if missing_anchor:
            path = root / "src/main.rs"
            text = path.read_text()
            anchor = "        if let Some(slots) = chests().lock().unwrap().remove(&(x, y, z)) {"
            assert text.count(anchor) == 1
            path.write_text(text.replace(anchor, "        if let Some(slots) = None::<HashMap<i16, Stack>> {", 1))
        if leftover:
            (root / "src/main.rs.mutation-backup").write_text("unrestored source")
        before = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        event = {"reason": "compiler-artifact", "target": {"name": "mc-spine", "kind": ["bin"]},
                 "executable": str(artifact)}
        if empty_artifact:
            event = {"reason": "build-finished", "success": True}
        cargo = root / "bin/cargo"
        cargo.write_text(f'''#!{sys.executable}
import sys,json
from pathlib import Path
mutated = bool(list(Path("src").glob("*.mutation-backup")))
seen = Path(".mutation-seen")
if mutated:
    seen.touch()
elif {baseline_error!r} or ({restored_error!r} and seen.exists()):
    print("positive control broken"); sys.exit(101)
if sys.argv[1] == "build" and not (mutated and {cargo_output.startswith("error[")!r}):
    print(json.dumps({event!r})); sys.exit(0)
print({cargo_output!r} if mutated else {CONTROL!r})
sys.exit({cargo_status} if mutated else 0)
''')
        cargo.chmod(0o755)
        env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"])
        result = subprocess.run(["bash", "tools/mutation_check.sh", "--offline"], cwd=root,
                                env=env, capture_output=True, text=True, timeout=90)
        after = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        assert before == after, "source bytes must be restored on every outcome"
        if not leftover:
            assert not list((root / "src").glob("*.mutation-backup")), "no mutation residue"
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
        ("wire harness error", failed, 101, {"wire_error": True}, False, "baseline control"),
        ("guessed executable", failed, 101, {"stale_artifact": True}, False, "baseline control"),
        ("missing Cargo artifact", failed, 101, {"empty_artifact": True}, False, "received 0"),
        ("unrelated failure is not proof", failed, 101, {"wrong_failure": True}, False, "neither compiled"),
        ("red baseline", failed, 101, {"baseline_error": True}, False, "baseline control"),
        ("red restored control", failed, 101, {"restored_error": True}, False, "restored control"),
    ]
    for name, output, status, kwargs, success, marker in cases:
        rc, log = run_case(output, status, **kwargs)
        assert (rc == 0) == success, f"{name}: unexpected exit {rc}\n{log}"
        assert marker in log, f"{name}: failure reason is not visible\n{log}"
        print(f"gate self-test: {name} PASS")


if __name__ == "__main__":
    main()
