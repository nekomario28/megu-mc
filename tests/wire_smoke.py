#!/usr/bin/env python3
"""Bounded loopback smoke test for the experimental protocol 767 server.

Never connects to a live Minecraft service. Uses temporary working files and
disposable random ports. All sockets/processes are cleaned up.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path


SYNTHETIC_REGISTRY_ENTRIES = (
    "data/minecraft/dimension_type/overworld.json",
    "data/minecraft/worldgen/noise_settings/overworld.json",
    "data/minecraft/worldgen/biome/plains.json",
    "data/minecraft/tags/block/mineable/pickaxe.json",
    "data/minecraft/chat_type/chat.json",
    "data/minecraft/trim_pattern/sentry.json",
    "data/minecraft/trim_material/iron.json",
    "data/minecraft/wolf_variant/pale.json",
    "data/minecraft/painting_variant/alban.json",
    "data/minecraft/damage_type/in_fire.json",
    "data/minecraft/banner_pattern/creeper.json",
    "data/minecraft/enchantment/sharpness.json",
    "data/minecraft/jukebox_song/13.json",
)


def write_synthetic_vanilla_jar(path: Path) -> None:
    """Create a minimal clean-room ZIP carrying empty stubs for the 11 synced registries."""
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as jar:
        for entry in SYNTHETIC_REGISTRY_ENTRIES:
            info = zipfile.ZipInfo(entry, date_time=(2020, 1, 1, 0, 0, 0))
            jar.writestr(info, b"{}")


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def vint(value):
    out = bytearray()
    while True:
        b = value & 0x7F
        value >>= 7
        out.append(b | (0x80 if value else 0))
        if not value:
            return bytes(out)


def frame(packet_id, content=b""):
    body = vint(packet_id) + content
    return vint(len(body)) + body


def recv_exact(sock, n):
    out = bytearray()
    while len(out) < n:
        chunk = sock.recv(n - len(out))
        if not chunk:
            raise AssertionError("unexpected EOF")
        out.extend(chunk)
    return bytes(out)


def recv_vint(sock):
    value = 0
    for i in range(5):
        b = recv_exact(sock, 1)[0]
        value |= (b & 0x7F) << (7 * i)
        if not b & 0x80:
            return value
    raise AssertionError("oversized response VarInt")


def status(game_port):
    handshake = vint(767) + vint(9) + b"localhost" + struct.pack(">H", game_port) + vint(1)
    with socket.create_connection(("127.0.0.1", game_port), timeout=3) as sock:
        sock.settimeout(3)
        sock.sendall(frame(0, handshake) + frame(0))
        n = recv_vint(sock)
        response = recv_exact(sock, n)
        assert b"megu" in response and b"767" in response, response[:100]


def malformed_handshake(game_port):
    # Legitimate frame with impossible host length; should only close the client.
    content = vint(767) + vint(0x7FFFFFFF)
    with socket.create_connection(("127.0.0.1", game_port), timeout=3) as sock:
        sock.settimeout(3)
        sock.sendall(frame(0, content))
        assert not sock.recv(1), "malformed handshake unexpectedly accepted"


def rcon_frame(req_id, kind, text):
    body = struct.pack("<ii", req_id, kind) + text.encode() + b"\0\0"
    return struct.pack("<i", len(body)) + body


def rcon_response(sock):
    n = struct.unpack("<i", recv_exact(sock, 4))[0]
    assert 10 <= n <= 8200, n
    return recv_exact(sock, n)


def check_rcon(rcon_port):
    with socket.create_connection(("127.0.0.1", rcon_port), timeout=3) as sock:
        sock.settimeout(3)
        sock.sendall(rcon_frame(31, 3, "incorrect"))
        assert struct.unpack("<i", rcon_response(sock)[:4])[0] == -1
    with socket.create_connection(("127.0.0.1", rcon_port), timeout=3) as sock:
        sock.settimeout(3)
        sock.sendall(rcon_frame(41, 3, "local-test-secret"))
        assert struct.unpack("<i", rcon_response(sock)[:4])[0] == 41
        sock.sendall(rcon_frame(42, 2, "list"))
        assert struct.unpack("<i", rcon_response(sock)[:4])[0] == 42


def main():
    binary = Path(sys.argv[1]).resolve()
    game, rcon = port(), port()
    assert game != rcon
    with tempfile.TemporaryDirectory(prefix="megu-smoke-") as workdir:
        work = Path(workdir)
        base_env = os.environ.copy()
        base_env.update(
            MEGU_PORT=str(game),
            MEGU_RCON_PORT=str(rcon),
            MEGU_RCON_PASS="local-test-secret",
            MEGU_WG="old",
            MEGU_BIND_ADDR="127.0.0.1",
        )

        # 1. Fail-closed contract: starting without config_packets.bin or any
        #    vanilla datapack/JAR must exit 2 before binding the listener.
        unconfigured_env = base_env.copy()
        unconfigured_env["MEGU_VANILLA_JAR"] = str(work / "absent.jar")
        unconfigured = subprocess.run(
            [str(binary)],
            cwd=workdir,
            env=unconfigured_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15,
            check=False,
        )
        assert unconfigured.returncode == 2, (
            f"expected fail-closed exit 2 without vanilla data, got {unconfigured.returncode}: "
            f"{unconfigured.stderr[:400]}"
        )
        assert "configuration exchange not ready" in unconfigured.stderr, unconfigured.stderr[:400]

        # 2. Clean-room synthetic registry JAR satisfies pre-bind validation
        #    so status, malformed handshake, and RCON smoke checks run without
        #    Mojang assets in the repository.
        synthetic_jar = work / "synthetic-vanilla.jar"
        write_synthetic_vanilla_jar(synthetic_jar)
        env = base_env.copy()
        env["MEGU_VANILLA_JAR"] = str(synthetic_jar)

        proc = subprocess.Popen(
            [str(binary)],
            cwd=workdir,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            for _ in range(100):
                if proc.poll() is not None:
                    err = proc.stderr.read() if proc.stderr else ""
                    raise AssertionError(
                        f"server exited early: {proc.returncode}\n{err[:600]}"
                    )
                try:
                    status(game)
                    break
                except (OSError, AssertionError):
                    time.sleep(0.1)
            else:
                raise AssertionError("server status did not become available")
            malformed_handshake(game)
            status(game)  # malformed input did not kill the listener
            check_rcon(rcon)
            print("wire smoke: fail-closed boot, status, malformed handshake, RCON rejection/auth PASS")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
            if proc.stderr:
                proc.stderr.close()


if __name__ == "__main__":
    main()
