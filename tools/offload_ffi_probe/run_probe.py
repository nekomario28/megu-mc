#!/usr/bin/env python3
import os, pathlib, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parents[1]
TMP = REPO / "target/tmp"
JAVA_HOME = pathlib.Path("/usr/lib/jvm/java-21-openjdk")
BUILD = ROOT / "target"
CLASSES = BUILD / "classes"
LIB = BUILD / "liboffload_ffi_probe.so"

def run(cmd):
    print("$", " ".join(map(str, cmd)), flush=True)
    subprocess.run(cmd, cwd=REPO, env={**os.environ, "TMPDIR": str(TMP)}, check=True)

def main():
    TMP.mkdir(parents=True, exist_ok=True)
    CLASSES.mkdir(parents=True, exist_ok=True)
    rustc = pathlib.Path.home() / ".cargo/bin/rustc"
    run([rustc, "--edition=2021", "--crate-type=cdylib", "-C", "opt-level=3", ROOT/"src/lib.rs", "-o", LIB])
    java_files = sorted((ROOT/"java").rglob("*.java"))
    run([JAVA_HOME/"bin/javac", "-d", CLASSES, *java_files])
    run([JAVA_HOME/"bin/java", f"-Djava.library.path={BUILD}", f"-Djava.io.tmpdir={TMP}", "-XX:-UsePerfData", "-cp", CLASSES, "probe.Probe"])

if __name__ == "__main__":
    main()
