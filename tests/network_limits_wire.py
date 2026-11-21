#!/usr/bin/env python3
"""Loopback checks for the connection limits (src/connection_limits.rs).

  tests/network_limits_wire.py <binary>

Each scenario starts its own disposable server on a random loopback port with
the knobs it needs, so the defaults stay untouched for the other wire tests.
Not part of tests/verify.sh; run it by hand after changing the listener.
"""
import contextlib
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from protocol767 import Client, Reader, ROOT, frame_packet, string, unframe_packet
from wire_smoke import frame, port, recv_exact, recv_vint, rcon_frame, rcon_response, status, vint


@contextlib.contextmanager
def server(binary, **env_overrides):
    with tempfile.TemporaryDirectory(prefix="spine-limits-") as tmp:
        root = Path(tmp)
        (root / "config_packets.bin").symlink_to(ROOT / "config_packets.bin")
        (root / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
        game = port()
        env = dict(os.environ, MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
                   MC_SPINE_WG="old", MC_SPINE_RESOURCE_PACK_URL="")
        env.pop("MC_SPINE_RCON_PORT", None)
        env.update({k: str(v) for k, v in env_overrides.items()})
        log_path = root / "server.log"
        with log_path.open("w+") as log:
            proc = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=log)
            try:
                for _ in range(200):
                    assert proc.poll() is None, "server exited before ready"
                    try:
                        status(game)
                        break
                    except (OSError, AssertionError):
                        time.sleep(0.05)
                else:
                    raise AssertionError("listener did not become ready")
                yield game, proc, log_path
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)


def log_text(path):
    return path.read_text(errors="replace")


def closes_within(sock, seconds):
    """True when the server closes (or resets) the socket within `seconds`."""
    deadline = time.monotonic() + seconds
    sock.settimeout(0.2)
    while time.monotonic() < deadline:
        try:
            if not sock.recv(65536):
                return True
        except socket.timeout:
            continue
        except OSError:
            return True
    return False


def raw_login_start(game, name):
    sock = socket.create_connection(("127.0.0.1", game), timeout=5)
    sock.sendall(frame(0, vint(767) + string("localhost") + struct.pack(">H", game) + vint(2)))
    import hashlib
    uid = bytearray(hashlib.md5(("OfflinePlayer:" + name).encode()).digest())
    uid[6] = (uid[6] & 15) | 48
    uid[8] = (uid[8] & 63) | 128
    sock.sendall(frame(0, string(name) + uid))
    return sock


