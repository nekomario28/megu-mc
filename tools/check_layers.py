#!/usr/bin/env python3
"""Fail closed on real source edges, named debt, or an inspector that cannot fail."""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import types
from rust_module_graph import ROOT, POLICY, source_graph

DIR = ROOT / "evidence/foundation/module-layers"
# The limit only stops a hung inspector. On a loaded host the pinned inspector takes over
# 30 s, which used to fail the gate for timing alone. The env var overrides the default.
INSPECT_TIMEOUT = float(os.environ.get("MEGU_LAYERS_INSPECT_TIMEOUT", "180"))


class SpecGraph:
    """The `depsgx` module surface the three evidence specs in DIR use."""

    def __init__(self, modules=(), depends=(), roots=(), layers=(), forbidden=()):
        self.modules = [str(name) for name in modules]
        self.depends = [tuple(edge) for edge in depends]
        self.roots = list(roots)
        self.layers = [list(layer) for layer in layers]
        self.forbidden = {tuple(edge) for edge in forbidden}


def graph_problems(graph):
    """The findings the probe pins: every independent cycle, upward edge, forbidden edge."""
    rank = {name: i for i, layer in enumerate(graph.layers) for name in layer}
    adjacency = {name: set() for name in graph.modules}
    for source, target in graph.depends:
        adjacency.setdefault(source, set()).add(target)
        adjacency.setdefault(target, set())

    def reaches(start):
        seen, stack = set(), [start]
        while stack:
            for name in adjacency.get(stack.pop(), ()):
                if name not in seen:
                    seen.add(name)
                    stack.append(name)
        return seen

    forward = {name: reaches(name) for name in adjacency}
    problems, grouped = [], set()
    for name in sorted(adjacency):
        if name in grouped:
            continue
        cycle = {other for other in adjacency
                 if other in forward[name] and name in forward[other]}
        if len(cycle) > 1 or name in adjacency.get(name, ()):
            problems.append({"severity": "error", "code": "cycle", "target": min(cycle),
                             "message": "dependency cycle " + " <-> ".join(sorted(cycle))})
        grouped |= cycle or {name}
    for source, target in graph.depends:
        if (source, target) in graph.forbidden:
            problems.append({"severity": "error", "code": "forbidden-dependency",
                             "target": source,
                             "message": f"{source} may not depend on {target}"})
        if source in rank and target in rank and rank[target] < rank[source]:
            problems.append({"severity": "error", "code": "layer-violation",
                             "target": source,
                             "message": f"{source} depends upward on {target}"})
    return problems


def inspect_spec(path):
    """Execute one spec like `depsgx inspect` does, then analyze the graph it built."""
    shim = types.ModuleType("depsgx")
    shim.ModuleGraph = SpecGraph
    namespace = {"__file__": str(path), "__name__": "__main__"}
    previous = sys.modules.get("depsgx")
    sys.modules["depsgx"] = shim
    try:
        exec(compile(path.read_text(), str(path), "exec"), namespace)
    finally:
        if previous is None:
            sys.modules.pop("depsgx", None)
        else:
            sys.modules["depsgx"] = previous
    graph = namespace.get("graph")
    if not isinstance(graph, SpecGraph):
        raise ValueError(f"{path.name} did not assign a depsgx.ModuleGraph to `graph`")
    problems = graph_problems(graph)
    return {"errors": len(problems), "problems": problems}


def inspector_main(argv):
    """Bundled fallback inspector: `check_layers.py inspect FILE --json --no-storyboard`.

    A pure-Python equivalent of the pinned inspector, restricted to what this gate's
    specs exercise. It is never self-selected: `layers_check.sh` or the operator points
    MEGU_DEPSGX/--depsgx at it only when no depsgx build is installed.
    """
    parser = argparse.ArgumentParser(prog="check_layers.py inspect")
    parser.add_argument("spec")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--no-storyboard", action="store_true")
    args = parser.parse_args(argv)
    try:
        report = inspect_spec(Path(args.spec))
    except Exception as error:
        # A spec that raises or builds no graph must read as a blind inspector.
        print(f"inspect {args.spec}: {error}", file=sys.stderr)
        return 2
    print(json.dumps(report))
    return int(bool(report["errors"]))


def inspect(executable, name):
    command = [executable]
    if executable.endswith(".py"):
        command = [sys.executable, executable]
    result = subprocess.run(command + ["inspect", str(DIR / name), "--json", "--no-storyboard"],
                            capture_output=True, text=True, timeout=INSPECT_TIMEOUT)
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


def find_inspector(override=None):
    """The inspector the operator pointed at, else the one on PATH.

    Deliberately not a search of the host's caches: a gate that picks its own
    inspector cannot be told which one it used. `tools/layers_check.sh`, the
    documented entry point, is where a host-local build of the pinned revision -
    or the bundled pure-Python fallback - is preferred; this function stays
    authoritative about the choice it is given.
    """
    explicit = override or os.environ.get("MEGU_DEPSGX")
    if explicit:
        if not Path(explicit).is_file():
            raise ValueError(
                f"depsgx is required; the explicit inspector {explicit!r} does not exist"
            )
        return explicit
    return shutil.which("depsgx")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--depsgx",
                        help="inspector executable to use (else MEGU_DEPSGX, else PATH)")
    args = parser.parse_args()
    tool = find_inspector(args.depsgx)
    if not tool:
        rev = pinned_revision()
        raise ValueError(
            "depsgx is required; an absent inspector cannot satisfy the gate. Install the "
            f"pinned revision ({rev}) as CI does, set MEGU_DEPSGX to an existing build, "
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
        if sys.argv[1:2] == ["inspect"]:
            sys.exit(inspector_main(sys.argv[2:]))
        sys.exit(main())
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        print(f"layers check: FAIL: {error}", file=sys.stderr)
        sys.exit(1)
