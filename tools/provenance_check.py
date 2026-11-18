#!/usr/bin/env python3
"""Fail closed when a file with no usable provenance is back in the tree.

  tools/provenance_check.py            check this checkout
  tools/provenance_check.py --strict-public
                                       also require that the data D5 is going to untrack is
                                       neither tracked nor committable
  tools/provenance_check.py --self-test
                                       prove the guard can fail, before trusting it in CI

Why the three files and not "all Mojang data": `assets/pblocks.json` and `assets/properties.json`
were byte-identical to files from another project under GPL-3.0 (so either copied from it or shared
an unknown upstream), and `resourcepack.zip` had no recorded origin at all. They are gone now, and
the consumers read `assets/block_states_1211.json`, generated from the pinned 1.21.1 jar by
`tools/vanilla_data/`. This gate is what keeps them from returning: a re-introduction is either a
`git add -f` of an ignored path or an edit that starts reading one again, and both are silent
without it.

`--strict-public` is the D5 rehearsal: it asserts the Mojang-derived data this branch still tracks
is untracked *and* ignored, so a future release commit cannot pick it up by accident. It is a
separate flag because the default must pass on this branch's tree as it is.

Exit status: 0 clean, 1 findings, 2 the check could not run (no git, unreadable asset).
"""
import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PINNED_JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
STATE_ASSET = "assets/block_states_1211.json"
# Tracked, referenced, or otherwise depended on => failure. The reference scan makes re-reading one
# of them a failure too, not just re-adding it.
GONE = ("assets/pblocks.json", "assets/properties.json", "resourcepack.zip")
SCAN_DIRS = ("src", "tests")
# The only places a removed file may still be named, with the count and the reason. `src/main.rs`
# reads a *user-supplied* resource pack from the server's working directory: this tree ships none,
# `.gitignore` keeps one out of the index, and the feature is documented (THIRD_PARTY.md §9 - the
# server advertises nothing without the file). `tests/gate_scripts.py` keeps prose about why its
# fixture no longer plants one. Both are declarations, so a second read - or a new file naming one
# of the three - is a failure rather than a silent re-introduction.
DECLARED = {
    ("src/main.rs", "resourcepack.zip"): (
        2,
        "the optional server resource-pack feature reads a user-supplied file from its cwd",
    ),
    ("tests/gate_scripts.py", "resourcepack.zip"): (
        1,
        "a comment recording why the fixture no longer plants one",
    ),
}
# Data derived from Mojang's jar that this branch still tracks. `--strict-public` is the check that
# a release without it would pass; the default branch does not.
STRICT_PUBLIC = (
    "datapacks/vanilla",
    "assets/block_states_1211.json",
    "tests/data/biome_probe_1211.tsv",
    "tests/data/vanilla_vectors_1211.tsv",
    "config_packets.bin",
)


class Unrunnable(Exception):
    pass


def git(root, *args):
    result = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    if result.returncode != 0:
        raise Unrunnable(f"git {' '.join(args)}: {result.stderr.strip()}")
    return result.stdout


def tracked(root):
    """Every path in the index, as the repository spells it."""
    return {line for line in git(root, "ls-files").splitlines() if line}


def ignored(root, path):
    return subprocess.run(
        ["git", "check-ignore", "-q", "--no-index", path], cwd=root, capture_output=True
    ).returncode == 0


def scan_for_references(root):
    """Every `src/`/`tests/` line that names one of the removed files, against the declarations."""
    counts = {}
    for directory in SCAN_DIRS:
        base = root / directory
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            if not path.is_file() or path.suffix not in (".rs", ".py", ".sh", ".json"):
                continue
            try:
                lines = path.read_text(errors="replace").splitlines()
            except OSError:
                continue
            relative = path.relative_to(root).as_posix()
            for line in lines:
                for name in GONE:
                    if name in line:
                        key = (relative, name)
                        counts[key] = counts.get(key, 0) + 1
    failures, notes = [], []
    for (relative, name), found in sorted(counts.items()):
        declared, reason = DECLARED.get((relative, name), (0, None))
        if found > declared:
            where = f"declared {declared}: {reason}" if reason else "no declared reason"
            failures.append(f"{relative} names {name} {found} time(s), {where}")
        elif found < declared:
            notes.append(f"{relative} names {name} {found} time(s), declared {declared} - "
                         f"lower the declaration to lock the gain in")
    return failures, notes


def check_state_asset(root):
    """The per-state asset must be present and must say which jar produced it."""
    path = root / STATE_ASSET
    if not path.is_file():
        return [f"{STATE_ASSET} is missing; the registry reads it"]
    try:
        asset = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise Unrunnable(f"{STATE_ASSET}: {error}")
    failures = []
    got = asset.get("jar_sha1")
    if got != PINNED_JAR_SHA1:
        failures.append(
            f"{STATE_ASSET} records jar_sha1 {got!r}, expected the pinned {PINNED_JAR_SHA1}"
        )
    if asset.get("minecraft") != "1.21.1":
        failures.append(
            f"{STATE_ASSET} records minecraft {asset.get('minecraft')!r}, expected '1.21.1'"
        )
    generator = asset.get("generated_by") or ""
    if "gen_vanilla_data.sh" not in generator:
        failures.append(
            f"{STATE_ASSET} records generated_by {generator!r}, which does not name the generator"
        )
    # A table with no states would satisfy every field check above and break every consumer.
    for column in ("flags", "luminance", "light_block", "collision_shapes"):
        values = asset.get(column)
        if not isinstance(values, list) or len(values) < 20_000:
            failures.append(f"{STATE_ASSET} column {column!r} does not cover the state space")
    if not asset.get("shapes"):
        failures.append(f"{STATE_ASSET} carries no collision `shapes` table")
    return failures


