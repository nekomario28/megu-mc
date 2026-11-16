#!/usr/bin/env python3
"""Exercise gate exit/control contracts in an isolated tree with fake backends.

Two properties hold for every case and are asserted from what the fake backend saw, not
inferred from "the bytes came back":

* the campaign mutates a *private copy*. The fake cargo journals its own cwd, that cwd's
  `src/main.rs` hash and `CARGO_TARGET_DIR`, so a run that mutated the fixture tree itself, or
  built in the caller's target, shows up as a journal entry.
* the tree being measured is the working tree as it is: an uncommitted edit travels with the
  copy, and a source that changes while it is being copied stops the run.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
CONTROL = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored"
DIRTY = "// MC_SPINE_DIRTY_MARKER"
# The campaign refuses to start without room for a cold private target (1.5 GiB), and /tmp is a
# small tmpfs on some hosts, so the fixture's tree sits where the real snapshot parent sits and
# `TemporaryDirectory` removes it - including any residue a killed fixture left behind.
WORK = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "mc-spine"
WORK.mkdir(parents=True, exist_ok=True)


def journal_entries(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def assert_private_copy(root, before, journal, snapshot_parent, log, dirty):
    """The campaign's isolation contract, read off what the backend actually saw."""
    for entry in journal:
        assert Path(entry["cwd"]) != root, f"the campaign ran in the source tree: {entry}"
    runs = [e for e in journal if e["mutated"]]
    proved = re.search(r"mutation check: (\d+) proved", log)
    if proved and int(proved.group(1)) > 0:
        assert runs, "an entry was reported proved without any mutant run reaching the backend"
    for entry in runs:
        assert Path(entry["cwd"]).name.startswith("run."), entry
        assert entry["main"] != before["main.rs"], (
            f"the mutant bytes are not in the private copy: {entry}")
        assert entry["dirty_marker"] is dirty, (
            f"the copy does not carry the source's uncommitted state: {entry}")
        assert entry["target"], "cargo ran without a target dir"
        target = Path(entry["target"])
        assert target != root / "shared-target", "the campaign used the caller's shared target"
        assert Path(entry["cwd"]) in target.parents, (
            f"the build target is not inside the snapshot: {target}")
    return runs


