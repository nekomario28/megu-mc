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

The license scan is the other half of provenance: every workspace manifest must declare a
`license =` field, and that declaration must not name a GPL-incompatible identifier
(PolyForm/BUSL/SSPL/ARR-style terms or a prose "all rights reserved"/"proprietary" claim), so a
crate that cannot be distributed inside this tree fails the gate instead of shipping quietly.

Exit status: 0 clean, 1 findings, 2 the check could not run (no git, unreadable asset).
"""
import argparse
import json
import re
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
# D5 requires these generator outputs to be untracked AND ignored on the public branch.
STRICT_PUBLIC = (
    "datapacks/vanilla",
    "assets/block_states_1211.json",
    "tests/data/biome_probe_1211.tsv",
    "tests/data/vanilla_vectors_1211.tsv",
    "config_packets.bin",
)
# Every workspace manifest whose [package] table must declare a distributable license. The list
# is explicit rather than globbed so a manifest deleted with its crate is a finding, not a pass.
LICENSE_MANIFESTS = (
    "Cargo.toml",
    "client-app/Cargo.toml",
    "client-gpu/Cargo.toml",
    "client-kernel/Cargo.toml",
    "client-port/Cargo.toml",
    "client-ui/Cargo.toml",
    "core/Cargo.toml",
    "create-adapter/Cargo.toml",
    "mod-adapter-core/Cargo.toml",
    "mod-offload/Cargo.toml",
    "proto/Cargo.toml",
    "server-files/Cargo.toml",
    "state-store/Cargo.toml",
    "tools/offload_ffi_probe/Cargo.toml",
    "tools/palette_bench/Cargo.toml",
    "tools/storage_bench/Cargo.toml",
)
# License terms a crate in this GPL-family tree cannot be distributed under. Identifiers are
# matched per '-'-separated token segment so SPDX spellings ("BUSL-1.1", "PolyForm-Shield-1.0.0",
# "LicenseRef-ARR") all resolve, and the phrases catch the prose stand-ins that carry no grant.
LICENSE_BLOCKED_SEGMENTS = frozenset({"polyform", "busl", "sspl", "arr", "proprietary"})
LICENSE_BLOCKED_PHRASES = ("all rights reserved", "proprietary")
_PACKAGE_TABLE = re.compile(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)")
_LICENSE_FIELD = re.compile(r"(?m)^\s*license\s*=\s*[\"']([^\"']*)[\"']")

# Gate G2 - zero production embeds of generated or Mojang-derived data. A
# non-test `include_bytes!`/`include_str!` on one of these paths re-embeds
# exactly what the public export strips, so the stripped tree stops compiling
# (and ships Mojang bytes inside the binary). `#[cfg(test)]` items and files
# under a `tests/` directory are exempt: fixtures may stay embedded.
EMBED_FORBIDDEN = re.compile(
    r"assets/[^\s\"']*_1211|datapacks/vanilla|config_packets\.bin")
INCLUDE_MACRO = re.compile(r"\binclude_(?:bytes|str)!\s*\(")
CFG_TEST_ATTR = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*(?:(?:all|any)\s*\(\s*)*test\b")
PATH_ATTR = re.compile(r"#\s*\[\s*path\s*=\s*\"([^\"]+)\"\s*\]")
MOD_DECL = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*([{;])")


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


def check_state_asset(root, *, allow_missing=False):
    """Validate the generated asset, including a local copy in a public checkout."""
    path = root / STATE_ASSET
    if allow_missing and not path.exists() and not path.is_symlink():
        return []
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


def manifest_license(text):
    """The `[package]` table's `license = "..."` value, or None when the field is absent."""
    package = _PACKAGE_TABLE.search(text)
    if package is None:
        return None
    field = _LICENSE_FIELD.search(package.group(1))
    return field.group(1) if field else None