def check_strict_public(root):
    failures = []
    index = tracked(root)
    for path in STRICT_PUBLIC:
        prefix = path.rstrip("/") + "/"
        if any(entry == path or entry.startswith(prefix) for entry in index):
            failures.append(f"--strict-public: {path} is still tracked")
        elif not (ignored(root, path) or ignored(root, path + "/")):
            failures.append(f"--strict-public: {path} is not ignored, so a release could add it")
    return failures


def check(root, strict_public=False):
    index = tracked(root)
    failures = [f"{path} is tracked; it has no usable provenance" for path in GONE if path in index]
    references, notices = scan_for_references(root)
    failures += references
    failures += check_state_asset(root)
    if strict_public:
        failures += check_strict_public(root)
    return failures, notices


# ---- self-test -----------------------------------------------------------------------------
# A guard that has never been observed failing is not evidence. Each case builds a throwaway
# repository in one shape and asserts the verdict, including the shape that must pass. The asset is
# this checkout's, so the passing case carries a real one: a green case built on an empty table
# would only prove that "not there" reads as "fine".


def selftest_case(asset, name, *, index=(), write=(), strict_public=False, expect):
    with tempfile.TemporaryDirectory(prefix="provenance-selftest-") as tmp:
        root = Path(tmp)
        git(root, "init", "-q")
        (root / "src").mkdir()
        (root / "tests").mkdir()
        (root / "assets").mkdir()
        (root / ".gitignore").write_text("/resourcepack.zip\n/datapacks/vanilla/\n"
                                         "/assets/block_states_1211.json\n"
                                         "/tests/data/biome_probe_1211.tsv\n"
                                         "/tests/data/vanilla_vectors_1211.tsv\n"
                                         "/config_packets.bin\n")
        (root / STATE_ASSET).write_bytes(asset)
        (root / "src/main.rs").write_text("fn main() {}\n")
        for path, text in write:
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text)
        for path in index:
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            if not target.exists():
                target.write_text("planted\n")
            git(root, "add", "-f", path)
        failures, _ = check(root, strict_public=strict_public)
        ok = bool(failures) == expect
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: {len(failures)} finding(s)")
        for failure in failures[:3]:
            print(f"        {failure}")
        return ok


def selftest(root):
    asset = (root / STATE_ASSET).read_bytes()
    if not asset:
        raise Unrunnable("no state asset to seed the self-test with")

    def case(name, **kwargs):
        return selftest_case(asset, name, **kwargs)

    results = [
        case("a clean tree passes", expect=False),
        case("a tracked pblocks.json fails", index=["assets/pblocks.json"], expect=True),
        case("a tracked resourcepack.zip fails", index=["resourcepack.zip"], expect=True),
        case("a new source that reads resourcepack.zip fails",
             write=[("src/extra.rs", 'let x = std::fs::read("resourcepack.zip");\n')],
             expect=True),
        case("one extra read in a declared file fails",
             write=[("src/main.rs", 'a\nstd::fs::read("resourcepack.zip");\n'
                                    'std::fs::read("resourcepack.zip");\n'
                                    'std::fs::read("resourcepack.zip");\n')],
             expect=True),
        case("a test that names properties.json fails",
             write=[("tests/gate.py", "open('assets/properties.json')\n")],
             expect=True),
        case("a truncated state table fails",
             write=[(STATE_ASSET, '{"jar_sha1": "%s"}' % PINNED_JAR_SHA1)],
             expect=True),
        case("a foreign jar sha1 fails",
             write=[(STATE_ASSET, asset.decode().replace(PINNED_JAR_SHA1, "0" * 40))],
             expect=True),
        case("strict-public rejects a tracked biome probe",
             index=["tests/data/biome_probe_1211.tsv"], strict_public=True, expect=True),
        case("strict-public rejects a tracked datapack",
             index=["datapacks/vanilla/pack.mcmeta"], strict_public=True, expect=True),
    ]
    # The reference scan is what makes "reading one again" a failure, so the case that plants a
    # reference must actually have been planted.
    with tempfile.TemporaryDirectory(prefix="provenance-selftest-ref-") as tmp:
        root = Path(tmp)
        (root / "src").mkdir()
        (root / "src/main.rs").write_text('let x = std::fs::read("assets/pblocks.json");\n')
        hits, _ = scan_for_references(root)
        ok = len(hits) == 1
        print(f"  {'ok  ' if ok else 'FAIL'} the reference scan sees a planted read: {len(hits)}")
        results.append(ok)
    return all(results)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--strict-public", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()
    if shutil.which("git") is None:
        print("provenance: cannot run without git", file=sys.stderr)
        return 2
    try:
        if args.self_test:
            ok = selftest(args.root)
            print(f"provenance: self-test {'passed' if ok else 'FAILED'}")
            return 0 if ok else 1
        failures, notices = check(args.root, strict_public=args.strict_public)
    except Unrunnable as error:
        print(f"provenance: {error}", file=sys.stderr)
        return 2
    for note in notices:
        print(f"provenance: note: {note}", file=sys.stderr)
    for failure in failures:
        print(f"provenance: {failure}", file=sys.stderr)
    if failures:
        print(f"provenance: {len(failures)} finding(s); these files must not be in this tree",
              file=sys.stderr)
        return 1
    if not args.quiet:
        scope = " and no tracked Mojang data" if args.strict_public else ""
        print(f"provenance: clean - none of {', '.join(GONE)} is tracked or referenced{scope}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
