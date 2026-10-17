#!/usr/bin/env python3
"""Dump the light a server sends for one chunk, canonically, to compare with vanilla.

  tools/light_parity_probe.py <port> <cx> <cz>

Prints the sky/block light masks and, for each section in the sky then block mask, a digest of
that section's 2048-byte array. Two servers with the same terrain and the same light engine print
the same masks and digests. This reads the protocol (0x27) the client already decodes; it is the
wire-level check, not the excluded on-screen one.
"""
import hashlib
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests"))
import protocol767  # noqa: E402
from protocol767 import Client  # noqa: E402

# A live server's air state ids need not match the fixture's set; only the light is read here.
protocol767.STRICT = False


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    port, cx, cz = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
    with Client(port, "LightProbe") as c:
        c.login(view=2)
        c.move(cx * 16 + 8, 80, cz * 16 + 8)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and (cx, cz) not in c.chunk_light:
            try:
                c.read(1.0)
            except TimeoutError:
                break
        if (cx, cz) not in c.chunk_light:
            print(f"no chunk packet for {(cx, cz)}")
            return 1
        masks, sky, block = c.chunk_light[(cx, cz)]
        print(f"chunk {cx},{cz} sky_mask={masks[0]:#x} block_mask={masks[1]:#x}")
        for kind, arrs in (("sky", sky), ("block", block)):
            for i, a in enumerate(arrs):
                print(f"{kind}[{i}] {hashlib.sha256(a).hexdigest()[:16]}")
        # Readable skylight at the surface: the highest set nibble per column of the top sky section.
        if sky:
            top = sky[-1]
            cols = []
            for z in range(16):
                for x in range(16):
                    idx = (15 << 8) | (z << 4) | x
                    cols.append((top[idx >> 1] >> (4 * (idx & 1))) & 0xF)
            print("top_sky_cols", "".join(f"{v:x}" for v in cols))
    return 0


if __name__ == "__main__":
    sys.exit(main())
