#!/usr/bin/env python3
"""Drive the real login/configuration/play paths and assert received chunk sets."""
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

from wire_smoke import frame, port, recv_exact, recv_vint, status, vint

ROOT = Path(__file__).resolve().parents[1]


def packet(sock):
    data = recv_exact(sock, recv_vint(sock))
    # Protocol 767's packets used here all have single-byte ids.
    assert data and data[0] < 128, "unexpected packet id"
    return data[0], data[1:]


def settings(view):
    # locale, signed view byte, chat mode, colours, skin parts, hand, filtering, listings
    return vint(5) + b"en_us" + struct.pack("b", view) + bytes([0, 1, 127, 1, 0, 1])


def until(sock, target, held):
    deadline = time.monotonic() + 10
    for _ in range(2048):
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"deadline waiting for packet {target:#x}"
        sock.settimeout(remaining)
        pid, body = packet(sock)
        if pid == 0x27:
            coord = struct.unpack(">ii", body[:8])
            assert coord not in held, f"duplicate chunk {coord}"
            held.add(coord)
        elif pid == 0x21:
            z, x = struct.unpack(">ii", body)
            assert (x, z) in held, f"unload of unknown chunk {(x, z)}"
            held.remove((x, z))
        if pid == target:
            return body
    raise AssertionError(f"no packet {target:#x} within bounded receive")


def square(radius, cx=0, cz=0):
    return {(cx + x, cz + z) for x in range(-radius, radius + 1) for z in range(-radius, radius + 1)}


def session(binary, late):
    with tempfile.TemporaryDirectory(prefix="spine-settings-") as tmp:
        root = Path(tmp)
        shutil.copy2(ROOT / "config_packets.bin", root / "config_packets.bin")
        # Use the real registry, without writing to the developer's world or assets.
        (root / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
        game = port()
        env = dict(os.environ, MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
                   MC_SPINE_WG="old", MC_SPINE_RESOURCE_PACK_URL="")
        env.pop("MC_SPINE_RCON_PORT", None)
        with (root / "server.log").open("w+") as log:
            proc = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=log)
            contract_started = False
            try:
                for _ in range(100):
                    assert proc.poll() is None, "server exited before login"
                    try:
                        status(game)
                        break
                    except (OSError, AssertionError):
                        time.sleep(0.05)
                else:
                    raise AssertionError("listener did not become ready")
                with socket.create_connection(("127.0.0.1", game), timeout=10) as sock:
                    sock.settimeout(10)
                    handshake = vint(767) + vint(9) + b"localhost" + struct.pack(">H", game) + vint(2)
                    sock.sendall(frame(0, handshake) + frame(0, vint(10) + b"WireTester" + bytes(16)))
                    assert packet(sock)[0] == 2, "login success"
                    sock.sendall(frame(3))
                    until(sock, 0x0e, set())
                    client_info = frame(0, settings(2))
                    known = frame(7, vint(0))
                    sock.sendall(known + client_info if late else client_info + known)
                    contract_started = True
                    until(sock, 3, set())
                    sock.sendall(frame(3))
                    held = set()
                    until(sock, 0x0c, held)
                    assert held == square(2), f"configuration {'late' if late else 'early'}: {len(held)} chunks"
                    if not late:
                        sock.sendall(frame(0x0a, settings(3)))
                        until(sock, 0x0c, held)
                        assert held == square(3), "play increase must send exactly the missing chunks"
                        sock.sendall(frame(0x0a, settings(2)))
                        until(sock, 0x54, held)
                        assert held == square(2), "play decrease must unload outside the new window"
                        sock.sendall(frame(0x0a, settings(127)))
                        until(sock, 0x0c, held)
                        assert held == square(8), "server maximum must limit the requested window"
                        sock.sendall(frame(0x0a, settings(-1)))
                        until(sock, 0x54, held)
                        assert held == square(2), "negative signed byte must not select the maximum"
                        sock.sendall(frame(0x1a, struct.pack(">ddd?", -24.0, 16.0, 24.0, True)))
                        until(sock, 0x0c, held)
                        assert held == square(2, -2, 1), "movement must retain the selected radius"
                        sock.sendall(frame(0x0a, settings(3)))
                        until(sock, 0x0c, held)
                        assert held == square(3, -2, 1), "settings must use the current player position"
                print(f"client settings wire: configuration {'late' if late else 'early'} PASS")
            except Exception as error:
                if contract_started and isinstance(error, (AssertionError, TimeoutError)):
                    print("client settings wire: FAIL", file=sys.stderr)
                log.flush()
                log.seek(0)
                print(log.read()[-4000:], file=sys.stderr)
                raise
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)


def main():
    binary = Path(sys.argv[1]).resolve()
    for late in [False, True]:
        session(binary, late)
    print("client settings wire: PASS")


if __name__ == "__main__":
    main()