def cpu_seconds(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def duplicate_login(binary):
    with server(binary) as (game, _proc, log):
        with Client(game, "Twin") as first:
            first.login()
            dup = raw_login_start(game, "Twin")
            sock_len = recv_vint(dup)
            body = recv_exact(dup, sock_len)
            assert body[0] == 0 and b"already logged in" in body, body
            assert closes_within(dup, 3), "duplicate must be closed"
            dup.close()
            # the first session is unaffected
            first.move(0.5, 80.0, 0.5)
            # a different name is fine
            with Client(game, "Other") as other:
                other.login()
        # once the first session has logged out the name is free again
        time.sleep(1.0)
        with Client(game, "Twin") as again:
            again.login()
        text = log_text(log)
        assert "duplicate login refused for Twin from 127.0.0.1" in text, text[-1500:]
        assert "Twin joined from 127.0.0.1" in text
    print("duplicate login: PASS")


def allowlist(binary):
    with server(binary, MC_SPINE_ALLOWLIST="Friend") as (game, _proc, log):
        sock = raw_login_start(game, "Stranger")
        body = recv_exact(sock, recv_vint(sock))
        assert body[0] == 0 and b"white-listed" in body, body
        sock.close()
        with Client(game, "Friend") as ok:
            ok.login()
    print("allowlist: PASS")


def per_ip_limit(binary):
    with server(binary, MC_SPINE_MAX_CONN_PER_IP=2) as (game, _proc, log):
        time.sleep(0.5)  # let the readiness probe's own connection finish closing
        a = socket.create_connection(("127.0.0.1", game))
        b = socket.create_connection(("127.0.0.1", game))
        time.sleep(0.3)
        c = socket.create_connection(("127.0.0.1", game))
        assert closes_within(c, 3), "third connection from one IP must be refused"
        assert not closes_within(a, 0.5), "existing connections stay open"
        a.close()
        time.sleep(0.5)
        d = socket.create_connection(("127.0.0.1", game))
        assert not closes_within(d, 1.0), "slot freed after a close"
        for s in (b, d):
            s.close()
        assert "per-IP connection limit" in log_text(log)
    print("per-IP limit: PASS")


def pre_play_deadline_and_no_spin(binary):
    with server(binary, MC_SPINE_LOGIN_TIMEOUT_SECS=3) as (game, proc, log):
        silent = socket.create_connection(("127.0.0.1", game))
        t0 = time.monotonic()
        assert closes_within(silent, 6), "silent handshake socket must be dropped"
        assert time.monotonic() - t0 < 5.5
        # Login to the configuration phase, then vanish: the handler used to
        # spin at 100% CPU on the dead socket for up to 15 s.
        sock = raw_login_start(game, "Ghost")
        # Set Compression (0x03) precedes Login Success when the default
        # threshold is negotiated; the login ack then travels zlib-framed.
        threshold = -1
        packet = unframe_packet(recv_exact(sock, recv_vint(sock)), threshold)
        reader = Reader(packet)
        if reader.varint() == 0x03:
            threshold = reader.varint()
            packet = unframe_packet(recv_exact(sock, recv_vint(sock)), threshold)
            reader = Reader(packet)
        assert reader.varint() == 2, "login success"
        sock.sendall(frame_packet(vint(3), threshold))
        time.sleep(0.5)
        sock.shutdown(socket.SHUT_WR)  # EOF to the server
        before = cpu_seconds(proc.pid)
        time.sleep(2.0)
        used = cpu_seconds(proc.pid) - before
        sock.close()
        assert used < 0.5, f"server burned {used:.2f}s CPU on a dead config socket"
    print(f"pre-play deadline + config EOF (cpu {used:.2f}s over 2s): PASS")


def idle_timeout(binary):
    with server(binary, MC_SPINE_IDLE_TIMEOUT_SECS=3) as (game, _proc, log):
        with Client(game, "Sleeper") as c:
            c.login()
            c.socket.settimeout(0.2)
            t0 = time.monotonic()
            closed = False
            # stay silent: never answer keepalives or chunk batches
            while time.monotonic() - t0 < 15 and not closed:
                try:
                    closed = not c.socket.recv(65536)
                except socket.timeout:
                    pass
                except OSError:
                    closed = True
            assert closed, "silent play client must be dropped"
        text = log_text(log)
        assert "Sleeper disconnected" in text, text[-1500:]
    print("play idle timeout: PASS")


def chat_limits(binary):
    with server(binary) as (game, _proc, log):
        with Client(game, "Talker") as c:
            c.login()
            c.chat("hello")
            c.until(lambda _p, _b: any("hello" in m for m in c.messages), "chat-echo")
            c.chat("x" * 257)
            assert closes_within(c.socket, 15), "over-long chat must disconnect"
        with Client(game, "Spammer") as c:
            c.login()
            for _ in range(14):
                c.chat("spam")
            assert closes_within(c.socket, 15), "chat flood must disconnect"
        text = log_text(log)
        assert "Talker disconnected: illegal chat message" in text
        assert "Spammer disconnected: chat spam" in text
    print("chat length + spam: PASS")


def public_bind_guard(binary):
    with tempfile.TemporaryDirectory(prefix="spine-bind-") as tmp:
        env = dict(os.environ, MC_SPINE_PORT=str(port()), MC_SPINE_BIND_ADDR="0.0.0.0",
                   MC_SPINE_WG="old")
        env.pop("MC_SPINE_ALLOW_PUBLIC_BIND", None)
        env.pop("MC_SPINE_RCON_PORT", None)
        r = subprocess.run([str(binary)], cwd=tmp, env=env, capture_output=True, text=True, timeout=20)
        assert r.returncode == 2, (r.returncode, r.stderr[-500:])
        assert "MC_SPINE_ALLOW_PUBLIC_BIND" in r.stderr, r.stderr[-500:]
    # with the explicit opt-in it starts (and still answers on loopback)
    with server(binary, MC_SPINE_BIND_ADDR="0.0.0.0", MC_SPINE_ALLOW_PUBLIC_BIND=1) as (_g, _p, log):
        assert "non-loopback bind" in log_text(log)
    print("public bind guard: PASS")


def rcon_lockout(binary):
    rcon = port()
    with server(binary, MC_SPINE_RCON_PORT=rcon, MC_SPINE_RCON_PASS="right-secret",
                MC_SPINE_RCON_MAX_FAILURES=3, MC_SPINE_RCON_LOCKOUT_SECS=60) as (game, _p, log):
        time.sleep(0.5)
        for i in range(3):
            with socket.create_connection(("127.0.0.1", rcon), timeout=5) as s:
                s.settimeout(5)
                s.sendall(rcon_frame(i, 3, "wrong-secret-%d" % i))
                assert struct.unpack("<i", rcon_response(s)[:4])[0] == -1
        # now locked out: even the right password gets no answer
        with socket.create_connection(("127.0.0.1", rcon), timeout=5) as s:
            s.sendall(rcon_frame(9, 3, "right-secret"))
            assert closes_within(s, 3), "locked-out address must be dropped"
        text = log_text(log)
        assert "rcon authentication failed from 127.0.0.1 (failure #3)" in text, text[-1500:]
        assert "wrong-secret" not in text and "right-secret" not in text, "password leaked to log"
        assert "rcon lockout" in text
    # a correct login is audited, with the command
    rcon = port()
    with server(binary, MC_SPINE_RCON_PORT=rcon, MC_SPINE_RCON_PASS="right-secret") as (game, _p, log):
        time.sleep(0.5)
        with socket.create_connection(("127.0.0.1", rcon), timeout=5) as s:
            s.settimeout(5)
            s.sendall(rcon_frame(1, 3, "right-secret"))
            assert struct.unpack("<i", rcon_response(s)[:4])[0] == 1
            s.sendall(rcon_frame(2, 2, "list"))
            rcon_response(s)
        text = log_text(log)
        assert '[rcon] 127.0.0.1 authenticated' in text and '[rcon] 127.0.0.1 exec: "list"' in text, text[-1500:]
        assert "right-secret" not in text
    print("RCON lockout + audit log: PASS")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve()
    for scenario in (duplicate_login, allowlist, per_ip_limit, pre_play_deadline_and_no_spin,
                     idle_timeout, chat_limits, public_bind_guard, rcon_lockout):
        scenario(binary)
    print("network limits wire: PASS")
