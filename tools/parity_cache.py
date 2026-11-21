#!/usr/bin/env python3
"""Content-hashed oracle result cache + changed-files harness mapper.

The idea is borrowed from Horizon's `parity.py` / `parity_shim.py` (see
`evidence/foundation/horizon-reuse/README.md`, verdict BORROW_CONCEPT): an oracle's
recorded output is keyed on the oracle name, the sha256 of every file it depends
on, the exact command line and an optional pinned jar sha1. Iterating on Rust
code then replays a Java oracle's stored output instead of paying for a JVM run
whose inputs did not move. The second half maps a changed-file list to the exact
offline harnesses that can observe those paths.

  tools/parity_cache.py run --name <oracle> --dep <file>...
                            [--jar-sha1 <sha1>] [--cache-dir <dir>] [--fresh]
                            -- <cmd...>
  tools/parity_cache.py map --paths <p>... [--changed-files <file>]
                            [--git-diff <ref>] [--json]
  tools/parity_cache.py --self-test

Cache entries live one JSON document per file under the cache dir (default
`$XDG_CACHE_HOME/mc-spine/parity-cache`, falling back to
`~/.cache/mc-spine/parity-cache`) and are written atomically (temp file +
rename). Captured output is decoded UTF-8 with replacement, so a cache hit can
differ from a live run only for non-UTF-8 bytes.

Fail-closed rules
  - a non-zero exit is never cached;
  - a corrupted, truncated, schema-mismatched or digest-mismatched entry is a
    miss and is re-executed cleanly — bogus cached output is never returned;
  - an unreadable dependency makes the key untrustworthy, so the command runs
    uncached instead of gambling on a partial hash;
  - --fresh always bypasses the cache and overwrites the entry on exit 0.

Exit status for `run` is the wrapped command's status; `map` exits 0 on success;
usage errors exit 2. Pure Python 3 stdlib.
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = 1

STANDALONE_CRATES = (
    "client-port",
    "client-kernel",
    "client-app",
    "mod-offload",
    "mod-adapter-core",
    "create-adapter",
    "proto",
    "state-store",
    "server-files",
    "core",
)

# Extra harnesses a crate change can affect beyond its own cargo test.
CRATE_EXTRA_HARNESS = {
    "client-port": ("python3 tests/culling_perf_gate.py --self-test",),
    "client-app": ("python3 tests/client_fork_e2e.py --self-test",),
    "mod-adapter-core": ("python3 tests/mod_fallback_test.py",),
}

# Direct consumers in the crate DAG: editing a shared leaf crate invalidates the
# crates built on it, so their harnesses run as if they had changed too.
# mc-spine-core is the zero-dependency leaf (see core/Cargo.toml); its direct
# consumers are the five crates whose manifest names mc-spine-core.
CRATE_DOWNSTREAM = {
    "core": ("proto", "state-store", "server-files", "mod-adapter-core",
             "client-kernel"),
}

# Gates every src/** change must pass.
SRC_GATES = (
    "bash tools/layers_check.sh",
    "bash tools/debt_ratchet.sh",
    "python3 tools/provenance_check.py",
)

RUST_SUITE = "cargo test --offline --locked -j1"

# src files whose changes are observed by a focused Rust test filter, cheaper
# than the full suite while iterating.
FOCUSED_RUST = {
    "src/anvil_io.rs": ("anvil_io",),
    "src/server_reliability.rs": ("server_reliability_tests",),
    "src/operator_ux.rs": ("operator_ux_tests",),
    "src/worldgen.rs": ("generation_contract_tests",),
    "src/mineshaft.rs": ("mineshaft_tests",),
    "src/mineshaft_layout.rs": ("mineshaft_tests",),
    "src/mineshaft_piece.rs": ("mineshaft_tests",),
    "src/brigadier.rs": ("brigadier_tests",),
    "src/brigadier_wire.rs": ("brigadier_wire_tests",),
    "src/minecart.rs": ("minecart_tests",),
    "src/spawner.rs": ("spawner_tests",),
    "src/natural_spawner.rs": ("spawner_tests",),
    "src/tick_queue.rs": ("tick_tests",),
    "src/scheduled_ticks.rs": ("tick_tests",),
    "src/save_v2.rs": ("persistence_tests",),
    "src/tick_persistence.rs": ("persistence_tests",),
    "src/block_registry.rs": ("registry_tests",),
    "src/entity_registry.rs": ("registry_tests",),
}


def crate_test(crate):
    return f"cargo test --manifest-path {crate}/Cargo.toml --offline --locked -j1"


def focused_test(name_filter):
    return f"cargo test --offline --locked -j1 {name_filter}"


def default_cache_dir():
    base = os.environ.get("XDG_CACHE_HOME")
    root = Path(base) if base else Path.home() / ".cache"
    return root / "mc-spine" / "parity-cache"


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cache_key(oracle_name, deps, argv, jar_sha1=None):
    """Deterministic key over (oracle, sorted dep hashes, argv, jar sha1)."""
    material = {
        "schema": SCHEMA,
        "oracle": oracle_name,
        "deps": sorted([rel, digest] for rel, digest in deps),
        "argv": list(argv),
        "jar_sha1": jar_sha1,
    }
    blob = json.dumps(material, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


def payload_digest(stdout, stderr, returncode):
    blob = json.dumps({"stdout": stdout, "stderr": stderr, "returncode": returncode},
                      sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


def _execute(argv):
    """-> (returncode, stdout, stderr); output decoded UTF-8/replace."""
    try:
        proc = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True)
    except OSError as exc:
        return 127, "", f"{type(exc).__name__}: {exc}"
    return (proc.returncode,
            proc.stdout.decode("utf-8", errors="replace"),
            proc.stderr.decode("utf-8", errors="replace"))


class OracleCache:
    def __init__(self, cache_dir=None):
        self.cache_dir = Path(cache_dir) if cache_dir else default_cache_dir()

    def entry_path(self, key):
        return self.cache_dir / f"{key}.json"

    def get(self, key):
        """Validated entry or None — every corruption mode is a miss."""
        try:
            entry = json.loads(self.entry_path(key).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        if not isinstance(entry, dict):
            return None
        if entry.get("schema") != SCHEMA or entry.get("key") != key:
            return None
        stdout, stderr, returncode = (entry.get("stdout"), entry.get("stderr"),
                                      entry.get("returncode"))
        if (not isinstance(stdout, str) or not isinstance(stderr, str)
                or not isinstance(returncode, int) or returncode != 0):
            return None
        if entry.get("sha256") != payload_digest(stdout, stderr, returncode):
            return None
        return entry

    def put(self, key, oracle_name, stdout, stderr, returncode):
        """Atomic store; non-zero exits are refused and write errors are non-fatal."""
        if returncode != 0:
            return False
        entry = {"schema": SCHEMA, "key": key, "oracle_name": oracle_name,
                 "stdout": stdout, "stderr": stderr, "returncode": returncode,
                 "sha256": payload_digest(stdout, stderr, returncode)}
        try:
            self.cache_dir.mkdir(parents=True, exist_ok=True)
            fd, tmp = tempfile.mkstemp(prefix=".entry-", dir=str(self.cache_dir))
            try:
                with os.fdopen(fd, "w", encoding="utf-8") as handle:
                    json.dump(entry, handle, sort_keys=True)
                os.replace(tmp, self.entry_path(key))
            except BaseException:
                try:
                    os.unlink(tmp)
                except OSError:
                    pass
                raise
        except OSError:
            return False
        return True

    def _dep_hashes(self, dep_paths):
        """(hashed (rel, sha) pairs, unreadable rel paths).

        Relative deps resolve against the caller's cwd, matching where the
        wrapped command itself runs.
        """
        hashed, missing = [], []
        for rel in dep_paths:
            path = Path(rel).expanduser()
            if not path.is_absolute():
                path = Path.cwd() / path
            try:
                hashed.append((rel, sha256_file(path)))
            except OSError:
                missing.append(rel)
        return hashed, missing

    def run(self, oracle_name, dep_paths, argv, jar_sha1=None, fresh=False):
        """-> (returncode, stdout, stderr, cache_hit, note)."""
        deps, missing = self._dep_hashes(dep_paths)
        if missing:
            returncode, stdout, stderr = _execute(argv)
            return (returncode, stdout, stderr, False,
                    "parity_cache: dep unreadable, ran uncached: " + ", ".join(missing))
        key = cache_key(oracle_name, deps, argv, jar_sha1)
        if not fresh:
            entry = self.get(key)
            if entry is not None:
                return (0, entry["stdout"], entry["stderr"], True,
                        f"parity_cache: hit {key[:12]} ({oracle_name})")
        returncode, stdout, stderr = _execute(argv)
        if returncode == 0:
            self.put(key, oracle_name, stdout, stderr, returncode)
            note = f"parity_cache: miss, stored {key[:12]} ({oracle_name})"
        else:
            note = f"parity_cache: ran, exit {returncode} is never cached ({oracle_name})"
        return returncode, stdout, stderr, False, note


def map_changed_files(paths):
    """Sorted harness commands covering the given repo-relative paths."""
    commands = set()
    for raw in paths:
        path = raw.strip().replace("\\", "/")
        while path.startswith("./"):
            path = path[2:]
        if not path:
            continue
        first = path.split("/", 1)[0]
        if first in STANDALONE_CRATES:
            for crate in (first,) + CRATE_DOWNSTREAM.get(first, ()):
                commands.add(crate_test(crate))
                commands.update(CRATE_EXTRA_HARNESS.get(crate, ()))
        elif path == "tools/mod_launcher.py":
            commands.add("python3 tests/mod_fallback_test.py")
        elif path == "tools/release_bundle.py":
            commands.add("python3 tools/release_bundle.py --self-test")
        elif path == "tools/export_public_branch.sh":
            commands.add("python3 tests/public_export_test.py")
        elif path == "tests/companion_suite_e2e.py":
            commands.add("python3 tests/companion_suite_e2e.py --self-test")
        elif path == "docker" or path.startswith("docker/"):
            commands.add("bash -n docker/entrypoint.sh")
            commands.add("python3 tools/provenance_check.py")
        elif path == "tools/vanilla_data" or path.startswith("tools/vanilla_data/"):
            commands.add("python3 tools/vanilla_data/version_pack.py --self-test")
        elif path == "website" or path.startswith("website/"):
            commands.add("python3 website/check_site.py")
            commands.add("python3 tools/provenance_check.py")
        elif path == "src" or path.startswith("src/"):
            commands.update(SRC_GATES)
            base = path.rsplit("/", 1)[-1]
            if base.endswith("_tests.rs"):
                commands.add(focused_test(base[:-len(".rs")]))
            elif path in FOCUSED_RUST:
                commands.update(focused_test(name) for name in FOCUSED_RUST[path])
            else:
                commands.add(RUST_SUITE)
    return sorted(commands)


def changed_files_from_git(ref):
    proc = subprocess.run(["git", "-C", str(ROOT), "diff", "--name-only", ref, "--"],
                          capture_output=True, text=True)
    if proc.returncode != 0:
        raise SystemExit(f"parity_cache: git diff --name-only {ref} failed: "
                         f"{proc.stderr.strip() or 'exit ' + str(proc.returncode)}")
    return proc.stdout.splitlines()


def read_paths_file(path):
    return [line.strip() for line in Path(path).read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")]


def _flatten(groups):
    return [item for group in groups for item in group]


def build_parser():
    parser = argparse.ArgumentParser(
        prog="parity_cache.py",
        description="Content-hashed oracle cache + changed-files harness mapper.")
    parser.add_argument("--self-test", action="store_true",
                        help="run the offline unit tests and exit")
    sub = parser.add_subparsers(dest="mode")

    run_p = sub.add_parser("run", help="run a command through the oracle cache")
    run_p.add_argument("--name", required=True, help="oracle name (part of the key)")
    run_p.add_argument("--dep", action="append", nargs="+", default=[], metavar="FILE",
                       help="input file(s) hashed into the key; repeatable")
    run_p.add_argument("--jar-sha1", help="optional pinned jar sha1 mixed into the key")
    run_p.add_argument("--cache-dir", help="override the cache directory")
    run_p.add_argument("--fresh", action="store_true",
                       help="bypass the cache and overwrite the entry on exit 0")

    map_p = sub.add_parser("map", help="map changed paths to covering harnesses")
    map_p.add_argument("--paths", nargs="*", default=[], metavar="PATH",
                       help="repo-relative changed paths")
    map_p.add_argument("--changed-files", metavar="FILE",
                       help="newline-separated file of changed paths")
    map_p.add_argument("--git-diff", metavar="REF",
                       help="take changed paths from `git diff --name-only REF`")
    map_p.add_argument("--json", action="store_true", help="emit a JSON array")
    return parser


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    command = None
    if "--" in argv:
        index = argv.index("--")
        command = argv[index + 1:]
        argv = argv[:index]
    args = build_parser().parse_args(argv)
    if args.self_test:
        return self_test()
    if args.mode == "run":
        if not command:
            print("parity_cache: run needs a command after '--'", file=sys.stderr)
            return 2
        cache = OracleCache(args.cache_dir)
        returncode, stdout, stderr, _hit, note = cache.run(
            args.name, _flatten(args.dep), command,
            jar_sha1=args.jar_sha1, fresh=args.fresh)
        sys.stdout.write(stdout)
        sys.stderr.write(stderr)
        if note:
            print(note, file=sys.stderr)
        return returncode
    if args.mode == "map":
        paths = list(args.paths)
        if args.changed_files:
            paths += read_paths_file(args.changed_files)
        if args.git_diff:
            paths += changed_files_from_git(args.git_diff)
        if not paths:
            print("parity_cache: map needs --paths, --changed-files or --git-diff",
                  file=sys.stderr)
            return 2
        mapped = set()
        for raw in paths:
            if map_changed_files([raw]):
                mapped.add(raw.strip())
        unmapped = [raw for raw in paths
                    if raw.strip() and raw.strip() not in mapped]
        commands = map_changed_files(paths)
        if args.json:
            print(json.dumps(commands, indent=2))
        else:
            for line in commands:
                print(line)
        if unmapped:
            print("parity_cache: no harness mapping for: " + ", ".join(sorted(unmapped)),
                  file=sys.stderr)
        return 0
    build_parser().print_help()
    return 2


# --------------------------------------------------------------------------
# self-test


class CacheContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.cache = OracleCache(self.dir / "cache")
        self.dep = self.dir / "dep.txt"
        self.dep.write_text("v1\n")
        self.counter = self.dir / "count.txt"

    def oracle_cmd(self, exit_code=0, text="oracle-out"):
        body = ("import pathlib,sys\n"
                f"p=pathlib.Path({str(self.counter)!r})\n"
                "n=int(p.read_text())+1 if p.exists() else 1\n"
                "p.write_text(str(n))\n"
                f"print({text!r})\n"
                "print('oracle-err', file=sys.stderr)\n"
                f"sys.exit({exit_code})\n")
        return [sys.executable, "-c", body]

    def count(self):
        return self.counter.read_text() if self.counter.exists() else "0"

    def run_it(self, **kw):
        return self.cache.run("test-oracle", [str(self.dep)], self.oracle_cmd(), **kw)

    def test_key_is_deterministic_and_inputs_sensitive(self):
        deps = [("a.txt", "0" * 64), ("b.txt", "1" * 64)]
        key1 = cache_key("o", deps, ["x", "y"])
        key2 = cache_key("o", list(reversed(deps)), ["x", "y"])
        self.assertEqual(key1, key2, "dep order must not change the key")
        self.assertNotEqual(key1, cache_key("o", deps, ["x", "z"]), "argv is keyed")
        self.assertNotEqual(key1, cache_key("o", deps, ["x", "y"], jar_sha1="f" * 40),
                            "jar sha1 is keyed")
        self.assertNotEqual(key1, cache_key("other", deps, ["x", "y"]),
                            "oracle name is keyed")

    def test_cache_hit_skips_the_second_execution(self):
        rc, out, err, hit, _ = self.run_it()
        self.assertEqual((rc, out, err, hit), (0, "oracle-out\n", "oracle-err\n", False))
        rc, out, err, hit, _ = self.run_it()
        self.assertEqual(hit, True)
        self.assertEqual(out, "oracle-out\n")
        self.assertEqual(err, "oracle-err\n")
        self.assertEqual(self.count(), "1", "a hit must not re-execute the command")

    def test_dep_modification_invalidates_the_entry(self):
        self.run_it()
        self.dep.write_text("v2\n")
        rc, _, _, hit, _ = self.run_it()
        self.assertEqual(rc, 0)
        self.assertFalse(hit, "changed dep content must miss")
        self.assertEqual(self.count(), "2")
        rc, _, _, hit, _ = self.run_it()
        self.assertTrue(hit)
        self.assertEqual(self.count(), "2")

    def test_fresh_always_reruns_and_overwrites(self):
        self.run_it()
        rc, out, _, hit, _ = self.run_it(fresh=True)
        self.assertEqual((rc, hit), (0, False))
        self.assertEqual(self.count(), "2")
        rc, _, _, hit, _ = self.run_it()
        self.assertTrue(hit)
        self.assertEqual(self.count(), "2")

    def test_failing_commands_are_never_cached(self):
        argv = self.oracle_cmd(exit_code=3)
        deps, _ = self.cache._dep_hashes([str(self.dep)])
        key = cache_key("t", deps, argv)
        rc1 = self.cache.run("t", [str(self.dep)], argv)
        rc2 = self.cache.run("t", [str(self.dep)], argv)
        self.assertEqual((rc1[0], rc2[0]), (3, 3))
        self.assertEqual(self.count(), "2", "a failure must run again, never replay")
        self.assertIsNone(self.cache.get(key), "a non-zero exit must leave no entry")

    def _stored_entry_path(self):
        entries = list((self.dir / "cache").glob("*.json"))
        self.assertEqual(len(entries), 1)
        return entries[0]

    def test_trucated_and_garbage_entries_fail_closed(self):
        self.run_it()
        path = self._stored_entry_path()
        path.write_text('{"schema": 1, "key": "')
        _, _, _, hit, _ = self.run_it()
        self.assertFalse(hit, "truncated JSON must re-run")
        self.assertEqual(self.count(), "2")
        self._stored_entry_path().write_text("not json at all")
        _, _, _, hit, _ = self.run_it()
        self.assertFalse(hit)
        self.assertEqual(self.count(), "3")

    def test_tampered_payload_is_a_miss_not_bogus_output(self):
        self.run_it()
        path = self._stored_entry_path()
        entry = json.loads(path.read_text())
        entry["stdout"] = "FORGED\n"
        path.write_text(json.dumps(entry))
        rc, out, _, hit, _ = self.run_it()
        self.assertFalse(hit, "a digest mismatch must not be served")
        self.assertEqual(out, "oracle-out\n", "the re-run replaces the forged text")
        self.assertEqual(self.count(), "2")
        rc, out, _, hit, _ = self.run_it()
        self.assertTrue(hit)
        self.assertEqual(out, "oracle-out\n")

    def test_wrong_key_or_schema_entry_is_a_miss(self):
        self.run_it()
        path = self._stored_entry_path()
        entry = json.loads(path.read_text())
        entry["schema"] = 999
        path.write_text(json.dumps(entry))
        _, _, _, hit, _ = self.run_it()
        self.assertFalse(hit)

    def test_unreadable_dep_runs_uncached(self):
        ghost = str(self.dir / "absent.bin")
        for _ in range(2):
            rc, _, _, hit, note = self.cache.run("o", [ghost], self.oracle_cmd())
            self.assertEqual((rc, hit), (0, False))
            self.assertIn("uncached", note)
        self.assertEqual(self.count(), "2")
        self.assertFalse(list((self.dir / "cache").glob("*.json")),
                         "an unhashable input must not leave an entry")


class MapContract(unittest.TestCase):
    def test_every_standalone_crate_maps_to_its_own_cargo_test(self):
        self.assertEqual(len(STANDALONE_CRATES), 10)
        for crate in STANDALONE_CRATES:
            commands = map_changed_files([f"{crate}/src/lib.rs"])
            self.assertIn(crate_test(crate), commands, crate)

    def test_core_change_reaches_its_five_direct_downstream_crates(self):
        commands = map_changed_files(["core/src/lib.rs"])
        self.assertIn(crate_test("core"), commands)
        for crate in ("proto", "state-store", "server-files", "mod-adapter-core",
                      "client-kernel"):
            self.assertIn(crate_test(crate), commands, crate)
        # Downstream crates get their full harness treatment, not just cargo test.
        self.assertIn("python3 tests/mod_fallback_test.py", commands)
        # Crates that do not depend on core stay out of the mapping.
        for crate in ("client-port", "client-app", "mod-offload", "create-adapter"):
            self.assertNotIn(crate_test(crate), commands, crate)

    def test_release_docker_and_companion_mappings(self):
        self.assertEqual(map_changed_files(["tools/release_bundle.py"]),
                         ["python3 tools/release_bundle.py --self-test"])
        self.assertEqual(map_changed_files(["tools/export_public_branch.sh"]),
                         ["python3 tests/public_export_test.py"])
        self.assertEqual(map_changed_files(["tests/companion_suite_e2e.py"]),
                         ["python3 tests/companion_suite_e2e.py --self-test"])
        docker = {"bash -n docker/entrypoint.sh",
                  "python3 tools/provenance_check.py"}
        self.assertEqual(set(map_changed_files(["docker/Dockerfile"])), docker)
        self.assertEqual(set(map_changed_files(["docker/entrypoint.sh"])), docker)

    def test_crate_extra_harnesses(self):
        self.assertIn("python3 tests/culling_perf_gate.py --self-test",
                      map_changed_files(["client-port/src/lib.rs"]))
        self.assertIn("python3 tests/client_fork_e2e.py --self-test",
                      map_changed_files(["client-app/src/lib.rs"]))
        self.assertIn("python3 tests/mod_fallback_test.py",
                      map_changed_files(["mod-adapter-core/src/lib.rs"]))

    def test_website_paths_map_to_the_site_gate_and_provenance(self):
        expected = {"python3 website/check_site.py",
                    "python3 tools/provenance_check.py"}
        self.assertEqual(set(map_changed_files(["website/app.js"])), expected)
        self.assertEqual(set(map_changed_files(["website/index.html"])), expected)
        self.assertEqual(set(map_changed_files(["website/check_site.py"])),
                         expected)
        self.assertEqual(set(map_changed_files(["website"])), expected)

    def test_tools_paths(self):
        self.assertEqual(map_changed_files(["tools/vanilla_data/build_vanilla_data.py"]),
                         ["python3 tools/vanilla_data/version_pack.py --self-test"])
        self.assertEqual(map_changed_files(["tools/mod_launcher.py"]),
                         ["python3 tests/mod_fallback_test.py"])

    def test_src_paths_get_gates_and_focused_hints(self):
        commands = map_changed_files(["src/worldgen.rs"])
        for gate in SRC_GATES:
            self.assertIn(gate, commands)
        self.assertIn(focused_test("generation_contract_tests"), commands)
        self.assertNotIn(RUST_SUITE, commands,
                         "a focused hint exists; the full suite is the fallback only")
        commands = map_changed_files(["src/anvil_io.rs"])
        self.assertIn(focused_test("anvil_io"), commands)
        commands = map_changed_files(["src/server_reliability.rs"])
        self.assertIn(focused_test("server_reliability_tests"), commands)
        commands = map_changed_files(["src/operator_ux.rs"])
        self.assertIn(focused_test("operator_ux_tests"), commands)

    def test_src_test_module_maps_to_itself_and_unknown_src_falls_back(self):
        self.assertIn(focused_test("mineshaft_tests"),
                      map_changed_files(["src/mineshaft_tests.rs"]))
        self.assertIn(RUST_SUITE, map_changed_files(["src/mob_ai.rs"]))

    def test_multi_path_union_and_normalization(self):
        commands = map_changed_files(["./proto/src/lib.rs", "src/worldgen.rs",
                                      "README.md", "tools/mod_launcher.py"])
        self.assertIn(crate_test("proto"), commands)
        self.assertIn(focused_test("generation_contract_tests"), commands)
        self.assertIn("python3 tests/mod_fallback_test.py", commands)
        self.assertEqual(commands, sorted(commands))
        self.assertEqual(map_changed_files(["unmapped/file.py"]), [])


def self_test():
    suite = unittest.TestSuite()
    loader = unittest.TestLoader()
    suite.addTests(loader.loadTestsFromTestCase(CacheContract))
    suite.addTests(loader.loadTestsFromTestCase(MapContract))
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