def run_case(cargo_output, cargo_status, *, missing_anchor=False, leftover=False, wire_error=False,
             stale_artifact=False, empty_artifact=False, baseline_error=False, restored_error=False,
             wrong_failure=False, mutation_filter=None, repeat_output=None, stability=None,
             race_copy=False, dirty=False, sigkill=False, sleep_while_mutated=0.0):
    # The snapshot parent must live outside the tree it copies, so the fixture owns two paths:
    # the tree that is being measured and the directory the private copies go into.
    with tempfile.TemporaryDirectory(prefix="gate-selftest-", dir=WORK) as tmp, \
            tempfile.TemporaryDirectory(prefix="gate-snapshots-", dir=WORK) as snaps:
        root = Path(tmp)
        snapshot_parent = Path(snaps)
        for folder in ["tools", "src", "bin", "tests"]:
            (root / folder).mkdir()
        artifact = root / "alternate-target/debug/mc-spine"
        artifact.parent.mkdir(parents=True)
        artifact.write_text("fake executable")
        for name in ["mutation_check.sh", "cargo_executable.py"]:
            shutil.copy2(ROOT / "tools" / name, root / "tools" / name)
        script = root / "tools/mutation_check.sh"
        cli_calls = re.findall(r"tests/(\w+\.py) '([^']+)'", script.read_text())
        backends = {"client_settings_wire.py": "client settings wire: PASS",
                    "gameplay_wire.py": "gameplay wire: PASS",
                    "container_wire.py": "containers wire: PASS",
                    "population_plan_test.py": "population plan: PASS",
                    "mineshaft_dispatch_test.py": "mineshaft dispatch: PASS"}
        # Match identities, not a frozen count: a new CLI gate needs a fake backend too.
        registered = set(re.findall(r"tests/\w+\.py", script.read_text()))
        assert {"tests/" + name for name, _ in cli_calls} == registered
        assert {name for name, _ in cli_calls} == set(backends), "missing fake CLI backend"
        markers = [marker for _, marker in cli_calls]
        for name, passed in backends.items():
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
        for source in (ROOT / "src").glob("*.rs"):
            shutil.copy2(source, root / "src" / source.name)
        if dirty:
            with open(root / "src/main.rs", "a") as handle:
                handle.write("\n" + DIRTY + "\n")
        if missing_anchor:
            path = root / "src/main.rs"
            text = path.read_text()
            anchor = "        if let Some(slots) = chests().lock().unwrap().remove(&ContainerKey::Block((x, y, z))) {"
            assert text.count(anchor) == 1
            path.write_text(text.replace(anchor, "        if let Some(slots) = None::<HashMap<i16, Stack>> {", 1))
        if leftover:
            (root / "src/main.rs.mutation-backup").write_text("unrestored source")
        before = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        event = {"reason": "compiler-artifact", "target": {"name": "mc-spine", "kind": ["bin"]},
                 "executable": str(artifact)}
        if empty_artifact:
            event = {"reason": "build-finished", "success": True}
        journal = root / "journal.jsonl"
        cargo = root / "bin/cargo"
        cargo.write_text(f'''#!{sys.executable}
import hashlib, json, os, sys, time
from pathlib import Path

def sha(path):
    p = Path(path)
    return hashlib.sha256(p.read_bytes()).hexdigest() if p.exists() else None

mutated = bool(list(Path("src").glob("*.mutation-backup")))
seen = Path(".mutation-seen")
if mutated:
    seen.touch()
elif {baseline_error!r} or ({restored_error!r} and seen.exists()):
    print("positive control broken"); sys.exit(101)
is_build = sys.argv[1] == "build"
runs = Path(".mutation-runs")
count = int(runs.read_text()) if runs.exists() else 0
main = Path("src/main.rs")
if os.environ.get("GATE_JOURNAL"):
    with open(os.environ["GATE_JOURNAL"], "a") as handle:
        handle.write(json.dumps({{
            "cwd": os.getcwd(),
            "mutated": mutated,
            "target": os.environ.get("CARGO_TARGET_DIR"),
            "main": sha(main),
            "dirty_marker": ({DIRTY!r} in main.read_text()) if main.exists() else None,
            "argv": sys.argv[1:3],
        }}) + "\\n")
if mutated and not is_build:
    runs.write_text(str(count + 1))
if is_build and not (mutated and {cargo_output.startswith("error[")!r}):
    print(json.dumps({event!r})); sys.exit(0)
if not mutated:
    print({CONTROL!r}); sys.exit(0)
out = {(repeat_output if repeat_output is not None else cargo_output)!r} if count else {cargo_output!r}
print(out)
if {sleep_while_mutated!r}:
    time.sleep({sleep_while_mutated!r})
# The exit status follows the output the way cargo's does: a run that reports ok exits 0.
sys.exit(0 if "test result: ok." in out else {cargo_status})
''')
        cargo.chmod(0o755)
        if race_copy:
            # Stand in for another writer touching the source while the campaign copies it.
            racer = root / "bin/tar"
            racer.write_text(f'''#!/bin/sh
# Copy first, then change the source: this is a writer that lands between the campaign's two
# manifests, not a change the copy already carries.
/usr/bin/tar "$@"
status=$?
if [ ! -e "{root}/raced" ]; then
  : > "{root}/raced"
  printf '\\n// raced\\n' >> "{root}/src/main.rs"
fi
exit $status
''')
            racer.chmod(0o755)
        env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"])
        for name in ["MUTATION_FILTER", "MUTATION_STABILITY", "MUTATION_SNAPSHOT_PARENT",
                     "MUTATION_SNAPSHOT_ROOT"]:
            env.pop(name, None)
        if mutation_filter is not None:
            env["MUTATION_FILTER"] = mutation_filter
        if stability is not None:
            env["MUTATION_STABILITY"] = str(stability)
        # The snapshot and the build target must both live in the private copy, and the caller's
        # target must not be used, so the fixture offers one and the assertions require it back.
        env["MUTATION_SNAPSHOT_PARENT"] = str(snapshot_parent)
        env["CARGO_TARGET_DIR"] = str(root / "shared-target")
        env["GATE_JOURNAL"] = str(journal)
        argv = ["bash", "tools/mutation_check.sh", "--offline"]

        if sigkill:
            proc = subprocess.Popen(argv, cwd=root, env=env, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    start_new_session=True)
            deadline = time.time() + 30
            while time.time() < deadline:
                if any(e["mutated"] for e in journal_entries(journal)):
                    break
                if proc.poll() is not None:
                    break
                time.sleep(0.02)
            os.killpg(proc.pid, signal.SIGKILL)
            out, err = proc.communicate(timeout=30)
            log = out + err
            after = {p.name: hashlib.sha256(p.read_bytes()).digest()
                     for p in (root / "src").glob("*.rs")}
            assert before == after, "a killed run must not change the source tree"
            assert not list((root / "src").glob("*.mutation-backup"))
            kept = sorted(snapshot_parent.glob("run.*"))
            assert kept, "no private snapshot was retained to inspect"
            residue = kept[0] / "src/main.rs.mutation-backup"
            assert residue.exists(), f"the killed run should leave its residue here: {kept[0]}"
            assert (kept[0] / "src/main.rs").read_bytes() != (root / "src/main.rs").read_bytes(), \
                "the retained copy should hold the mutant"
            assert (kept[0] / "src/main.rs").stat().st_mtime > 0
            return proc.returncode, log, journal_entries(journal)

        result = subprocess.run(argv, cwd=root, env=env, capture_output=True, text=True,
                                timeout=180)
        log = result.stdout + result.stderr
        after = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        if not race_copy:
            assert before == after, "source bytes must be restored on every outcome"
        if not leftover:
            assert not list((root / "src").glob("*.mutation-backup")), "no mutation residue"
        left = sorted(p.name for p in snapshot_parent.glob("run.*"))
        assert not left, f"the snapshot was left behind by a completed run: {left}"
        journal = journal_entries(journal)
        assert_private_copy(root, before, journal, snapshot_parent, log, dirty)
        return result.returncode, log, journal


