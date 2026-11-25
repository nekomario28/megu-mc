#!/usr/bin/env python3
"""Check the actual-native contract for placing a block that carries NBT.

Runs `TemplateInitOracle` against the pinned 1.21.1 jar: it loads a `RegistryAccess`
from the vanilla datapack (so the chunk can answer biomes), places a template whose
chest carries `LootTable` + `CustomName`, and reports every written cell with its
canonical state and a SHA-256 of the block entity's own `saveWithFullMetadata`.

What this pins: how native decodes and applies the tag (components re-encoded, a
`LootTableSeed` generated), and that the state is written rotated before the tag is
loaded. What it does NOT pin: that a level creates the block entity, because a
`ProtoChunk` does not and `LevelChunk` needs a server; the oracle registers one at
the target itself. It also does not pin entities, packets, or save/reload.

Regenerate only with `--write`; the default run compares against the checked fixture.
"""
import argparse
import io
import pathlib
import sys
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parents[1]
FIXTURE = REPO / "tests/data/template_init_1211.tsv"
COLUMNS = ("case", "placed", "cell", "state", "nbt", "contents", "text")


def datapack(inner, work):
    """The vanilla datapack half of the inner jar, which the registry loader reads."""
    target = pathlib.Path(work) / "datapack"
    target.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(inner)) as jar:
        jar.extractall(target)
    return target


def validate_rows(text):
    """Fail closed: every case must report its own cells, once, in order."""
    rows = [line.split("\t") for line in text.splitlines() if line.strip()]
    if not rows:
        raise ValueError("native initialization output is empty")
    seen = set()
    for row in rows:
        if len(row) != len(COLUMNS):
            raise ValueError(f"expected {len(COLUMNS)} columns, got {len(row)}: {row}")
        case, placed, cell, state, nbt, contents, text = row
        if placed not in ("true", "false"):
            raise ValueError(f"placed must be true or false: {row}")
        key = (case, cell)
        if key in seen:
            raise ValueError(f"duplicate cell {cell} in case {case}")
        seen.add(key)
        x, y, z = (int(v) for v in cell.split(","))
        if (x, y, z) != (int(cell.split(",")[0]), y, z):
            raise ValueError(f"cell is not x,y,z: {cell}")
        if not state.startswith("minecraft:"):
            raise ValueError(f"state must be namespaced: {state}")
        if nbt != "absent" and (len(nbt) != 64 or any(c not in "0123456789abcdef" for c in nbt)):
            raise ValueError(f"nbt must be a SHA-256 or 'absent': {nbt}")
        if contents != "-":
            for entry in contents.split(";"):
                slot, _, stack = entry.partition("=")
                if not slot.isdigit() or " " not in stack:
                    raise ValueError(f"contents entry must be slot=count id: {entry}")
        if contents != "-" and nbt == "absent":
            raise ValueError(f"contents reported without a block entity: {cell}")
        if text != "-" and nbt == "absent":
            raise ValueError(f"sign text reported without a block entity: {cell}")
        if text != "-" and len(text.split("|")) != 4:
            raise ValueError(f"a sign side has four lines: {text}")
    cases = {row[0] for row in rows}
    if cases != {"none", "clockwise_90"}:
        raise ValueError(f"both rotations must be covered, got {sorted(cases)}")
    # The template writes two cells per placement; a case that reports another count
    # has lost or duplicated a write, which is exactly what this oracle exists to see.
    for case in cases:
        cells = [row for row in rows if row[0] == case]
        if len(cells) != 4:
            raise ValueError(f"case {case} reported {len(cells)} cells, expected 4")
    # The negative control is the point: a cell without NBT must report no entity.
    if not any(row[4] == "absent" for row in rows):
        raise ValueError("no cell without a block entity: the control is missing")
    # And the placed case must actually carry an entity, or nothing was initialized.
    if not any(row[4] != "absent" for row in rows):
        raise ValueError("no cell with a block entity: nothing was initialized")
    # One cell must show decoded stacks: a fixture with entities but no contents would
    # pass while the item component path was broken.
    if not any(row[5] != "-" for row in rows):
        raise ValueError("no cell with decoded contents: the item path was not exercised")
    # And the sign must carry text, or the block-entity family with components is untested.
    if not any(row[6] != "-" for row in rows):
        raise ValueError("no cell with sign text: the component path was not exercised")
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--jar")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()
    with pinned_runtime(pathlib.Path(args.jar) if args.jar else DEFAULT_JAR) as (work, classpath, inner):
        data = datapack(inner, work)
        stdout = run_probe(
            work,
            classpath,
            REPO / "tools/structure-oracle/TemplateInitOracle.java",
            "dev.megu.tools.TemplateInitOracle",
            data,
            extra_sources=(REPO / "tools/structure-oracle/NativeHarness.java",
                           REPO / "tools/structure-oracle/StructureTemplateOracle.java"),
            capture=True,
        )
    rows = rows_from_stdout(stdout)
    text = "".join("\t".join(row) + "\n" for row in rows)
    if args.write:
        FIXTURE.write_text(text)
        print(f"WRITTEN {FIXTURE} ({len(rows)} rows)")
        return 0
    expected = FIXTURE.read_text()
    if expected != text:
        print("template initialization: MISMATCH", file=sys.stderr)
        print("--- expected\n" + expected + "--- actual\n" + text, file=sys.stderr)
        return 1
    print(f"template initialization: {len(rows)} cells MATCH")
    return 0


def rows_from_stdout(text):
    rows = []
    for line in text.splitlines():
        marker = "INIT\t"
        at = line.find(marker)
        if at != -1:
            rows.append(line[at + len(marker):])
    return validate_rows("\n".join(rows))


if __name__ == "__main__":
    sys.exit(main())
