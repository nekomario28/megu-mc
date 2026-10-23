#!/usr/bin/env python3
"""Fail closed on real source edges, named debt, or an inspector that cannot fail."""
from collections import Counter
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from rust_module_graph import ROOT, POLICY, source_graph

DIR = ROOT / "evidence/foundation/module-layers"


def inspect(executable, name):
    result = subprocess.run([executable, "inspect", str(DIR / name), "--json", "--no-storyboard"],
                            capture_output=True, text=True, timeout=30)
    try:
        report = json.loads(result.stdout)
        errors = [p for p in report["problems"] if p["severity"] == "error"]
        if report["errors"] != len(errors) or result.returncode != int(bool(errors)):
            raise ValueError("exit status disagrees with the reported errors")
    except (ValueError, KeyError, TypeError) as error:
        raise ValueError(f"BLIND {name}: {error}\n{result.stdout[-2000:]}\n{result.stderr[-2000:]}") from error
    for p in errors:
        print(f"{name}: [{p['code']}] {p['target']}: {p['message']}")
    return errors


def pinned_revision():
    """The revision CI installs the inspector from (single source: the workflow)."""
    workflow = ROOT / ".github/workflows/ci.yml"
    if not workflow.is_file():
        return "unknown"
    for line in workflow.read_text().splitlines():
        if line.strip().startswith("GXKIT_REV:"):
            return line.split("GXKIT_REV:", 1)[1].strip()
    return ""


def find_inspector():
    """The inspector the operator pointed at, else the one on PATH.

    Deliberately not a search of the host's caches: a gate that picks its own
    inspector cannot be told which one it used. `tools/layers_check.sh`, the
    documented entry point, is where a host-local build of the pinned revision is
    preferred; this function stays authoritative about the choice it is given.
    """
    explicit = os.environ.get("MC_SPINE_DEPSGX")
    if explicit:
        return explicit
    return shutil.which("depsgx")


def main():
    tool = find_inspector()
    if not tool:
        rev = pinned_revision()
        raise ValueError(
            "depsgx is required; an absent inspector cannot satisfy the gate. Install the "
            f"pinned revision ({rev}) as CI does, set MC_SPINE_DEPSGX to an existing build, "
            "or put one on PATH"
        )
    probe = inspect(tool, "gate_probe.py")
    expected = Counter({"cycle": 2, "layer-violation": 1, "forbidden-dependency": 1})
    if Counter(p["code"] for p in probe) != expected:
        raise ValueError("inspector probe must report both independent cycles, layer-violation and forbidden-dependency")
    policy = json.loads(POLICY.read_text())
    graph = source_graph()
    actual = set(map(tuple, graph["depends"]))
    allowed = set(map(tuple, policy["allowed_edges"]))
    failed = actual != allowed
    if failed:
        print(f"FAIL source edge ratchet: added={sorted(actual - allowed)}, removed={sorted(allowed - actual)}")
    if inspect(tool, "deps_target.py"):
        print("FAIL intended design graph")
        failed = True
    current = inspect(tool, "deps.py")
    identities = Counter((p["code"], p["target"]) for p in current)
    if identities != Counter(map(tuple, policy["accepted_errors"])):
        print(f"FAIL current error identities: {dict(identities)}; resolve debt and lock the policy")
        failed = True
    print(f"source graph: {len(graph['modules'])} modules, {len(actual)} edges, {len(current)} named errors")
    if failed:
        return 1
    print("layers check: PASS (current debt is named, not resolved)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        print(f"layers check: FAIL: {error}", file=sys.stderr)
        sys.exit(1)
