#!/usr/bin/env python3
"""Exercise gate exit/control contracts in an isolated tree with fake backends.

The properties every case asserts from what the fake backend saw, not from "the bytes came back":

* the campaign mutates a *private copy*. The fake cargo journals its own cwd, that cwd's
  `src/main.rs` hash, `CARGO_TARGET_DIR`, the wire config's hash and the presence of a probe list,
  so a run that mutated the fixture tree itself, built in the caller's target, or copied a file it
  said it excludes shows up as a journal entry.
* the tree being measured is the working tree as it is: an uncommitted edit travels with the copy,
  and a source that changes while it is being copied stops the run.
* the copy carries what the *server* reads from its cwd. `src/main.rs::load_config_packets` reads
  `config_packets.bin` from the working directory and the wire helper's `local_server` symlinks the
  repository's copy into a temporary server cwd, so a copy without it starts a server with empty
  registries and every wire gate measures different inputs. The fake wire backend resolves the
  config the way the real helper does and requires the bytes of this fixture.
* the campaign cannot be pointed back at the live tree: an inherited `MUTATION_SNAPSHOT_ROOT` and a
  snapshot parent that only *lexically* sits outside the source are both refused before anything is
  created or measured.
* a failed hash or enumeration is not a matching copy: a backend that prints a complete manifest and
  exits non-zero, or one that prints nothing and exits non-zero, stops the run before cargo is
  called.

`--script PATH` runs these cases against another copy of `mutation_check.sh` (the negative-control
harness: the pre-repair script, read out of git into a private fixture, must fail the cases added
2026-11-16). `--keep-going` reports every case instead of stopping at the first failure. Neither
option changes what the in-repo gate runs by default.
"""
import argparse
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
SCRIPT = ROOT / "tools/mutation_check.sh"
CONTROL = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored"
DIRTY = "// MC_SPINE_DIRTY_MARKER"
# The wire config the fixture plants: its bytes are what a copy has to carry, so the assertion names
# a digest rather than "a file exists".
WIRE_CONFIG = b"MCSPINE-CONFIG-" + bytes(range(96))
WIRE_SHA = hashlib.sha256(WIRE_CONFIG).hexdigest()
# Files the campaign must not copy even though they sit inside an input directory: the exclusion
# policy and the copy used to be separate (`find -type f` versus `tar` over the directory).
EXCLUDED = ["tools/target/cached.bin", "src/__pycache__/stray.pyc", "evidence/target/obj.o",
            "tests/cache.pyc", "tools/notes.mutation-backup"]
# Ordinary inputs, including the one the server reads from its cwd, that must travel with the copy.
# `resourcepack.zip` used to be a second one; it was removed as unprovenanced (THIRD_PARTY.md) and
# the server advertises no resource pack without it, so it is no longer a runtime input.
INCLUDED = ["Cargo.toml", "src/main.rs", "tools/mutation_check.sh", "assets/sentinel.json",
            "datapacks/sentinel.json", "evidence/sentinel.bin", "config_packets.bin"]
# The campaign refuses to start without room for a cold private target (1.5 GiB), and /tmp is a
# small tmpfs on some hosts, so the fixture's tree sits where the real snapshot parent sits and
# `TemporaryDirectory` removes it - including any residue a killed fixture left behind.
WORK = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "mc-spine"
WORK.mkdir(parents=True, exist_ok=True)