def check_licenses(root):
    failures = []
    for relative in LICENSE_MANIFESTS:
        path = root / relative
        try:
            declared = manifest_license(path.read_text())
        except OSError:
            failures.append(f"{relative} is missing; every workspace crate declares a license")
            continue
        if not declared or not declared.strip():
            failures.append(f"{relative} declares no license = field")
            continue
        normalized = re.sub(r"[-_+/:]", " ", declared.lower())
        phrase = next((p for p in LICENSE_BLOCKED_PHRASES if p in normalized), None)
        if phrase is not None:
            failures.append(f"{relative} declares the incompatible license "
                            f"{declared!r} ({phrase})")
            continue
        segments = [segment for token in re.split(r"[\s()]+", normalized)
                    for segment in token.split() if segment]
        blocked = [segment for segment in segments if segment in LICENSE_BLOCKED_SEGMENTS]
        if blocked:
            failures.append(f"{relative} declares the incompatible license "
                            f"{declared!r} ({blocked[0]})")
    return failures


def _mask_noncode(text):
    """Space out comments and string/char literal contents, keeping offsets.

    Line and block comments, regular strings, byte strings, raw strings
    (`r#"..."#`) and char literals are all blanked so a `{`, `;` or the
    `include_` spelling inside one cannot fake code structure. Identifiers
    inside literals can still be read from the unmasked text via the shared
    offsets.
    """
    out = list(text)
    i, n = 0, len(text)
    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end == -1 else end
            out[i:end] = " " * (end - i)
            i = end
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = n if end == -1 else end + 2
            for k in range(i, end):
                if text[k] != "\n":
                    out[k] = " "
            i = end
        elif text.startswith('"', i) or text.startswith('b"', i):
            start = i
            i += 2 if text.startswith('b"', i) else 1
            while i < n and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            end = min(i + 1, n)
            for k in range(start, end):
                if text[k] != "\n":
                    out[k] = " "
            i = end
        elif text[i] == "r" or (text[i] == "b" and i + 1 < n and text[i + 1] == "r"):
            # Raw string: r"...", r#"..."#, br#"..."#
            m = re.match(r"(?:b)?r(#+)?\"", text[i:])
            if m:
                terminator = '"' + (m.group(1) or "")
                start = i
                i += m.end()
                close = text.find(terminator, i)
                end = n if close == -1 else close + len(terminator)
                for k in range(start, end):
                    if text[k] != "\n":
                        out[k] = " "
                i = end
            else:
                i += 1
        elif text[i] == "'":
            # A char literal is `'x'` or `'\n'`; a lifetime is `'a` with no
            # closing quote, so only a pattern that actually closes masks.
            m = re.match(r"'(?:\\.|[^\\'])'", text[i:])
            if m:
                out[i:i + m.end()] = " " * m.end()
                i += m.end()
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def _item_end_after_attr(masked, pos):
    """End offset of the item an attribute attaches to: the next `;` at brace
    depth 0, or the balanced `{}` block that closes the item."""
    while pos < len(masked) and masked[pos].isspace():
        pos += 1
    end, depth = pos, 0
    while end < len(masked):
        char = masked[end]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return end + 1
        elif char == ";" and depth == 0:
            return end + 1
        end += 1
    return end


def _test_regions(masked):
    """Byte spans covered by `#[cfg(test)]`-attributed items: whole `mod`
    blocks, or the single statement the attribute decorates."""
    regions = []
    for attr in CFG_TEST_ATTR.finditer(masked):
        regions.append((attr.start(), _item_end_after_attr(masked, attr.end())))
    return regions


