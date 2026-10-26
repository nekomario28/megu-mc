#!/usr/bin/env python3
"""Offline shape/provenance guards for the nextInt vector corpus."""
import hashlib
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import nextint_oracle  # noqa: E402


def main():
    info = nextint_oracle.validate(nextint_oracle.TSV)
    assert info["rows"] == 1116
    assert info["sha256"] == hashlib.sha256(nextint_oracle.TSV.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "server.jar"
        fake.write_bytes(b"not the pinned jar")
        run = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "nextint_oracle.py"),
             "--jar", str(fake)],
            capture_output=True, text=True)
        assert run.returncode == 2, (run.returncode, run.stderr)
    print("nextInt oracle guards passed")


if __name__ == "__main__":
    main()
