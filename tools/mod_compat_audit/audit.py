#!/usr/bin/env python3
"""Compile representative 1.21.1 JARs and measure the pinned shimcheck surface."""

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit
from urllib.request import urlopen
from zipfile import ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
from java_toolchain import require_java_home
ADAPTER = ROOT / "compat/neoforge-events"
SURFACE = ADAPTER / "shim-surface.json"
SCANNER = Path("java/net/megu/neoforge/ShimCheck.java")
FIXTURES = Path(__file__).resolve().parent / "fixtures"
CASES = ("gameplay", "content", "automation", "network", "transforms", "implicit_at", "fabric")
MAX_JAR_BYTES = 20 * 1024 * 1024


def digest(data):
    return hashlib.sha256(data).hexdigest()


def pinned_libraries(directory):
    neo = json.loads((ADAPTER / "dependencies.json").read_text())
    fabric = json.loads((ROOT / "compat/fabric-probe/dependencies.json").read_text())
    pins = [neo[name] for name in ("asm", "neoforge", "bus", "fml", "distmarker", "annotations")]
    pins.extend(pin for pin in fabric["libraries"] if pin["name"].startswith("net.fabricmc:"))
    directory.mkdir()
    paths = []
    for pin in pins:
        name = Path(urlsplit(pin["url"]).path).name
        cached = ADAPTER / ".cache" / name
        if cached.is_file():
            data = cached.read_bytes()
        else:
            with urlopen(pin["url"], timeout=30) as response:
                data = response.read(MAX_JAR_BYTES + 1)
        if len(data) > MAX_JAR_BYTES or digest(data) != pin["sha256"]:
            raise ValueError(f"pinned dependency hash/size mismatch: {name}")
        path = directory / name
        path.write_bytes(data)
        paths.append(path)
    return paths


def javac(java_home, env, output, sources, classpath=()):
    output.mkdir()
    command = [str(java_home / "bin/javac"), "--release", "21", "-proc:none", "-d", str(output)]
    if classpath:
        command.extend(["-cp", os.pathsep.join(map(str, classpath))])
    checked(command + [str(path) for path in sorted(sources)], env)


def checked(command, env, accepted=(0,)):
    result = subprocess.run(command, capture_output=True, text=True, env=env, timeout=120)
    if result.returncode not in accepted:
        raise ValueError(f"{Path(command[0]).name} failed ({result.returncode}):\n"
                         f"{result.stdout}{result.stderr}")
    return result


def neo_metadata(fixture):
    mod_id = "audit_" + fixture
    metadata = f'''modLoader="javafml"
loaderVersion="[4.0.39]"
license="LGPL-3.0-or-later"

[[mods]]
modId="{mod_id}"
version="1.0.0"
displayName="Audit {fixture}"

[[dependencies.{mod_id}]]
modId="minecraft"
type="required"
versionRange="[1.21.1]"
side="BOTH"

[[dependencies.{mod_id}]]
modId="neoforge"
type="required"
versionRange="[21.1.172]"
side="BOTH"
'''
    if fixture == "transforms":
        metadata += '''
[[mixins]]
config="transforms.mixins.json"

[[mixins]]
config="hooks.mixins.json"

[[accessTransformers]]
file="META-INF/accesstransformer.cfg"
'''
    return metadata.encode()


def build_fixture(fixture, java_home, env, work, classpath):
    source = FIXTURES / fixture
    classes = work / fixture
    javac(java_home, env, classes, source.glob("*.java"), classpath)
    entries = {path.relative_to(classes).as_posix(): path.read_bytes()
               for path in classes.rglob("*.class")}
    resources = source / "resources"
    entries.update({path.relative_to(resources).as_posix(): path.read_bytes()
                    for path in resources.rglob("*") if path.is_file()})
    if fixture != "fabric":
        entries["META-INF/neoforge.mods.toml"] = neo_metadata(fixture)
    jar = work / f"{fixture}.jar"
    with ZipFile(jar, "w") as archive:
        for name, data in sorted(entries.items()):
            archive.writestr(ZipInfo(name), data)
    return jar


def run_shimcheck(work, java_home, env, asm, jars):
    # Stage the unmodified wrapper and current scanner in a scratch root. The wrapper
    # resolves .build relative to itself, so it can reuse this scanner-only classpath.
    shim_root = work / "shimcheck"
    wrapper = shim_root / "tools/shimcheck.py"
    wrapper.parent.mkdir(parents=True)
    shutil.copyfile(ROOT / "tools/shimcheck.py", wrapper)
    adapter = shim_root / "compat/neoforge-events"
    scanner = adapter / SCANNER
    scanner.parent.mkdir(parents=True)
    shutil.copyfile(ADAPTER / SCANNER, scanner)
    surface = adapter / SURFACE.name
    shutil.copyfile(SURFACE, surface)
    build = adapter / ".build"
    build.mkdir(parents=True)
    classes = build / "classes"
    javac(java_home, env, classes, [scanner], [asm])
    (build / "classpath.txt").write_text(os.pathsep.join(map(str, [classes, asm])))
    result = checked([sys.executable, str(wrapper), "--json", "--surface", str(surface),
                      "--java-home", str(java_home), *map(str, jars)], env, accepted=(0, 2))
    report = json.loads(result.stdout)
    if (report["status"], result.returncode) not in {("COMPATIBLE", 0), ("REJECTED", 2)}:
        raise ValueError("shimcheck JSON status disagrees with exit code")
    if [Path(row["jar"]) for row in report["jars"]] != jars:
        raise ValueError("shimcheck did not report exactly the submitted fixtures")
    report["surface_sha256"] = digest(surface.read_bytes())
    report["scanner_sha256"] = digest(scanner.read_bytes())
    return report


