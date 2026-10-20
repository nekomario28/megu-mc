#!/usr/bin/env python3
"""Replace the bundled block tags with the 1.21.1 server jar's.

    tools/vanilla_tags_sync.py <server.jar> [--check]

`datapacks/vanilla` is not 1.21.1 data: its biomes carried 1.21.2+ fields (fixed
by taking the biome files from the jar) and its block tags are newer too. The
tag skew is not cosmetic - a worldgen feature reads tags through its target
predicate, so `ore_dirt` could only replace dirt/coarse_dirt/rooted_dirt while
vanilla also replaces grass_block, podzol, mycelium, moss_block and mud, and
`deepslate_ore_replaceables` was missing tuff, so deepslate-variant ores could
not replace tuff at all.

Tags that exist only in the repository are left alone: they are newer data the
game logic may use, and writing a 1.21.1 file over them would delete entries
the 1.21.1 jar simply does not know about.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from vanilla_nofeat_pack import open_server_jar  # noqa: E402

PREFIX = "data/minecraft/tags/block/"
REPO = "datapacks/vanilla/data/minecraft/tags/block"


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    jar = sys.argv[1]
    check = "--check" in sys.argv
    z = open_server_jar(jar)
    names = sorted(n for n in z.namelist() if n.startswith(PREFIX) and n.endswith(".json"))
    if not names:
        print(f"{jar}: no block tags; pass the 1.21.1 server jar", file=sys.stderr)
        return 1
    added = changed = same = 0
    changed_names = []
    for name in names:
        base = os.path.basename(name)
        dst = os.path.join(REPO, base)
        raw = z.read(name)
        # Normalise so a rewrite of an identical tag reports as `same`.
        new = json.dumps(json.loads(raw), indent=2, sort_keys=False) + "\n"
        if not os.path.exists(dst):
            added += 1
            if not check:
                with open(dst, "w") as f:
                    f.write(new)
            continue
        old = open(dst).read()
        if json.loads(old) == json.loads(raw):
            same += 1
            continue
        changed += 1
        changed_names.append(base)
        if not check:
            with open(dst, "w") as f:
                f.write(new)
    verb = "would write" if check else "wrote"
    print(f"{verb}: {added} new, {changed} replaced, {same} already 1.21.1")
    for n in changed_names:
        print(f"  {n}")


if __name__ == "__main__":
    sys.exit(main())
