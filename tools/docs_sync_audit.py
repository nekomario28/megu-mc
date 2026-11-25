#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""docs_sync_audit.py -- docs-to-code sync guard for the A14/A16 documentation set.

Schema: "megu-docs-sync-audit/1".

Three checks over the repo, all fail-closed (a missing input is a hard
error, exit 2, never a silent pass):

1. `parity_gates_documented` -- every `gateNN_name` defined in
   `tools/strict_vanilla_parity_audit.py` is named in
   `docs/VANILLA_PARITY_CHECKLIST.md`, and no stale scorecard fraction
   remains: every `X/Y` immediately followed by `PASS`/`pass` must equal
   the current gate count (the retired `5/10` snapshot is the defect class
   this check exists for).
2. `compat_tiers_defined` -- `docs/mod-compatibility.md` defines the full
   six-rung compatibility ladder `L0` .. `L5`.
3. `cargo_features_documented` -- every `[features]` entry in the root
   `Cargo.toml` (`java-compat`, `fabric-probe`, `neoforge-events`,
   `create-offload`, `mod-suite`, `native-oracle`, ...) is mentioned in
   `docs/mod-compatibility.md`.

Gate IDs are extracted from `def gateNN_name(` definitions only: a mention
in a docstring, comment, or the `GATES` registry is not a definition, so a
gate that was removed from the registry but kept as prose cannot satisfy
this audit.

