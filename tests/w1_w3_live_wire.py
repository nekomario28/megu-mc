#!/usr/bin/env python3
"""Live W1/W3 protocol-767 contracts against the real mc-spine binary.

W3 login admission (pre-seeded banned-players.json and whitelist.json), the
W1 P3-B server-ops command packets a level-4 operator receives (0x77/0x41 on
join, 0x60 SetPlayerTeam, 0x71 TickingState, 0x04 AwardStats), the W1 stdin
console surviving EOF, and the W1 --export-anvil/--import-anvil CLI.

Never connects to a live Minecraft service. Temporary working files,
disposable loopback ports, all child processes cleaned up.
"""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time

from protocol767 import ROOT, Client, ContractFailure, Reader, require, string, vint
from wire_smoke import frame, port, recv_exact, recv_vint, status

OP = "WireOp"
BANNED = "WireBanned"
ALLOWED = "WireAllowed"
OUTSIDER = "WireOutsider"


def offline_uuid(name):
    # Same derivation Client.login uses for its wire identity.
    uid = bytearray(hashlib.md5(("OfflinePlayer:" + name).encode()).digest())
    uid[6] = (uid[6] & 15) | 48
    uid[8] = (uid[8] & 63) | 128
    return bytes(uid)


def uuid_string(name):
    raw = offline_uuid(name).hex()
    return f"{raw[:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:]}"


def refused_login(game, name):
    """Drive handshake + login start; the only legal first answer to a refused
    login is the Login-state Disconnect packet (0x00)."""
    with socket.create_connection(("127.0.0.1", game), timeout=10) as sock:
        sock.settimeout(10)
        sock.sendall(frame(0, vint(767) + string("localhost")
                           + struct.pack(">H", game) + vint(2)))
        sock.sendall(frame(0, string(name) + offline_uuid(name)))
        response = Reader(recv_exact(sock, recv_vint(sock)))
        pid = response.varint()
        body = response.take(len(response.data) - response.pos)
        require(pid == 0x00, f"login-not-refused-{name}-{pid:#04x}")
        reader = Reader(body)
        reason = json.loads(reader.string()).get("text", "")
        require(reader.pos == len(reader.data), f"refusal-trailing-{name}")
        return reason


class LiveClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.play = {}

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play":
            self.play.setdefault(pid, []).append(body)
        return pid, body

    def await_packet(self, packet_id, index=0, contract=None):
        packets = self.play.setdefault(packet_id, [])
        if len(packets) <= index:
            self.until(lambda *_: len(packets) > index,
                       contract or f"play-packet-{packet_id:#04x}")
        return packets[index]

    def command(self, text, feedback, packet_id=None, index=0):
        before = len(self.messages)
        self.send(0x04, string(text))
        self.until(lambda *_: len(self.messages) > before, f"feedback-{text}")
        actual = self.messages[before]
        require(actual.startswith(feedback), f"feedback-{text}: {actual!r}")
        if packet_id is not None:
            return Reader(self.await_packet(packet_id, index, f"packet-{packet_id:#04x}-{text}"))


def decode_team(body):
    reader = Reader(body)
    name = reader.string()
    mode = reader.number("B")
    if mode in (0, 2):
        reader.nbt()        # display name component
        reader.number("B")  # friendly flags
        reader.string()     # nametag visibility
        reader.string()     # collision rule
        reader.varint()     # color
        reader.nbt()        # prefix
        reader.nbt()        # suffix
    entities = [reader.string() for _ in range(reader.varint())] \
        if mode in (0, 3, 4) else []
    require(reader.pos == len(reader.data), "team-packet-complete")
    return name, mode, entities


def decode_ticking_state(body):
    reader = Reader(body)
    rate, frozen = reader.number(">f"), reader.number("B") != 0
    require(reader.pos == len(reader.data), "ticking-state-complete")
    return rate, frozen


def decode_award_stats(body):
    reader = Reader(body)
    count = reader.varint()
    require(0 <= count <= 1_000_000, "award-stats-count")
    stats = [(reader.varint(), reader.varint(), reader.varint())
             for _ in range(count)]
    require(reader.pos == len(reader.data), "award-stats-complete")
    return stats


def decode_recipe_book(body):
    reader = Reader(body)
    require(reader.varint() == 0, "recipe-book-not-init")
    reader.take(8)  # four pages of (open, filtering) booleans
    unlocked = [reader.string() for _ in range(reader.varint())]
    highlight = [reader.string() for _ in range(reader.varint())]
    require(reader.pos == len(reader.data), "recipe-book-complete")
    return unlocked, highlight


def decode_disconnect_reason(body):
    reader = Reader(body)
    reason = json.loads(reader.string()).get("text", "")
    require(reader.pos == len(reader.data), "disconnect-complete")
    return reason