def check_prod_embeds(root):
    """Gate G2: no non-test `.rs` may `include_bytes!`/`include_str!` generated
    or Mojang-derived data (`assets/*_1211*`, `datapacks/vanilla`,
    `config_packets.bin`).

    A file is test-only when a `#[cfg(test)] mod name;` declaration resolves
    to it (directly or via `#[path]`), or it lives under a `tests/` directory.
    Inside other files, includes inside a `#[cfg(test)]`-attributed item are
    exempt; everything else naming a forbidden path fails the gate.
    """
    failures = []
    skip_dirs = {".git", "target", "tests"}
    sources = [
        path for path in sorted(root.rglob("*.rs"))
        if path.is_file() and not skip_dirs.intersection(
            path.relative_to(root).parts)
    ]
    # First pass: files a `#[cfg(test)] mod ...;` declaration makes test-only.
    test_only = set()
    masked_sources = {}
    for path in sources:
        try:
            masked_sources[path] = _mask_noncode(
                path.read_text(errors="replace"))
        except OSError:
            continue
        masked = masked_sources[path]
        for attr in CFG_TEST_ATTR.finditer(masked):
            span_end = _item_end_after_attr(masked, attr.end())
            segment = masked[attr.end():span_end]
            base, stem = path.parent, path.stem
            enclosing = []
            for decl in MOD_DECL.finditer(segment):
                if decl.group(2) == "{":
                    enclosing.append(decl.group(1))
                    continue
                name = decl.group(1)
                candidates = [
                    base / f"{name}.rs", base / name / "mod.rs",
                    base / stem / f"{name}.rs", base / stem / name / "mod.rs",
                ]
                for outer in enclosing:
                    candidates += (base / stem / outer / f"{name}.rs",
                                   base / stem / outer / name / "mod.rs")
                for resolved in PATH_ATTR.finditer(segment):
                    candidates.append(base / resolved.group(1))
                for candidate in candidates:
                    test_only.add(candidate.resolve())
    # Second pass: includes outside test regions, in non-test files. The
    # `tests.rs` / `*_tests.rs` convention also marks a file as test-only.
    for path in sources:
        try:
            relative = path.relative_to(root)
        except ValueError:
            continue
        if (path.resolve() in test_only or path.stem == "tests"
                or path.stem.endswith("_tests")):
            continue
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        masked = masked_sources[path]
        regions = _test_regions(masked)
        for include in INCLUDE_MACRO.finditer(masked):
            if any(start <= include.start() < end for start, end in regions):
                continue
            depth, end = 0, include.end() - 1
            while end < len(masked):
                if masked[end] == "(":
                    depth += 1
                elif masked[end] == ")":
                    depth -= 1
                    if depth == 0:
                        end += 1
                        break
                end += 1
            argument = text[include.end() - 1:end]
            hit = EMBED_FORBIDDEN.search(argument)
            if hit:
                line = text[:include.start()].count("\n") + 1
                failures.append(
                    f"{relative.as_posix()}:{line} embeds {hit.group(0)!r} "
                    "at production scope; generated/Mojang data must load at "
                    "runtime (Gate G2)")
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
    failures += check_state_asset(root, allow_missing=(
        strict_public or (STATE_ASSET not in index and ignored(root, STATE_ASSET))
    ))
    failures += check_licenses(root)
    failures += check_prod_embeds(root)
    if strict_public:
        failures += check_strict_public(root)
    return failures, notices


# ---- self-test -----------------------------------------------------------------------------
# A guard that has never been observed failing is not evidence. Each case builds a throwaway
# repository in one shape and asserts the verdict, including the shape that must pass.


def synthetic_state_asset():
    table = [0] * 20_000
    return json.dumps(dict(jar_sha1=PINNED_JAR_SHA1, minecraft="1.21.1",
                           generated_by="gen_vanilla_data.sh (self-test fixture)",
                           flags=table, luminance=table, light_block=table,
                           collision_shapes=table, shapes=[[]])).encode()


def selftest_case(asset, name, *, index=(), write=(), strict_public=False, missing=False, expect):
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
        # A clean license baseline in every case; a `write` entry can then break one manifest.
        for index_, manifest in enumerate(LICENSE_MANIFESTS):
            target = root / manifest
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(f'[package]\nname = "selftest-{index_}"\nversion = "0.0.0"\n'
                              'license = "LGPL-3.0-or-later OR GPL-3.0-or-later '
                              'OR AGPL-3.0-or-later"\n')
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
        if missing:
            (root / STATE_ASSET).unlink()
        failures, _ = check(root, strict_public=strict_public)
        ok = bool(failures) == expect
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: {len(failures)} finding(s)")
        for failure in failures[:3]:
            print(f"        {failure}")
        return ok