Pure Python 3 stdlib. Run: python3 tools/docs_sync_audit.py [--root DIR]
[--json] [--strict]. Default prints a per-check scorecard and exits 0;
--strict exits 1 when any check FAILs; exit 2 is a fail-closed
input/parse error.
"""

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

SCHEMA = "megu-docs-sync-audit/1"

# The parity scorecard document, the mod-compatibility document and the
# authoritative parity gate tool (relative to --root).
CHECKLIST_DOC = "docs/VANILLA_PARITY_CHECKLIST.md"
COMPAT_DOC = "docs/mod-compatibility.md"
PARITY_TOOL = "tools/strict_vanilla_parity_audit.py"
ROOT_MANIFEST = "Cargo.toml"

GATE_DEF = re.compile(r"(?m)^[ \t]*def[ \t]+(gate\d+_[a-z0-9_]+)\s*\(")
# A scorecard fraction directly in front of PASS/pass: "5/10 PASS" and
# "11/11 PASS" both match; "5x4x5 open water" and "0x3d" do not.
SCORE_FRACTION = re.compile(r"(\d+)\s*/\s*(\d+)\s*(?=PASS|pass)")
FEATURE_KEY = re.compile(r"^\s*([A-Za-z0-9_\-]+)\s*=")
SECTION_HEADER = re.compile(r"^\s*\[[^\]]+\]\s*$")
FEATURES_HEADER = re.compile(r"^\s*\[features\]\s*$")


class AuditError(Exception):
    """Fail-closed input error -> exit 2."""


def read_required(root, rel):
    """Text of root/rel; a missing/unreadable file is an AuditError."""
    path = root / rel
    if not path.is_file():
        raise AuditError(f"missing required file {rel}")
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        raise AuditError(f"unreadable {rel}: {exc}") from exc


def extract_gate_ids(tool_text):
    """Gate ids from `def gateNN_name(` definitions, in definition order."""
    ids = GATE_DEF.findall(tool_text)
    if not ids:
        raise AuditError(
            f"{PARITY_TOOL}: no `def gateNN_name(` definitions found")
    if len(set(ids)) != len(ids):
        raise AuditError(f"{PARITY_TOOL}: duplicate gate definitions found")
    return ids


def parse_cargo_features(manifest_text):
    """Feature names from the root manifest's [features] section.

    A missing [features] section is an AuditError (fail-closed: the audit
    cannot prove the feature set is empty). An empty section yields [].
    """
    lines = manifest_text.splitlines()
    start = None
    for i, line in enumerate(lines):
        if FEATURES_HEADER.match(line):
            start = i + 1
            break
    if start is None:
        raise AuditError(
            f"{ROOT_MANIFEST}: no [features] section (cannot prove the "
            "feature set is empty)")
    features = []
    for line in lines[start:]:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if SECTION_HEADER.match(line):
            break
        match = FEATURE_KEY.match(line)
        if match:
            features.append(match.group(1))
    return features


def _check(gid, title):
    return {"id": gid, "title": title, "status": "PASS",
            "findings": [], "evidence": []}


def _finish(check):
    if check["findings"]:
        check["status"] = "FAIL"
    return check


def check_parity_gates(checklist, gate_ids):
    """Every gate id documented; every PASS fraction equals the gate count."""
    check = _check(
        "parity_gates_documented",
        "parity checklist documents all strict-audit gates")
    check["evidence"].append(
        f"{len(gate_ids)} gates extracted from {PARITY_TOOL}")
    missing = [gid for gid in gate_ids if gid not in checklist]
    if missing:
        check["findings"].append(
            f"{CHECKLIST_DOC} does not document: "
            + ", ".join(f"`{gid}`" for gid in missing))
    total = len(gate_ids)
    seen_current = False
    for match in SCORE_FRACTION.finditer(checklist):
        num, den = int(match.group(1)), int(match.group(2))
        if num == total and den == total:
            seen_current = True
        else:
            check["findings"].append(
                f"{CHECKLIST_DOC}: stale scorecard fraction "
                f"{match.group(1)}/{match.group(2)} adjacent to "
                "PASS/pass (current gate count is "
                f"{total}/{total})")
    if not seen_current:
        check["evidence"].append(
            f"no up-to-date {total}/{total} PASS fraction found "
            f"in {CHECKLIST_DOC}")
    return _finish(check)


def check_compat_tiers(compat_doc):
    """L0 .. L5 all defined in mod-compatibility.md."""
    check = _check(
        "compat_tiers_defined",
        "mod-compatibility doc defines the L0-L5 tier ladder")
    missing = []
    for tier in range(6):
        if re.search(r"\bL%d\b" % tier, compat_doc) is None:
            missing.append("L%d" % tier)
        else:
            check["evidence"].append(f"L{tier} defined")
    if missing:
        check["findings"].append(
            f"{COMPAT_DOC} does not define tier(s): " + ", ".join(missing))
    return _finish(check)


def check_cargo_features(compat_doc, features):
    """Every root Cargo.toml feature named in mod-compatibility.md."""
    check = _check(
        "cargo_features_documented",
        "root Cargo.toml features documented in mod-compatibility")
    check["evidence"].append(
        f"{len(features)} features parsed from {ROOT_MANIFEST}")
    for feature in features:
        if feature in compat_doc:
            check["evidence"].append(f"`{feature}` documented")
        else:
            check["findings"].append(
                f"cargo feature `{feature}` is not documented in "
                f"{COMPAT_DOC}")
    return _finish(check)


def audit_repo(root):
    """Run the three checks against a repo root -> report dict."""
    root = root.resolve()
    tool_text = read_required(root, PARITY_TOOL)
    checklist = read_required(root, CHECKLIST_DOC)
    compat_doc = read_required(root, COMPAT_DOC)
    manifest = read_required(root, ROOT_MANIFEST)
    gate_ids = extract_gate_ids(tool_text)
    features = parse_cargo_features(manifest)
    checks = [
        check_parity_gates(checklist, gate_ids),
        check_compat_tiers(compat_doc),
        check_cargo_features(compat_doc, features),
    ]
    summary = {
        "pass": sum(1 for c in checks if c["status"] == "PASS"),
        "fail": sum(1 for c in checks if c["status"] == "FAIL"),
        "total": len(checks),
    }
    return {"schema": SCHEMA, "checks": checks, "summary": summary}


def render_text(report):
    out = ["docs-to-code sync audit (A14/A16 documentation set)", ""]
    for check in report["checks"]:
        out.append(f"[{check['status']:4}] {check['id']}: {check['title']}")
        for ev in check["evidence"]:
            out.append(f"       evidence: {ev}")
        for f in check["findings"]:
            out.append(f"       gap: {f}")
    s = report["summary"]
    out.append("")
    out.append(f"score: {s['pass']}/{s['total']} PASS ({s['fail']} fail)")
    return "\n".join(out)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT,
                        help="repo root containing docs/ tools/ and "
                             "Cargo.toml (default: this repo)")
    parser.add_argument("--json", action="store_true",
                        help="emit the schema'd JSON report instead of text")
    parser.add_argument("--strict", action="store_true",
                        help="exit 1 when any check FAILs (CI wiring)")
    options = parser.parse_args(argv)
    try:
        report = audit_repo(options.root)
    except AuditError as exc:
        print(f"docs sync audit: ERROR {exc}", file=sys.stderr)
        return 2
    report["root"] = str(options.root.resolve())
    if options.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text(report))
    if options.strict and report["summary"]["fail"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
