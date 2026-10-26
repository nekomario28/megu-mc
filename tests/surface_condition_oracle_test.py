#!/usr/bin/env python3
"""Offline guards for the synthetic SurfaceRules condition corpus."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import surface_condition_oracle  # noqa: E402


def main():
    data = json.loads(surface_condition_oracle.FIXTURE.read_text())
    counts = surface_condition_oracle.validate(data)
    assert counts == {"rows": 4037, "kinds": ["hole", "prelim", "stone", "water"]}
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "server.jar"
        fake.write_bytes(b"not the pinned jar")
        out = Path(tmp) / "out.json"
        out.write_text("keep me")
        before = hashlib.sha256(out.read_bytes()).hexdigest()
        run = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "surface_condition_oracle.py"),
             "--jar", str(fake), "--output", str(out)],
            capture_output=True, text=True)
        assert run.returncode == 2, (run.returncode, run.stderr)
        assert hashlib.sha256(out.read_bytes()).hexdigest() == before
    print("surface condition oracle guards passed")


if __name__ == "__main__":
    main()