def main():
    failed = "running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored"
    green = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored"
    crash = "Segmentation fault (core dumped)"
    # Exactly one registered entry, so a stability scenario cannot bleed into the next entry.
    ONE = "sky_light_assembly_lights_through_an_opening"
    cases = [
        # Case 1 exercises all registered entries unfiltered against real src/*.rs anchors.
        ("assertion catches mutation", failed, 101, {}, True, "proved"),
        ("missing anchor", failed, 101,
         {"missing_anchor": True, "mutation_filter": "a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load"},
         False, "SKIP"),
        ("ignored test", "test result: ok. 0 passed; 0 failed; 1 ignored", 0,
         {"mutation_filter": "pickup_"}, False, "PARK"),
        ("zero matching tests", "running 0 tests\ntest result: ok. 0 passed; 0 failed", 0,
         {"mutation_filter": "pickup_"}, False, "matched no test"),
        ("mutant stays green", green, 0,
         {"mutation_filter": "pickup_"}, False, "stayed green"),
        ("compile failure", "error[E0425]: missing function", 101,
         {"mutation_filter": "pickup_"}, False, "did not compile"),
        ("leftover mutation", failed, 101, {"leftover": True}, False, "refusing to run"),
        ("wire harness error", failed, 101,
         {"wire_error": True, "mutation_filter": "tests/gameplay_wire.py"}, False, "baseline control"),
        ("guessed executable", failed, 101,
         {"stale_artifact": True, "mutation_filter": "tests/gameplay_wire.py"}, False, "baseline control"),
        ("missing Cargo artifact", failed, 101,
         {"empty_artifact": True, "mutation_filter": "tests/gameplay_wire.py"}, False, "received 0"),
        ("unrelated failure is not proof", failed, 101,
         {"wrong_failure": True, "mutation_filter": "tests/gameplay_wire.py"}, False, "neither compiled"),
        ("red baseline", failed, 101,
         {"baseline_error": True, "mutation_filter": "pickup_"}, False, "baseline control"),
        ("red restored control", failed, 101,
         {"restored_error": True, "mutation_filter": "pickup_"}, False, "restored control"),
        ("selected mutations", failed, 101, {"mutation_filter": "pickup_"}, True, "proved"),
        ("empty mutation selection", failed, 101, {"mutation_filter": "not-a-real-contract"}, False, "0 proved"),
        # The stability re-run is classified by the same owner as the first run, so a second run
        # that did not report a failing test is an unproven result, not a catch (measured
        # 2026-11-16: a compile error and a SIGSEGV on the re-run were both recorded as proved).
        ("stable catch: fails twice", failed, 101,
         {"mutation_filter": ONE, "stability": 1, "repeat_output": failed}, True, "proved"),
        ("unstable catch: second run green", failed, 101,
         {"mutation_filter": ONE, "stability": 1, "repeat_output": green}, False,
         "the failure was a flake"),
        ("second run compile error is unproven", failed, 101,
         {"mutation_filter": ONE, "stability": 1,
          "repeat_output": "error[E0308]: mismatched types"}, False, "unproven"),
        ("second run harness crash is unproven", failed, 101,
         {"mutation_filter": ONE, "stability": 1, "repeat_output": crash}, False, "unproven"),
        ("second run selects no test is unproven", failed, 101,
         {"mutation_filter": ONE, "stability": 1,
          "repeat_output": "running 0 tests\ntest result: ok. 0 passed; 0 failed"}, False, "unproven"),
        ("stability is not a truthiness test", failed, 101,
         {"mutation_filter": ONE, "stability": 2}, False, "must be 0 or 1"),
        ("stability 0 is off, not on", failed, 101,
         {"mutation_filter": ONE, "stability": 0, "repeat_output": green}, True, "proved"),
        # Isolation: the work happens in a copy, an uncommitted source edit travels with it, and a
        # writer touching the source mid-copy stops the run instead of mixing two revisions.
        ("uncommitted source edit is carried into the copy", failed, 101,
         {"mutation_filter": "pickup_", "dirty": True}, True, "proved"),
        ("source moved while copying is refused", failed, 101,
         {"mutation_filter": "pickup_", "race_copy": True}, False,
         "changed while it was being copied"),
    ]
    for name, output, status, kwargs, success, marker in cases:
        rc, log, _ = run_case(output, status, **kwargs)
        assert (rc == 0) == success, f"{name}: unexpected exit {rc}\n{log}"
        assert marker in log, f"{name}: failure reason is not visible\n{log}"
        print(f"gate self-test: {name} PASS")

    # A killed run is the case a trap cannot cover: the mutant and its backup stay in the private
    # snapshot, and the source tree is still the one that was there before.
    rc, log, journal = run_case(failed, 101, mutation_filter="pickup_",
                                sigkill=True, sleep_while_mutated=10.0)
    assert rc != 0, f"a killed run must not report success\n{log}"
    assert any(e["mutated"] for e in journal), "the kill landed before any mutant ran"
    print("gate self-test: sigkill leaves residue only in the private copy PASS")


if __name__ == "__main__":
    main()
