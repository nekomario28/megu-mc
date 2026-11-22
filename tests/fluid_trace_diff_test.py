#!/usr/bin/env python3
"""Positive/negative controls for the independent 1.21.1 fluid trace comparator."""
from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from fluid_trace_diff import TraceError, compare, load_trace  # noqa: E402

SCRIPT = ROOT / "tools/fluid_trace_diff.py"
PREFIX = (
    "# format=megu-fluid-trace-v1\n"
    "# version=1.21.1\n"
    "# scenario=water_hole\n"
    "tick\tsequence\tx\ty\tz\tstate\n"
)
BASE = ["5\t0\t0\t64\t0\t34", "5\t1\t1\t64\t0\t35", "7\t0\t2\t63\t0\t36"]


def fixture(events: list[str], *, header: str = PREFIX, footer: bool = True) -> str:
    body = header + "".join(event + "\n" for event in events)
    return body + (f"# end=count={len(events)}\n" if footer else "")


def run(native_text: str, candidate_text: str) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory(prefix="fluid-trace-") as dirname:
        root = Path(dirname)
        a, b = root / "native.tsv", root / "candidate.tsv"
        a.write_text(native_text, encoding="utf-8")
        b.write_text(candidate_text, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--native", str(a), "--candidate", str(b)],
            cwd=ROOT, capture_output=True, text=True, check=False,
        )


def expect_invalid(native: str, candidate: str) -> None:
    result = run(native, candidate)
    assert result.returncode == 2, (result.returncode, result.stdout, result.stderr)
    assert "INVALID TRACE" in result.stderr


def main() -> None:
    identical = run(fixture(BASE), fixture(BASE))
    assert identical.returncode == 0, (identical.stdout, identical.stderr)
    assert '"status": "MATCH"' in identical.stdout

    changed = run(fixture(BASE), fixture(BASE[:1] + ["5\t1\t1\t64\t0\t36"] + BASE[2:]))
    assert changed.returncode == 1 and '"missing_events": 1' in changed.stdout
    assert '"extra_events": 1' in changed.stdout

    wrong_tick = run(fixture(BASE), fixture(BASE[:2] + ["8\t0\t2\t63\t0\t36"]))
    assert wrong_tick.returncode == 1 and '"status": "DIFF"' in wrong_tick.stdout

    # Same updates, different processing order must still fail.
    reordered = ["5\t0\t1\t64\t0\t35", "5\t1\t0\t64\t0\t34", BASE[2]]
    order = run(fixture(BASE), fixture(reordered))
    assert order.returncode == 1, (order.stdout, order.stderr)
    assert '"order_only_difference": true' in order.stdout

    duplicated = run(fixture(BASE), fixture(BASE[:2] + [BASE[1].replace("5\t1", "5\t2"), BASE[2]]))
    assert duplicated.returncode == 1 and '"extra_events": 1' in duplicated.stdout

    expect_invalid(fixture(BASE), fixture([]))
    expect_invalid(fixture(BASE), fixture(BASE, footer=False))
    expect_invalid(fixture(BASE), fixture(BASE).rstrip("\n"))
    expect_invalid(fixture(BASE), fixture(BASE).replace("# end=count=3", "# end=count=4"))
    expect_invalid(fixture(BASE), fixture(BASE).replace("5\t1", "5\t2"))
    expect_invalid(fixture(BASE), fixture(BASE).replace("7\t0", "4\t0"))
    expect_invalid(fixture(BASE), fixture(BASE).replace("34\n", "26684\n"))
    expect_invalid(fixture(BASE), fixture(BASE).replace("# version=1.21.1", "# version=1.21.8"))
    expect_invalid(fixture(BASE), fixture(BASE).replace("# scenario=water_hole", "# scenario=other"))
    print("fluid trace comparator: 14 positive/negative controls PASS")


if __name__ == "__main__":
    main()
