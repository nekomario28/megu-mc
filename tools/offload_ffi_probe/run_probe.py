#!/usr/bin/env python3
import hashlib
import os
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parents[1]
TMP = REPO / "target/tmp"
JAVA_HOME = pathlib.Path("/usr/lib/jvm/java-21-openjdk")
BUILD = ROOT / "target"
CLASSES = BUILD / "classes"
LIB = BUILD / "release/liboffload_ffi_probe.so"


def run(*cmd):
    print("$", " ".join(map(str, cmd)), flush=True)
    subprocess.run(cmd, cwd=REPO, env={**os.environ, "TMPDIR": str(TMP)}, check=True)


def main():
    TMP.mkdir(parents=True, exist_ok=True)
    CLASSES.mkdir(parents=True, exist_ok=True)
    run("cargo", "build", "--release", "--offline", "--locked",
        "--manifest-path", ROOT / "Cargo.toml", "--target-dir", BUILD)
    print(f"jni_sha256={hashlib.sha256(LIB.read_bytes()).hexdigest()} mtime_ns={LIB.stat().st_mtime_ns}",
          flush=True)
    java_files = sorted((ROOT / "java").rglob("*.java"))
    run(JAVA_HOME / "bin/javac", "-d", CLASSES, *java_files)
    run(JAVA_HOME / "bin/java", f"-Djava.library.path={LIB.parent}",
        f"-Djava.io.tmpdir={TMP}", "-XX:-UsePerfData", "-cp", CLASSES, "probe.Probe")


if __name__ == "__main__":
    main()