@contextlib.contextmanager
def live_server(binary, root, extra_env=None):
    """`local_server` plus caller env and a piped stdin: the console probe
    needs to deliver one command line and then EOF."""
    root = Path(root)
    for link, target in (("config_packets.bin", ROOT / "config_packets.bin"),
                         ("assets", ROOT / "assets")):
        if not (root / link).exists():
            (root / link).symlink_to(target, target_is_directory=target.is_dir())
    game = port()
    # The binary negotiates a 256-byte compression threshold unless the env says
    # otherwise; this suite's fixtures speak raw frames, so it opts out.
    env = dict(os.environ, MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
               MC_SPINE_WG="old", MC_SPINE_RESOURCE_PACK_URL="",
               MC_SPINE_COMPRESSION_THRESHOLD="-1")
    env.pop("MC_SPINE_RCON_PORT", None)
    env.setdefault("MC_SPINE_ANTICHEAT", "off")
    if extra_env:
        env.update(extra_env)
    with (root / "server.log").open("w+") as log:
        proc = subprocess.Popen([str(binary)], cwd=root, env=env,
                                stdin=subprocess.PIPE, stdout=log, stderr=log)
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
            yield game, proc, log
        except Exception:
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


def seed_admission_files(root):
    (root / "ops.json").write_text(json.dumps([{
        "uuid": uuid_string(OP), "name": OP,
        "level": 4, "bypassesPlayerLimit": False}]))
    (root / "banned-players.json").write_text(json.dumps([{
        "uuid": uuid_string(BANNED), "name": BANNED,
        "created": "2026-01-01T00:00:00Z", "source": "wire-test",
        "expires": "forever", "reason": "live wire ban"}]))
    (root / "whitelist.json").write_text(json.dumps([{
        "uuid": uuid_string(ALLOWED), "name": ALLOWED}]))


def admission_and_console(binary, root):
    seed_admission_files(root)
    with live_server(binary, root, {"MC_SPINE_WHITELIST": "1"}) as (game, proc, log):
        # W3: a banned name gets the Login Disconnect before Login Success.
        require("banned" in refused_login(game, BANNED), "ban-refusal-text")
        # W3: enforcement on, name absent from whitelist.json, not an operator.
        require("white-listed" in refused_login(game, OUTSIDER), "whitelist-refusal-text")
        # A whitelisted non-operator completes login and reaches Play.
        with LiveClient(game, ALLOWED) as allowed:
            allowed.login()
            allowed.await_packet(0x2B, contract="whitelist-join-game")

        with LiveClient(game, OP) as client:
            client.login()
            client.await_packet(0x2B, contract="op-join-game")
            require(Reader(client.await_packet(0x77, contract="update-recipes"))
                    .varint() > 0, "update-recipes-empty")
            decode_recipe_book(client.await_packet(0x41, contract="recipe-book-init"))

            reader = client.command("team add red", "Created team red", 0x60)
            require(decode_team(reader.data) == ("red", 0, []), "team-add-wire")
            ticking = len(client.play.get(0x71, ()))
            reader = client.command("tick freeze", "Froze the game", 0x71)
            require(decode_ticking_state(reader.data)[1] is True, "tick-freeze-wire")
            reader = client.command("tick unfreeze", "The game is no longer frozen",
                                    0x71, index=ticking + 1)
            require(decode_ticking_state(reader.data)[1] is False, "tick-unfreeze-wire")
            # C2S 0x09 ClientCommand action 1 (Request Stats) -> S2C 0x04.
            client.send(0x09, vint(1))
            decode_award_stats(client.await_packet(0x04, contract="award-stats"))

        # W1 stdin console: one line runs, then EOF must not crash or hang.
        # The child holds its own handle on server.log, so the poll re-reads by
        # path rather than through this side's buffered file object.
        proc.stdin.write(b"list\n")
        proc.stdin.flush()
        deadline = time.monotonic() + 5
        while "[cmd]" not in (root / "server.log").read_text():
            require(time.monotonic() < deadline, "console-list-no-output")
            require(proc.poll() is None, "console-command-crashed-server")
            time.sleep(0.05)
        proc.stdin.close()
        time.sleep(0.5)
        require(proc.poll() is None, "console-eof-killed-server")
        status(game)  # the listener still answers after stdin's EOF


def anvil_cli(binary, root):
    env = dict(os.environ, MC_SPINE_WG="old")
    out = root / "exported-world"
    export = subprocess.run([str(binary), "--export-anvil", str(out)],
                            cwd=root, env=env, capture_output=True, text=True,
                            timeout=60)
    require(export.returncode == 0,
            f"anvil-export-rc-{export.returncode}-{export.stderr[-300:]}")
    require((out / "level.dat").is_file(), "anvil-export-level-dat")
    reimport = subprocess.run([str(binary), "--import-anvil", str(out)],
                              cwd=root, env=env, capture_output=True, text=True,
                              timeout=60)
    require(reimport.returncode == 0,
            f"anvil-import-rc-{reimport.returncode}-{reimport.stderr[-300:]}")


def main(binary):
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    with tempfile.TemporaryDirectory(prefix="spine-w1w3-wire-") as directory:
        root = Path(directory)
        with tempfile.TemporaryDirectory(
                prefix="spine-w1w3-anvil-", dir=directory) as scratch:
            scratch = Path(scratch)
            (scratch / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
            anvil_cli(binary, scratch)
        admission_and_console(binary, root)


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else (
        ROOT / "target/debug/mc-spine")
    try:
        main(binary)
    except ContractFailure as error:
        print(f"w1/w3 live wire: FAIL {error.code}", file=sys.stderr)
        raise
    print("w1/w3 live wire: login admission, op command packets, stdin console, anvil CLI: PASS")