def split_symbols(symbols):
    classes, members = [], []
    for symbol in symbols:
        owner, separator, member = symbol.partition("#")
        if not separator:
            classes.append(owner)
            continue
        if ":" in member:
            name, descriptor = member.split(":", 1)
        else:
            name, descriptor = member.split("(", 1)
            descriptor = "(" + descriptor
        members.append([owner, name, descriptor])
    return sorted(classes), sorted(members)


def summarize(row):
    supported_classes, supported_members = split_symbols(row["supportedSymbols"])
    unsupported_classes, unsupported_members = split_symbols(row["unsupportedSymbols"])
    jar = Path(row["jar"])
    with ZipFile(jar) as archive:
        resources = sorted(name for name in archive.namelist() if not name.endswith(".class"))
    return {
        "fixture": jar.stem,
        "status": row["status"],
        "resources": resources,
        "supported_classes": supported_classes,
        "unsupported_classes": unsupported_classes,
        "supported_members": supported_members,
        "unsupported_members": unsupported_members,
        "client_classes": row["clientClasses"],
        "diagnostics": row["reasons"],
    }


def audit(java_home, tmp):
    with tempfile.TemporaryDirectory(prefix="mod-surface-", dir=tmp) as directory:
        work = Path(directory)
        env = os.environ.copy()
        env["TMPDIR"] = str(tmp)
        env["_JAVA_OPTIONS"] = f'-Djava.io.tmpdir="{tmp}" -XX:-UsePerfData'
        asm, *fixture_libraries = pinned_libraries(work / "libraries")
        stubs = work / "compile-only"
        javac(java_home, env, stubs, FIXTURES.joinpath("api").rglob("*.java"))
        classpath = [stubs, *fixture_libraries]
        jars = [build_fixture(case, java_home, env, work, classpath) for case in CASES]
        result = run_shimcheck(work, java_home, env, asm, jars)
        rows = [summarize(row) for row in result["jars"]]

    counts = {}
    for key in ("supported_members", "unsupported_members", "supported_classes", "unsupported_classes"):
        values = [value for row in rows for value in row[key]]
        counts[key] = len(values)
        counts["distinct_" + key] = len({tuple(value) if isinstance(value, list) else value
                                         for value in values})
    return {
        "minecraft": result["minecraft"],
        "neoforge": result["neoforge"],
        "status": result["status"],
        "surface_sha256": result["surface_sha256"],
        "scanner_sha256": result["scanner_sha256"],
        "counts": counts,
        "fixtures": rows,
    }


def markdown(report):
    lines = [
        "| Fixture | Shimcheck | Members supported / rejected | Classes supported / rejected | Other diagnostics |",
        "|---|---|---:|---:|---|",
    ]
    for row in report["fixtures"]:
        diagnostics = Counter(reason["code"] for reason in row["diagnostics"]
                              if reason["code"] != "UNSUPPORTED_SYMBOL")
        diagnostic_text = ", ".join(f"`{code}` × {count}" for code, count in sorted(diagnostics.items())) or "—"
        lines.append(f"| `{row['fixture']}` | {row['status']} | "
                     f"{len(row['supported_members'])} / {len(row['unsupported_members'])} | "
                     f"{len(row['supported_classes'])} / {len(row['unsupported_classes'])} | {diagnostic_text} |")
    counts = report["counts"]
    passed = sum(row["status"] == "COMPATIBLE" for row in report["fixtures"])
    lines.extend([
        "",
        f"{passed}/{len(report['fixtures'])} JARs returned `COMPATIBLE`. "
        f"Across fixture rows, {counts['supported_members']} member references were supported "
        f"and {counts['unsupported_members']} rejected, with {counts['supported_classes']} "
        f"supported class references and {counts['unsupported_classes']} rejected. "
        f"Across the corpus, {counts['distinct_supported_members']} distinct member triples were supported "
        f"and {counts['distinct_unsupported_members']} rejected, with {counts['distinct_supported_classes']} "
        f"distinct classes supported and {counts['distinct_unsupported_classes']} rejected.",
        "",
        f"- Surface SHA-256 `{report['surface_sha256']}`",
        f"- Scanner source SHA-256 `{report['scanner_sha256']}`",
    ])
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    parser.add_argument("--java-home", type=Path)
    args = parser.parse_args()
    java_home = args.java_home or require_java_home()
    java_home = java_home.resolve()
    tmp = ROOT / "target/tmp"
    try:
        tmp.mkdir(parents=True, exist_ok=True)
        report = audit(java_home, tmp)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"mod surface audit: {error}", file=sys.stderr)
        return 1
    print(markdown(report) if args.format == "markdown" else json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
