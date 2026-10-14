#!/usr/bin/env python3
"""Fail closed on real source edges, named debt, or an inspector that cannot fail."""
from collections import Counter
import json
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


def main():
    tool = shutil.which("depsgx")
    if not tool:
        raise ValueError("depsgx is required; an absent inspector cannot satisfy the gate")
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