def selftest(root):
    try:
        asset = (root / STATE_ASSET).read_bytes()
    except FileNotFoundError:
        asset = synthetic_state_asset()

    def case(name, **kwargs):
        return selftest_case(asset, name, **kwargs)

    results = [
        case("a clean tree passes", expect=False),
        case("a strict export without a generated asset passes", missing=True,
             strict_public=True, expect=False),
        case("a missing unignored asset fails", missing=True, write=[(".gitignore", "")],
             expect=True),
        case("a missing tracked asset fails even if ignored", missing=True, index=[STATE_ASSET],
             expect=True),
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
        # License hygiene: the GPL-family expressions pass, the incompatible identifiers and a
        # missing declaration all fail closed.
        case("MIT OR Apache-2.0 is an accepted expression",
             write=[("mod-offload/Cargo.toml",
                     '[package]\nname = "m"\nlicense = "MIT OR Apache-2.0"\n')],
             expect=False),
        case("a PolyForm license fails",
             write=[("mod-offload/Cargo.toml",
                     '[package]\nname = "m"\nlicense = "PolyForm-Shield-1.0.0"\n')],
             expect=True),
        case("BUSL and SSPL licenses fail",
             write=[("client-port/Cargo.toml",
                     '[package]\nname = "p"\nlicense = "BUSL-1.1"\n'),
                    ("client-kernel/Cargo.toml",
                     '[package]\nname = "k"\nlicense = "SSPL-1.0"\n')],
             expect=True),
        case("an ARR license fails",
             write=[("create-adapter/Cargo.toml",
                     '[package]\nname = "c"\nlicense = "ARR"\n')],
             expect=True),
        case("an all-rights-reserved claim fails",
             write=[("client-app/Cargo.toml",
                     '[package]\nname = "a"\nlicense = "LicenseRef-All-Rights-Reserved"\n')],
             expect=True),
        case("a proprietary license fails",
             write=[("client-app/Cargo.toml",
                     '[package]\nname = "a"\nlicense = "Proprietary"\n')],
             expect=True),
        case("a manifest without a license field fails",
             write=[("mod-adapter-core/Cargo.toml",
                     '[package]\nname = "a"\nversion = "0.1.0"\n')],
             expect=True),
        # Gate G2: production embeds of generated/Mojang data fail; test-scope
        # embeds stay exempt.
        case("a production include_bytes! of a *_1211 asset fails",
             write=[("src/registry.rs",
                     'const X: &[u8] = '
                     'include_bytes!("../assets/block_state_facts_1211.bin");\n')],
             expect=True),
        case("a production include_str! of datapacks/vanilla fails",
             write=[("src/dim.rs",
                     'const X: &str = '
                     'include_str!("../datapacks/vanilla/data/minecraft/x.json");\n')],
             expect=True),
        case("a cfg(test) module embed passes",
             write=[("src/registry.rs",
                     '#[cfg(test)]\nmod tests {\nconst X: &[u8] = '
                     'include_bytes!("../assets/block_state_facts_1211.bin");\n}\n')],
             expect=False),
        case("a cfg(test) statement embed passes",
             write=[("src/spawn.rs",
                     '#[cfg(test)]\nconst X: &[u8] = '
                     'include_bytes!("../datapacks/vanilla/x.json");\n')],
             expect=False),
        case("a tests-dir embed passes",
             write=[("tests/embed.rs",
                     'const X: &[u8] = '
                     'include_bytes!("../datapacks/vanilla/x.json");\n')],
             expect=False),
        case("a cfg(test)-declared module file embed passes",
             write=[("src/main.rs",
                     '#[cfg(test)]\n#[path = "fixture_tests.rs"]\n'
                     'mod fixture_tests;\nfn main() {}\n'),
                    ("src/fixture_tests.rs",
                     'const X: &[u8] = '
                     'include_bytes!("../assets/blocks_1211.json");\n')],
             expect=False),
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
