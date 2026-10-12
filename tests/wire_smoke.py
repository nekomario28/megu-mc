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
from pathlib import Path


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
        assert b"mc-spine" in response and b"767" in response, response[:100]


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
    env = os.environ.copy()
    env.update(MC_SPINE_PORT=str(game), MC_SPINE_RCON_PORT=str(rcon),
               MC_SPINE_RCON_PASS="local-test-secret", MC_SPINE_WG="old",
               MC_SPINE_BIND_ADDR="127.0.0.1")
    with tempfile.TemporaryDirectory(prefix="mc-spine-smoke-") as workdir:
        proc = subprocess.Popen([str(binary)], cwd=workdir, env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                if proc.poll() is not None:
                    raise AssertionError(f"server exited early: {proc.returncode}")
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
            print("wire smoke: status, malformed handshake, RCON rejection/auth PASS")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)


if __name__ == "__main__":
    main()
