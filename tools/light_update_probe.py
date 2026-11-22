#!/usr/bin/env python3
"""Runtime probe for the boundary generated-light update (Q).

Starts the server twice with the repository's data-pack worldgen, logs in a CLI client at
view distance 2, moves one chunk, and reports the chunks for which an Update Light packet
(0x2a) arrived. Flat worldgen is used as the control: it has no generated emitters, so it
must send none.

Usage: python3 tools/light_update_probe.py <release-binary>
No desktop client is involved; this observes the wire, not what is drawn.
"""
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
import protocol767  # noqa: E402
from protocol767 import Client, port, status  # noqa: E402


def lenient_chunk(body, _biome_count):
    """Parse only the coordinate: the probe reads light packets, not chunk contents."""
    reader = protocol767.Reader(body)
    return (reader.number(">i"), reader.number(">i")), [], [0, 0, 0, 0], [], []


protocol767.decode_chunk_light = lenient_chunk


def run(binary, wg):
    with tempfile.TemporaryDirectory(prefix="light-probe-") as tmp:
        root = Path(tmp)
        (root / "config_packets.bin").symlink_to(ROOT / "config_packets.bin")
        (root / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
        (root / "datapacks").symlink_to(ROOT / "datapacks", target_is_directory=True)
        game = port()
        env = dict(os.environ, MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
                   MEGU_RESOURCE_PACK_URL="")
        if wg:
            env["MEGU_WG"] = wg
        with (root / "server.log").open("w+") as log:
            proc = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=log)
            try:
                for _ in range(200):
                    if proc.poll() is not None:
                        raise AssertionError("server exited before login")
                    try:
                        status(game)
                        break
                    except (OSError, AssertionError):
                        time.sleep(0.05)
                else:
                    raise AssertionError("listener did not become ready")
                with Client(game, "LightProbe") as client:
                    client.login(view=2)
                    if client.pos is None:
                        client.until_id(0x40)
                    sx, sy, sz = client.pos
                    time.sleep(1.0)
                    client.move(sx + 16, sy, sz)
                    client.until_id(0x0c)
                    time.sleep(1.5)
                return sorted(set(client.light_updates))
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()


def main():
    binary = Path(sys.argv[1]).resolve()
    for wg in [None, "old"]:
        updates = run(binary, wg)
        print(f"wg={wg!r} light_updates={updates} count={len(updates)}")


if __name__ == "__main__":
    main()
