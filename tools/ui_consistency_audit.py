#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Permanent UI-consistency guard for the unified Vanilla 1.21.1 client UI.

Four narrow textual invariants, deliberately cheap and deterministic:

1. The legacy dark blue-slate palette never reappears in
   ``client-ui/src/draw.rs`` — the tokens are the exact rgb() tuples the
   Vanilla migration removed.
2. ``client-ui/src/draw.rs`` derives its palette from the canonical
   ``megu_core::ui_contract::VANILLA_1_21_1`` token table so client-ui
   and megu-core share one color truth.
3. ``client-ui/src/retained_cache.rs`` is built on
   ``megu_core::rev::{UiLayerBus, RetainedLayerTracker}`` (plus the
   dep-graph composite) and is consumed live by client-gpu — an
   orphaned cache would silently lose the zero-alloc guarantee.
4. ``client-ui/src/vanilla_widgets.rs`` enforces the ModUiManifest
   contract: ``audit_manifest`` (rejects title-screen overlays) and
   ``compute_gui_scale`` (canonical scale snapping) must both be called
   on the manifest render path.

Run: python3 tools/ui_consistency_audit.py [--strict] [--json] [--root DIR]
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

DRAW_RS = Path("client-ui/src/draw.rs")
RETAINED_RS = Path("client-ui/src/retained_cache.rs")
VANILLA_WIDGETS_RS = Path("client-ui/src/vanilla_widgets.rs")
CLIENT_GPU_SRC = Path("client-gpu/src")

# The four blue-slate rgb() tuples the VANILLA_1_21_1 migration removed.
# Whitespace inside the call is tolerated so reformatting cannot slip a
# regression past the guard.
LEGACY_COLORS = (
    re.compile(r"rgb\(\s*24\s*,\s*26\s*,\s*32\s*\)"),
    re.compile(r"rgb\(\s*30\s*,\s*33\s*,\s*42\s*\)"),
    re.compile(r"rgb\(\s*58\s*,\s*62\s*,\s*76\s*\)"),
    re.compile(r"rgb\(\s*84\s*,\s*96\s*,\s*130\s*\)"),
)


def finding(guard, path, line, message):
    return {
        "guard": guard,
        "path": str(path),
        "line": line,
        "message": message,
    }


def read_lines(root, relative):
    path = root / relative
    if not path.is_file():
        return None
    return path.read_text(encoding="utf-8", errors="replace").splitlines()


def guard_legacy_palette(root):
    """Legacy blue-slate rgb() tuples must never reappear in draw.rs."""
    findings = []
    lines = read_lines(root, DRAW_RS)
    if lines is None:
        findings.append(
            finding(
                "legacy-blue-slate-palette",
                DRAW_RS,
                0,
                "client-ui/src/draw.rs is missing — palette derivation is unauditable",
            )
        )
        return findings
    for line_no, line in enumerate(lines, start=1):
        for pattern in LEGACY_COLORS:
            if pattern.search(line):
                findings.append(
                    finding(
                        "legacy-blue-slate-palette",
                        DRAW_RS,
                        line_no,
                        "legacy dark blue-slate color returned — palette must come from VANILLA_1_21_1",
                    )
                )
                break
    return findings


def guard_palette_derivation(root):
    """draw.rs must derive its palette from VANILLA_1_21_1."""
    lines = read_lines(root, DRAW_RS)
    if lines is None:
        return []  # the missing file is already a legacy-palette finding
    token = "megu_core::ui_contract::VANILLA_1_21_1"
    for line in lines:
        if token in line:
            return []
    return [
        finding(
            "vanilla-palette-derivation",
            DRAW_RS,
            0,
            "draw.rs does not derive its palette from megu_core::ui_contract::VANILLA_1_21_1",
        )
    ]


def guard_retained_wiring(root):
    """retained_cache must ride megu_core::rev/dep_graph and feed client-gpu."""
    findings = []
    lines = read_lines(root, RETAINED_RS)
    if lines is None:
        findings.append(
            finding(
                "retained-cache-wiring",
                RETAINED_RS,
                0,
                "client-ui/src/retained_cache.rs is missing",
            )
        )
        return findings
    text = "\n".join(lines)
    for token, what in (
        ("UiLayerBus", "revision bus"),
        ("RetainedLayerTracker", "dirty-layer tracker"),
        ("megu_core::rev", "megu_core::rev"),
        ("megu_core::dep_graph", "megu_core::dep_graph composite"),
    ):
        if token not in text:
            findings.append(
                finding(
                    "retained-cache-wiring",
                    RETAINED_RS,
                    0,
                    f"retained_cache.rs no longer uses the {what} ({token})",
                )
            )
    gpu_dir = root / CLIENT_GPU_SRC
    consumed = False
    if gpu_dir.is_dir():
        for path in gpu_dir.rglob("*.rs"):
            try:
                if "retained_cache::UiRetainedCache" in path.read_text(
                    encoding="utf-8", errors="replace"
                ):
                    consumed = True
                    break
            except OSError:
                continue
    if not consumed:
        findings.append(
            finding(
                "retained-cache-wiring",
                CLIENT_GPU_SRC,
                0,
                "no client-gpu source references retained_cache::UiRetainedCache — the cache is unwired",
            )
        )
    return findings


def guard_manifest_purity(root):
    """vanilla_widgets must audit manifests and snap the GUI scale."""
    lines = read_lines(root, VANILLA_WIDGETS_RS)
    if lines is None:
        return [
            finding(
                "mod-manifest-purity",
                VANILLA_WIDGETS_RS,
                0,
                "client-ui/src/vanilla_widgets.rs is missing",
            )
        ]
    text = "\n".join(lines)
    findings = []
    for token, what in (
        ("audit_manifest", "ModUiManifest purity audit (title-screen overlay rejection)"),
        ("compute_gui_scale", "canonical GUI-scale snapping"),
    ):
        if token not in text:
            findings.append(
                finding(
                    "mod-manifest-purity",
                    VANILLA_WIDGETS_RS,
                    0,
                    f"vanilla_widgets.rs no longer calls {what} ({token})",
                )
            )
    return findings


GUARDS = (
    ("legacy-blue-slate-palette", guard_legacy_palette),
    ("vanilla-palette-derivation", guard_palette_derivation),
    ("retained-cache-wiring", guard_retained_wiring),
    ("mod-manifest-purity", guard_manifest_purity),
)


def audit(root):
    findings = []
    summary = {}
    for name, guard in GUARDS:
        guard_findings = guard(root)
        summary[name] = len(guard_findings)
        findings.extend(guard_findings)
    findings.sort(key=lambda row: (row["guard"], row["path"], row["line"]))
    return {"ok": not findings, "root": str(root), "summary": summary, "findings": findings}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--json", action="store_true", help="emit a machine-readable report")
    parser.add_argument("--strict", action="store_true", help="exit nonzero when any guard reports a finding")
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    options = parser.parse_args(argv)

    report = audit(options.root)
    if options.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        for row in report["findings"]:
            print(f"{row['guard']}: FAIL {row['path']}:{row['line']} — {row['message']}")
        total = len(report["findings"])
        status = "PASS" if report["ok"] else f"FAIL ({total} finding(s))"
        counts = ", ".join(f"{name}={count}" for name, count in sorted(report["summary"].items()))
        print(f"ui consistency audit: {status} [{counts}]")
    if options.strict and not report["ok"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
