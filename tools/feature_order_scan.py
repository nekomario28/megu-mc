#!/usr/bin/env python3
"""Report which worldgen classes contain an order-sensitive iteration construct.

  tools/feature_order_scan.py [--jar PATH] [--mappings PATH] [--class-prefix P ...]

Why: "vanilla walks this box with `betweenClosed`" / "vanilla collects into a `HashSet`" is
the kind of claim that is right about the mechanism and wrong about the direction, and it is
the claim behind a whole class of RNG-parity bugs (a port that visits the same cells in a
different order keeps every draw count and permutes which cell receives which value). This
script answers "where does the reference even have an order to get wrong" from the pinned
jar instead of from memory, so an audit starts from a list rather than from recall.

What it matches (obfuscated owners resolved through the official mappings, so the table is
readable):

  betweenClosed            Method jd.(c:(Ljd;Ljd;) | b:(IIIIII) | d:(Ljd;Ljd;))   x fastest, z slowest
  betweenClosedStream(BB)  Method jd.a:(Ldda;)Ljava/util/stream/Stream;
  randomBetweenClosed      Method jd.a:(Layw;IIIIIII)Ljava/lang/Iterable;
  breadthFirstTraversal    Method jd.a:(Ljd;IILjava/util/function/BiConsumer;
  spiralAround             Method jd.a:(Ljd;ILji;Lji;)
  Direction.values()       Method ji.values:()[Lji;
  Plane.HORIZONTAL         Field ji$c.a:Lji$c;            (NORTH, EAST, SOUTH, WEST)
  Plane.getShuffled        Method ji$c.c:(Layw;)Ljava/util/List;
  Sets.newHashSet          Sets.newHashSet | java/util/HashSet."<init>"
  Util.shuffledCopy        Method ad.a:(Lit/unimi/dsi/fastutil/objects/ObjectArrayList;Layw;)

`javap` output is produced once for every class in the selected packages (414 for
`levelgen.feature` + `levelgen.structure`), so the whole scan is one pass rather than one
`javap` per class.

Pinned inputs (see `tools/bytecode_probe.sh` for the same jar/mapping contract):

  server.jar      sha1   59353fb40c36d304f2035d51e7d6e6baa98dc05c
  official maps   server.txt  sha1 03f8985492bda0afc0898465341eb0acef35f570

Exit codes: 0 scan completed, 2 invalid or missing input.
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
MAPS_SHA1 = "03f8985492bda0afc0898465341eb0acef35f570"

# name -> (regex over javap output, one-line description)
SIGNATURES = [
    (
        "betweenClosed",
        r"Method jd\.(?:c:\(Ljd;Ljd;\)|b:\(IIIIII\)|d:\(Ljd;Ljd;\))",
        "x fastest, then y, then z",
    ),
    (
        "betweenClosedStream(BB)",
        r"Method jd\.a:\(Ldda;\)Ljava/util/stream/Stream;",
        "x fastest, then y, then z",
    ),
    (
        "randomBetweenClosed",
        r"Method jd\.a:\(Layw;IIIIIII\)",
        "count > 0: three nextInt per sample, x then y then z",
    ),
    (
        "breadthFirstTraversal",
        r"Method jd\.a:\(Ljd;IILjava/util/function/BiConsumer;",
        "queue order is the visit order",
    ),
    (
        "spiralAround",
        r"Method jd\.a:\(Ljd;ILji;Lji;\)",
        "ring order is the visit order",
    ),
    ("Direction.values()", r"Method ji\.values:\(\)\[Lji;", "DOWN, UP, NORTH, SOUTH, WEST, EAST"),
    ("Plane.HORIZONTAL", r"Field ji\$c\.a:Lji\$c;", "NORTH, EAST, SOUTH, WEST"),
    ("Plane.getShuffled", r"Method ji\$c\.c:\(Layw;\)Ljava/util/List;", "plane array, shuffled"),
    (
        "newHashSet",
        r'(?:Sets\.newHashSet|java/util/HashSet\."<init>")',
        "bucket order, not insertion order",
    ),
    (
        "shuffledCopy",
        r"Method ad\.a:\(Lit/unimi/dsi/fastutil/objects/ObjectArrayList;Layw;\)",
        "Util.shuffledCopy: Fisher-Yates from the end",
    ),
]


def sha1(path: Path) -> str:
    import hashlib

    digest = hashlib.sha1()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def load_mappings(path: Path) -> dict:
    """obfuscated name -> official name ("a" -> "net.minecraft.core.BlockPos")."""
    names = {}
    for line in path.read_text(errors="replace").splitlines():
        if " -> " not in line or not line.startswith(("net.minecraft", "com.mojang")):
            continue
        official, obf = line.split(" -> ", 1)
        names[obf.strip().rstrip(":")] = official.strip()
    return names


def class_entries(mappings: dict, prefixes) -> dict:
    """obf name -> official name for every mapped class under the requested prefixes."""
    out = {}
    for obf, official in mappings.items():
        if not re.fullmatch(r"[a-z]+(\$[a-z]+)*", obf):
            continue
        if any(official.startswith(prefix) for prefix in prefixes):
            out[obf] = official
    return out


def main() -> int:
    parser = argparse.ArgumentParser(
        description="list the worldgen classes that iterate positions or directions in an "
        "order-sensitive way, read off the pinned server jar"
    )
    parser.add_argument(
        "--jar",
        default=os.environ.get("MEGU_VANILLA_JAR", "/tmp/carverfresh/server.jar"),
        help="pinned server.jar (MEGU_VANILLA_JAR)",
    )
    parser.add_argument(
        "--mappings",
        default=os.environ.get(
            "MEGU_VANILLA_MAPPINGS", "~/.cache/megu-work/mc121/server.txt"
        ),
        help="official mappings (MEGU_VANILLA_MAPPINGS)",
    )
    parser.add_argument(
        "--class-prefix",
        action="append",
        default=None,
        help="official-name prefix to scan (repeatable; default levelgen.feature and .structure)",
    )
    parser.add_argument(
        "--verify-sha1",
        action="store_true",
        help="require the pinned jar and mappings sha1s (a different jar is a different reference)",
    )
    parser.add_argument(
        "pattern",
        nargs="?",
        help="only print classes whose official name matches this Python regex",
    )
    args = parser.parse_args()

    jar = Path(args.jar).expanduser()
    maps = Path(args.mappings).expanduser()
    for path, what in ((jar, "server jar"), (maps, "official mappings")):
        if not path.is_file():
            print(f"feature_order_scan: no {what} at {path}", file=sys.stderr)
            return 2
    if args.verify_sha1:
        for path, want, what in ((jar, JAR_SHA1, "jar"), (maps, MAPS_SHA1, "mappings")):
            got = sha1(path)
            if got != want:
                print(
                    f"feature_order_scan: {what} is {got}, not the pinned {want}; "
                    "a different jar is a different reference",
                    file=sys.stderr,
                )
                return 2
    if not shutil.which("javap"):
        print("feature_order_scan: javap is required (a JDK, not just a JRE)", file=sys.stderr)
        return 2

    mappings = load_mappings(maps)
    prefixes = args.class_prefix or [
        "net.minecraft.world.level.levelgen.feature",
        "net.minecraft.world.level.levelgen.structure",
    ]
    entries = class_entries(mappings, prefixes)
    if not entries:
        print("feature_order_scan: the mappings map no class under those prefixes", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        # the outer jar is the bundler; the classes live in the nested one
        subprocess.run(
            ["unzip", "-o", "-q", str(jar), "META-INF/versions/*", "-d", str(tmp / "inner")],
            check=True,
        )
        inner = next(iter((tmp / "inner").rglob("server-*.jar")), None)
        if inner is None:
            print("feature_order_scan: the jar has no nested server jar", file=sys.stderr)
            return 2
        classes = tmp / "cls"
        classes.mkdir()
        # one unzip call per page of names, then one javap over everything
        obf = sorted(entries)
        chunk = 200
        for start in range(0, len(obf), chunk):
            subprocess.run(
                ["unzip", "-o", "-q", str(inner)]
                + [f"{name}.class" for name in obf[start : start + chunk]]
                + ["-d", str(classes)],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        extracted = sorted(p.stem for p in classes.glob("*.class"))
        listing = subprocess.run(
            ["javap", "-p", "-c", "-cp", str(classes)] + extracted,
            capture_output=True,
            text=True,
            check=False,
        )
        if listing.returncode != 0 and not listing.stdout.strip():
            print(f"feature_order_scan: javap failed: {listing.stderr.strip()}", file=sys.stderr)
            return 2

    lines = listing.stdout.splitlines()
    starts = [
        i
        for i, line in enumerate(lines)
        if not line.startswith(" ")
        and re.search(r"\b(class|interface|enum) \w+", line)
    ]
    hits = {}
    for index, start in enumerate(starts):
        match = re.search(r"\b(?:class|interface|enum) (\w+)", lines[start])
        if not match:
            continue
        name = match.group(1)
        end = starts[index + 1] if index + 1 < len(starts) else len(lines)
        body = "\n".join(lines[start:end])
        found = [label for label, pattern, _ in SIGNATURES if re.search(pattern, body)]
        if found:
            hits[name] = found

    want = re.compile(args.pattern) if args.pattern else None
    rows = []
    for name in sorted(hits):
        official = entries.get(name, name)
        if want and not want.search(official):
            continue
        rows.append((official, hits[name]))
    if not rows:
        print("feature_order_scan: no class matched", file=sys.stderr)
        return 2
    width = max(len(official) for official, _ in rows)
    for official, found in rows:
        print(f"{official:<{width}}  {', '.join(found)}")
    print(f"\n{len(rows)} of {len(extracted)} scanned classes carry an order-sensitive construct")
    return 0


if __name__ == "__main__":
    sys.exit(main())
