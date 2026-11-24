#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Licence / SPDX header audit — permanent per-push CI gate.

Three checks over the tracked tree:

1. No tracked source file (.rs .py .sh .js .mjs .wgsl) may carry the retired
   triple-licence SPDX header. The tree unified on LGPL-3.0-or-later on
   2026-11-22 (see LICENSING.md); the old contribution expression must never
   come back on a file header.

2. No tracked source file may carry a stale upstream-donor attribution
   marker — the donor project's name next to derivation / port / test-vector
   wording. Donor aquifer vectors were replaced by vanilla-jar-derived
   probes (THIRD_PARTY.md §10); the markers are gone and stay gone.
   Historical records are out of scope: evidence/ snapshots and the docs
   that narrate the migration (THIRD_PARTY.md, NOTICE, LICENSING.md, specs/)
   keep theirs — none are source files anyway.

3. The licence anchors exist: LICENSE, docs/COPYING.GPL, NOTICE,
   THIRD_PARTY.md — and every Cargo.toml manifest declares
   license = "LGPL-3.0-or-later".

Run: python3 tools/license_spdx_audit.py [--root DIR]
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

SOURCE_SUFFIXES = (".rs", ".py", ".sh", ".js", ".mjs", ".wgsl")

UNIFIED = "LGPL-3.0-or-later"
# The retired contribution expression, built rather than written out so this
# file and its test never carry the literal the gate hunts for.
OBSOLETE = " OR ".join((UNIFIED, "GPL-3.0-or-later", "AGPL-3.0-or-later"))
SPDX_TAG = "SPDX-License-Identifier"

# Same trick for the donor-project marker: a file is only stale when the
# upstream project's name sits next to derivation/port/vector wording —
# block names like carved_pumpkin and competitive-landscape prose naming
# the project are not attribution markers.
DONOR = "pump" + "kin"
DONOR_MARKER = re.compile(
    r"(?i)(?:\b" + DONOR + r"[-_ ](?:derived|donor|port|mc)\b"
    r"|\b" + DONOR + r"'s\b"
    r"|\b" + DONOR + r"\b[^\n]{0,40}\.(?:rs|java|toml)\b"
    r"|\b(?:derived|donor|ported?|reference[sd]?|read|borrowed|copied|lifted|from)\b"
    r"[^\n]{0,30}\b" + DONOR + r"\b"
    r"|\b" + DONOR + r"\b[^\n]{0,30}"
    r"\b(?:semantics?|tests?|test[\s_]?vectors?|vectors?|code|sources?)\b)"
)

REQUIRED_FILES = ("LICENSE", "docs/COPYING.GPL", "NOTICE", "THIRD_PARTY.md")

LICENSE_FIELD = re.compile(r'(?m)^[ \t]*license[ \t]*=[ \t]*"([^"]*)"')

# Never walked when falling back to a filesystem scan; `git ls-files` already
# never yields them.
SKIP_PARTS = {".git", "target", "node_modules"}
# Frozen snapshots keep whatever markers they recorded; check 1 still scans
# them (a retired SPDX header there would be a finding).
MARKER_EXEMPT_PARTS = {"evidence"}


def file_list(root):
    """Tracked paths under root, or a filesystem walk for synthetic trees."""
    probe = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=False)
    if probe.returncode == 0 and Path(probe.stdout.strip()).resolve() == root:
        listed = subprocess.run(["git", "-C", str(root), "ls-files", "-z"],
                                capture_output=True, text=True, check=False)
        if listed.returncode == 0:
            return sorted(root / name for name in listed.stdout.split("\0") if name)
    return sorted(p for p in root.rglob("*") if p.is_file()
                  and not (SKIP_PARTS & set(p.relative_to(root).parts)))


def audit(root):
    """Return a list of findings as (path, line-or-0, message)."""
    findings = []
    manifests = []
    for path in file_list(root):
        rel = path.relative_to(root).as_posix()
        if path.name == "Cargo.toml":
            manifests.append((rel, path))
        if not path.name.endswith(SOURCE_SUFFIXES):
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            findings.append((rel, 0, f"unreadable: {exc}"))
            continue
        # Most files can contain neither finding; skip the per-line scan.
        if SPDX_TAG not in text and DONOR not in text.lower():
            continue
        marker_exempt = MARKER_EXEMPT_PARTS & set(path.relative_to(root).parts[:-1])
        for lineno, line in enumerate(text.splitlines(), 1):
            if SPDX_TAG in line and OBSOLETE in line:
                findings.append(
                    (rel, lineno, "retired triple-licence SPDX header; "
                     f"the tree is {UNIFIED} only"))
            if not marker_exempt and DONOR in line.lower() \
                    and DONOR_MARKER.search(line):
                findings.append(
                    (rel, lineno, "stale upstream-donor attribution marker"))
    for rel, path in manifests:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            findings.append((rel, 0, f"unreadable: {exc}"))
            continue
        declared = LICENSE_FIELD.findall(text)
        if not declared:
            findings.append((rel, 0, "manifest declares no license field"))
        elif any(value != UNIFIED for value in declared):
            findings.append(
                (rel, 0, f'manifest license is {declared}, want "{UNIFIED}"'))
    if not manifests:
        findings.append((".", 0, "no Cargo.toml manifest found under root"))
    for name in REQUIRED_FILES:
        if not (root / name).is_file():
            findings.append((name, 0, "required licence anchor missing"))
    return findings


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    options = parser.parse_args(argv)
    root = options.root.resolve()

    findings = audit(root)
    for rel, line, message in findings:
        where = f"{rel}:{line}" if line else rel
        print(f"license/SPDX audit: FAIL {where} — {message}")
    if findings:
        print(f"license/SPDX audit: FAIL ({len(findings)} finding(s))")
        return 1
    print("license/SPDX audit: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