def journal_entries(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def assert_private_copy(root, before, journal, log, dirty):
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


def assert_copy_contents(journal, *, refusal=False):
    """What the copy holds, from the probe list the backend reported while it stood there."""
    for entry in journal:
        probe = entry["probe"]
        for path in EXCLUDED:
            assert not probe.get(path), (
                f"{path} was copied into {entry['cwd']} although it is excluded")
        for path in INCLUDED:
            assert probe.get(path), f"{path} is missing from the copy at {entry['cwd']}"
        assert entry["wire_sha"] == WIRE_SHA, (
            f"the copy's wire config is not this tree's bytes at {entry['cwd']}: "
            f"{entry['wire_sha']}")
    if refusal:
        assert not journal, f"a refused run must not reach cargo: {journal}"


def build_fixture(root, *, dirty=False, leftover=False, missing_anchor=False, symlink_input=False,
                  script=None):
    script = Path(script or SCRIPT)
    for folder in ["tools", "src", "bin", "tests", "assets", "datapacks", "evidence"]:
        (root / folder).mkdir()
    (root / "Cargo.toml").write_text('[package]\nname = "mc-spine"\nversion = "0.0.0"\n')
    (root / "config_packets.bin").write_bytes(WIRE_CONFIG)
    for path in EXCLUDED:
        (root / path).parent.mkdir(parents=True, exist_ok=True)
        (root / path).write_text("excluded sentinel\n")
    (root / "assets/sentinel.json").write_text('{"sentinel": "assets"}\n')
    (root / "datapacks/sentinel.json").write_text('{"sentinel": "datapacks"}\n')
    (root / "evidence/sentinel.bin").write_bytes(b"evidence-sentinel")
    for name in ["mutation_check.sh", "cargo_executable.py"]:
        shutil.copy2(script.parent / name, root / "tools" / name)
    for source in (ROOT / "src").glob("*.rs"):
        shutil.copy2(source, root / "src" / source.name)
    if dirty:
        with open(root / "src/main.rs", "a") as handle:
            handle.write("\n" + DIRTY + "\n")
    if missing_anchor:
        path = root / "src/main.rs"
        text = path.read_text()
        anchor = (
            "            if let Some(slots) = chests()\n"
            "                .lock()\n"
            "                .unwrap()\n"
            "                .remove(&ContainerKey::Block((x, y, z)))"
        )
        assert text.count(anchor) == 1
        path.write_text(
            text.replace(anchor, anchor.replace("(x, y, z)", "(x, y + 1000, z)"), 1)
        )
    if leftover:
        (root / "src/main.rs.mutation-backup").write_text("unrestored source")
    if symlink_input:
        (root / "outside").mkdir()
        (root / "outside/origin.rs").write_text("// origin: never ours to mutate\n")
        (root / "src/linked.rs").symlink_to("../outside/origin.rs")
    for path in EXCLUDED + INCLUDED:
        assert (root / path).exists(), f"the fixture is missing its probe sentinel: {path}"


def write_wire_backends(root, artifact, *, cargo_output, wire_error, wrong_failure, markers,
                        cargo_status):
    """The fake wire gates. Each one resolves the wire config the way the real helper does."""
    for name, passed in {"client_settings_wire.py": "client settings wire: PASS",
                         "gameplay_wire.py": "gameplay wire: PASS",
                         "dimension_wire.py": "dimension wire: PASS",
                         "container_wire.py": "containers wire: PASS",
                         "population_plan_test.py": "population plan: PASS",
                         "mineshaft_dispatch_test.py": "mineshaft dispatch: PASS"}.items():
        (root / "tests" / name).write_text(f'''import hashlib, os, sys, tempfile
from pathlib import Path
assert sys.argv[1] == {str(artifact)!r}
# The real helper's `ROOT` is derived from its own module path, so this asserts the same thing it
# does: the config the server will read comes from the copy the campaign is running in.
root = Path(__file__).resolve().parents[1]
config = root / "config_packets.bin"
if not config.is_file():
    print("wire config: MISSING under", root); sys.exit(3)
got = hashlib.sha256(config.read_bytes()).hexdigest()
if got != {WIRE_SHA!r}:
    print("wire config: WRONG BYTES", got); sys.exit(3)
if not root.name.startswith("run."):
    print("wire config: helper ROOT is not the private copy:", root); sys.exit(3)
linked = Path(tempfile.mkdtemp()) / "config_packets.bin"
linked.symlink_to(config)
resolved = Path(os.path.realpath(linked))
if resolved != config.resolve() or root.resolve() not in resolved.parents:
    print("wire config: the helper's link leaves the copy:", resolved); sys.exit(3)
print("wire config: PASS", got)
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


def write_cargo(root, artifact, *, cargo_output, cargo_status, empty_artifact, baseline_error,
                restored_error, repeat_output, sleep_while_mutated):
    probe = root / "bin/probe-paths.txt"
    probe.write_text("\n".join(EXCLUDED + INCLUDED) + "\n")
    event = {"reason": "compiler-artifact", "target": {"name": "mc-spine", "kind": ["bin"]},
             "executable": str(artifact)}
    if empty_artifact:
        event = {"reason": "build-finished", "success": True}
    cargo = root / "bin/cargo"
    cargo.write_text(f'''#!{sys.executable}
import hashlib, json, os, sys, time
from pathlib import Path

def sha(path):
    p = Path(path)
    return hashlib.sha256(p.read_bytes()).hexdigest() if p.exists() else None

probe = [line for line in Path({str(probe)!r}).read_text().splitlines() if line]
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
            "probe": {{p: Path(p).exists() for p in probe}},
            "wire_sha": sha("config_packets.bin"),
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


def write_copy_racer(root):
    """Stand in for another writer touching the source while the campaign copies it."""
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


def write_hash_failure(root, mode):
    """A deterministic failing hash backend: no unreadable file, no root privilege assumption."""
    shim = root / "bin/sha256sum"
    body = "/usr/bin/sha256sum \"$@\"\nexit 1\n" if mode == "status" else "exit 1\n"
    shim.write_text("#!/bin/sh\n" + body)
    shim.chmod(0o755)


def run_case(cargo_output, cargo_status, *, script=None, missing_anchor=False, leftover=False,
             wire_error=False, stale_artifact=False, empty_artifact=False, baseline_error=False,
             restored_error=False, wrong_failure=False, mutation_filter=None, repeat_output=None,
             stability=None, race_copy=False, dirty=False, sigkill=False, sleep_while_mutated=0.0,
             external_snapshot_root=False, snapshot_parent_alias=False, symlink_input=False,
             hash_fail=None, refusal=False, keep_logs=False):
    # The snapshot parent must live outside the tree it copies, so the fixture owns two paths:
    # the tree that is being measured and the directory the private copies go into.
    with tempfile.TemporaryDirectory(prefix="gate-selftest-", dir=WORK) as tmp, \
            tempfile.TemporaryDirectory(prefix="gate-snapshots-", dir=WORK) as snaps:
        root = Path(tmp)
        snapshot_parent = Path(snaps)
        script = Path(script or SCRIPT)
        artifact = root / "alternate-target/debug/mc-spine"
        artifact.parent.mkdir(parents=True)
        artifact.write_text("fake executable")
        build_fixture(root, dirty=dirty, leftover=leftover, missing_anchor=missing_anchor,
                      symlink_input=symlink_input, script=script)
        script_path = root / "tools/mutation_check.sh"
        cli_calls = re.findall(r"tests/(\w+\.py) '([^']+)'", script_path.read_text())
        # Match identities, not a frozen count: a new CLI gate needs a fake backend too.
        registered = set(re.findall(r"tests/\w+\.py", script_path.read_text()))
        assert {"tests/" + name for name, _ in cli_calls} == registered
        markers = [marker for _, marker in cli_calls]
        write_wire_backends(root, artifact, cargo_output=cargo_output, wire_error=wire_error,
                            wrong_failure=wrong_failure, markers=markers,
                            cargo_status=cargo_status)
        if stale_artifact:
            text = script_path.read_text()
            anchor = 'python3 "$filter" "$executable"'
            assert text.count(anchor) == 1
            script_path.write_text(text.replace(anchor, 'python3 "$filter" target/debug/mc-spine'))
        before = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        write_cargo(root, artifact, cargo_output=cargo_output, cargo_status=cargo_status,
                    empty_artifact=empty_artifact, baseline_error=baseline_error,
                    restored_error=restored_error, repeat_output=repeat_output,
                    sleep_while_mutated=sleep_while_mutated)
        if race_copy:
            write_copy_racer(root)
        if hash_fail:
            write_hash_failure(root, hash_fail)
        journal = root / "journal.jsonl"
        alias = None
        if snapshot_parent_alias:
            # Lexically outside the source, physically inside it: the alias is the whole point.
            (root / "cache").mkdir()
            alias = snapshot_parent / "alias"
            alias.symlink_to(root / "cache")
        env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"])
        for name in ["MUTATION_FILTER", "MUTATION_STABILITY", "MUTATION_SNAPSHOT_PARENT",
                     "MUTATION_SNAPSHOT_ROOT", "MUTATION_LOG_DIR"]:
            env.pop(name, None)
        if mutation_filter is not None:
            env["MUTATION_FILTER"] = mutation_filter
        if stability is not None:
            env["MUTATION_STABILITY"] = str(stability)
        # The snapshot and the build target must both live in the private copy, and the caller's
        # target must not be used, so the fixture offers one and the assertions require it back.
        env["MUTATION_SNAPSHOT_PARENT"] = str(alias if alias else snapshot_parent)
        env["CARGO_TARGET_DIR"] = str(root / "shared-target")
        env["GATE_JOURNAL"] = str(journal)
        if keep_logs:
            env["MUTATION_LOG_DIR"] = str(root / "mutation-logs")
        if external_snapshot_root:
            env["MUTATION_SNAPSHOT_ROOT"] = str(root)
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
            return proc.returncode, log, journal_entries(journal)

        result = subprocess.run(argv, cwd=root, env=env, capture_output=True, text=True,
                                timeout=180)
        log = result.stdout + result.stderr
        if keep_logs:
            log += "".join(p.read_text() for p in sorted((root / "mutation-logs").glob("*")))
        after = {p.name: hashlib.sha256(p.read_bytes()).digest() for p in (root / "src").glob("*.rs")}
        if not race_copy:
            assert before == after, "source bytes must be restored on every outcome"
        if not leftover:
            assert not list((root / "src").glob("*.mutation-backup")), "no mutation residue"
        left = sorted(p.name for p in snapshot_parent.glob("run.*"))
        assert not left, f"the snapshot was left behind by a completed run: {left}"
        if snapshot_parent_alias:
            assert not list((root / "cache").glob("run.*")), (
                "a snapshot parent aliased into the source tree must not be created")
            assert alias.is_symlink()
        journal = journal_entries(journal)
        if refusal:
            assert result.returncode == 2, f"a refused run must exit 2, got {result.returncode}\n{log}"
            # A refusal must not have touched the caller: no build in the offered target, and no
            # directory created inside a source-aliased snapshot parent.
            assert not (root / "shared-target").exists(), "a refused run created the caller's target"
            if snapshot_parent_alias:
                assert list((root / "cache").iterdir()) == [], (
                    "a refused run created a directory inside the source tree")
        assert_private_copy(root, before, journal, log, dirty)
        assert_copy_contents(journal, refusal=refusal)
        return result.returncode, log, journal


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--script", default=None,
                        help="mutation_check.sh to exercise (default: this tree's)")
    parser.add_argument("--keep-going", action="store_true",
                        help="report every case instead of stopping at the first failure")
    options = parser.parse_args()

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
        # Added 2026-11-16, after the review of the isolation repair.
        #
        # R1: the copy has to carry what the *server* reads from its cwd. The wire config was not in
        # the input set, so the wire gates baselined a server with empty registries while the tree
        # they claimed to measure had the blob. The backend names the digest, so "a file exists" is
        # not enough to pass.
        ("copied wire config is the source's bytes", failed, 101,
         {"mutation_filter": "tests/gameplay_wire.py", "keep_logs": True}, True,
         "wire config: PASS " + WIRE_SHA),
        # R2: the old recursion marker was an entry point - exporting it skipped the snapshot and put
        # the campaign back on the live tree with the caller's target.
        ("an inherited snapshot root is refused", failed, 101,
         {"external_snapshot_root": True, "refusal": True}, False, "not an entry"),
        # R3: a snapshot parent that is only lexically outside the source.
        ("snapshot parent symlinked into the source is refused", failed, 101,
         {"snapshot_parent_alias": True, "refusal": True}, False, "inside the tree it copies"),
        # R3: a symlink in the inputs would let the mutant be written through the link.
        ("a symlink in the inputs is refused", failed, 101,
         {"symlink_input": True, "refusal": True}, False, "contain symlinks"),
        # R4: a hash backend that fails is not a matching copy. Two shapes: a complete-looking
        # manifest with a non-zero status, and no output at all. The old script compared the empty
        # or partial answers and started the campaign.
        ("a failed hash status is not a matching copy", failed, 101,
         {"hash_fail": "status", "refusal": True}, False, "could not be taken"),
        ("an empty failed manifest is not a matching copy", failed, 101,
         {"hash_fail": "silent", "refusal": True}, False, "could not be taken"),
    ]
    failures = []
    for name, output, status, kwargs, success, marker in cases:
        kwargs = dict(kwargs)
        kwargs.setdefault("script", options.script)
        try:
            rc, log, _ = run_case(output, status, **kwargs)
            assert (rc == 0) == success, f"unexpected exit {rc}\n{log}"
            assert marker in log, f"failure reason is not visible\n{log}"
        except AssertionError as error:
            if not options.keep_going:
                raise
            failures.append(name)
            print(f"gate self-test: {name} FAIL\n{error}")
            continue
        print(f"gate self-test: {name} PASS")

    # A killed run is the case a trap cannot cover: the mutant and its backup stay in the private
    # snapshot, and the source tree is still the one that was there before.
    try:
        rc, log, journal = run_case(failed, 101, script=options.script, mutation_filter="pickup_",
                                    sigkill=True, sleep_while_mutated=10.0)
        assert rc != 0, f"a killed run must not report success\n{log}"
        assert any(e["mutated"] for e in journal), "the kill landed before any mutant ran"
    except AssertionError as error:
        if not options.keep_going:
            raise
        failures.append("sigkill leaves residue only in the private copy")
        print(f"gate self-test: sigkill leaves residue only in the private copy FAIL\n{error}")
    else:
        print("gate self-test: sigkill leaves residue only in the private copy PASS")

    if failures:
        print(f"gate self-test: {len(failures)} case(s) failed: {', '.join(failures)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
