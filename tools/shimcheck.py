#!/usr/bin/env python3
"""Inspect pinned NeoForge mod JARs without loading their classes."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "compat" / "neoforge-events"
DEFAULT_SURFACE = ADAPTER / "shim-surface.json"


def main() -> int:
    parser = argparse.ArgumentParser(description="Fail-closed NeoForge 1.21.1 S0 compatibility audit")
    parser.add_argument("--json", action="store_true", help="emit deterministic JSON")
    parser.add_argument("--surface", type=Path, default=DEFAULT_SURFACE)
    parser.add_argument("--java-home", type=Path)
    parser.add_argument("jars", nargs="+", type=Path, metavar="mod.jar")
    args = parser.parse_args()

    java_home = args.java_home
    if java_home is None:
        candidates = [Path("/usr/lib/jvm/java-21-openjdk")]
        if os.environ.get("JAVA_HOME"):
            candidates.append(Path(os.environ["JAVA_HOME"]))
        java_home = next((item for item in candidates if (item / "bin/java").is_file()), None)
    if java_home is None:
        print("shimcheck: Java 21 not found (use --java-home)", file=sys.stderr)
        return 1
    java = java_home / "bin/java"
    javac = java_home / "bin/javac"
    if not java.is_file() or not javac.is_file():
        print(f"shimcheck: missing Java tools in {java_home}", file=sys.stderr)
        return 1

    tmp = ROOT / "target" / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["TMPDIR"] = str(tmp)
    env["_JAVA_OPTIONS"] = f"-Djava.io.tmpdir={tmp} -XX:-UsePerfData"
    classpath = ADAPTER / ".build" / "classpath.txt"
    if not classpath.is_file():
        try:
            subprocess.run([sys.executable, str(ADAPTER / "build.py"), "--java-home", str(java_home)],
                           cwd=ROOT, env=env, check=True, stdout=subprocess.DEVNULL)
        except subprocess.CalledProcessError as error:
            print(f"shimcheck: Java adapter build failed ({error.returncode})", file=sys.stderr)
            return 1
    try:
        cp = classpath.read_text(encoding="utf-8").strip()
        command = [str(java), "-XX:-UsePerfData", f"-Djava.io.tmpdir={tmp}", "-cp", cp,
                   "net.megu.neoforge.ShimCheck"]
        if args.json:
            command.append("--json")
        command += ["--surface", str(args.surface)]
        command += [str(path) for path in args.jars]
        return subprocess.run(command, cwd=ROOT, env=env, check=False).returncode
    except OSError as error:
        print(f"shimcheck: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
