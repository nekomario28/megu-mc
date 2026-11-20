"""Headless Protocol 767 differential player. Uses the repo's strict chunk decoder.

Only loopback, disposable worlds, an offline UUID, and ordinary client packets
are used. All gameplay actions are supplied by the same trace for either server.
"""

from collections import Counter
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid
import zlib

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests"))
from protocol767 import (Client, Reader, AIR_STATES, WIRE_BLOCKS, frame,  # noqa: E402
                         position, settings, string, vint)
from wire_smoke import port, rcon_frame, rcon_response, recv_exact, recv_vint  # noqa: E402

SEED = 4242
NAME = "ParityProbe"
STONE = next(b["defaultState"] for b in WIRE_BLOCKS if b["name"] == "stone")
WATER = next(b["defaultState"] for b in WIRE_BLOCKS if b["name"] == "water")
STONE_ITEM = 1  # Protocol 767 minecraft:stone item registry ID.
JAVA = Path("/usr/lib/jvm/java-21-openjdk/bin/java")
JAR = Path("/home/yuu/tmpx/carverfresh/server.jar")
JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
RCON_PASS = "local-client-diff-probe"


def offline_uuid(name):
    value = bytearray(hashlib.md5(("OfflinePlayer:" + name).encode()).digest())
    value[6] = (value[6] & 15) | 48
    value[8] = (value[8] & 63) | 128
    return bytes(value)


class PacketStream:
    """Translate optional Minecraft compression to ordinary framed packets."""

    def __init__(self, sock):
        self.sock = sock
        self.threshold = None
        self.buffer = bytearray()
        self.position_handler = None

    def settimeout(self, timeout):
        self.sock.settimeout(timeout)

    def close(self):
        self.sock.close()

    def recv(self, size):
        if not self.buffer:
            length = recv_vint(self.sock)
            if not 0 < length <= 16 * 1024 * 1024:
                raise ValueError(f"invalid packet frame length {length}")
            data = recv_exact(self.sock, length)
            if self.threshold is not None:
                reader = Reader(data)
                uncompressed = reader.varint()
                payload = reader.take(len(data) - reader.pos)
                if uncompressed:
                    if uncompressed < self.threshold or uncompressed > 16 * 1024 * 1024:
                        raise ValueError(f"invalid uncompressed packet length {uncompressed}")
                    decomp = zlib.decompressobj()
                    data = decomp.decompress(payload, uncompressed + 1)
                    if len(data) != uncompressed or not decomp.eof or decomp.unused_data:
                        raise ValueError("invalid compressed Minecraft frame")
                else:
                    if len(payload) >= self.threshold:
                        raise ValueError("uncompressed packet exceeds compression threshold")
                    data = payload
            if self.position_handler is not None:
                data = self.position_handler(data)
            self.buffer.extend(vint(len(data)) + data)
        part = self.buffer[:size]
        del self.buffer[:size]
        return bytes(part)

    def sendall(self, frames):
        reader = Reader(frames)
        while reader.pos < len(frames):
            packet = reader.take(reader.varint())
            if self.threshold is None:
                data = packet
            elif len(packet) >= self.threshold:
                data = vint(len(packet)) + zlib.compress(packet)
            else:
                data = vint(0) + packet
            self.sock.sendall(vint(len(data)) + data)


class ParityClient(Client):
    def __init__(self, game, name=NAME):
        super().__init__(game, name)
        self.socket = PacketStream(self.socket)
        self.packet_counts = Counter()
        self.phase_packets = {"login": [], "configuration": [], "play": []}
        self.position_syncs = []
        self.position_flags = []
        self.border_initializations = []
        self.advancement_payload_sizes = []
        self.advancement_payload_hashes = []
        self.entity_id = None
        self.unmatched_unloads = 0
        self.errors = []
        self.socket.position_handler = self._position_frame

    def _position_frame(self, packet):
        """Resolve 1.21.1 relative teleport flags for the strict base decoder."""
        if self.phase != "play" or not packet or packet[0] != 0x40:
            return packet
        if len(packet) < 35:
            raise ValueError("short player teleport packet")
        flags = packet[33]
        self.position_flags.append(flags)
        if flags & 0x07:
            if self.pos is None:
                raise ValueError("relative position before initial absolute spawn")
            positions = list(struct.unpack_from(">ddd", packet, 1))
            for axis in range(3):
                if flags & (1 << axis):
                    positions[axis] += self.pos[axis]
            packet = packet[:1] + struct.pack(">ddd", *positions) + packet[25:]
        # Parent decoder only accepts zero flags, and does not use rotations.
        return packet[:33] + b"\0" + packet[34:]

    def read(self, timeout):
        phase = self.phase
        try:
            packet, body = super().read(timeout)
        except AssertionError as error:
            if str(error).startswith("unload of unknown chunk"):
                # Vanilla can cancel a pending chunk send while changing view.
                self.unmatched_unloads += 1
                return 0x21, b""
            raise
        self.packet_counts[packet] += 1
        if len(self.phase_packets[phase]) < 200:
            self.phase_packets[phase].append(packet)
        if phase == "configuration" and packet == 0x04:
            self.send(0x04, body)  # Configuration Keep Alive.
        if phase == "play" and packet == 0x2b:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        if phase == "play" and packet == 0x40:
            self.position_syncs.append(self.pos)
        if phase == "play" and packet == 0x25:
            self.border_initializations.append(body)
        if phase == "play" and packet == 0x74:
            self.advancement_payload_sizes.append(len(body))
            self.advancement_payload_hashes.append(hashlib.sha256(body).hexdigest())
        return packet, body

    def login(self, view=2):
        self.send(0, vint(767) + string("localhost") + struct.pack(">H", self.game) + vint(2))
        self.send(0, string(self.name) + offline_uuid(self.name))
        for _ in range(12):
            pid, body = self.read(15)
            if pid == 0x03:
                if self.socket.threshold is not None:
                    raise AssertionError("duplicate compression setup")
                self.socket.threshold = Reader(body).varint()
            elif pid == 0x02:
                break
            else:
                raise AssertionError(f"unexpected login packet 0x{pid:02x}: {body[:80]!r}")
        else:
            raise AssertionError("login success absent")
        self.send(0x03)
        self.phase = "configuration"
        self.until_id(0x0e, "configuration-known-packs")
        self.socket.sendall(frame(0x00, settings(view)) + frame(0x07, vint(0)))
        self.until_id(0x03, "configuration-finish")
        self.send(0x03)
        self.phase = "play"


def read_for(client, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            client.read(min(0.12, max(0.001, deadline - time.monotonic())))
        except socket.timeout:
            continue


def until(client, condition, label, timeout=12):
    deadline = time.monotonic() + timeout
    while not condition():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"{label}: deadline ({timeout}s)")
        try:
            client.read(min(0.5, remaining))
        except socket.timeout:
            continue


def command(rcon_port, text):
    """Set up the shared test fixture through the independent admin channel."""
    with socket.create_connection(("127.0.0.1", rcon_port), timeout=5) as sock:
        sock.settimeout(5)
        sock.sendall(rcon_frame(1, 3, RCON_PASS))
        auth = rcon_response(sock)
        if struct.unpack("<i", auth[:4])[0] != 1:
            raise AssertionError("fixture RCON authentication rejected")
        sock.sendall(rcon_frame(2, 2, text))
        response = rcon_response(sock)
        if struct.unpack("<i", response[:4])[0] != 2:
            raise AssertionError(f"fixture RCON command rejected: {text}")
        output = response[8:-2].decode("utf-8", "replace")
        if "unknown command" in output.lower() or "incorrect argument" in output.lower():
            raise AssertionError(f"fixture command {text!r} failed: {output}")
        return output


def server_position(rcon_port):
    """Read authoritative player coordinates through a common RCON command."""
    output = command(rcon_port, f"data get entity {NAME} Pos")
    match = re.search(r"\[([^\]]+)\]", output)
    if match is None:
        raise AssertionError(f"cannot parse authoritative position: {output!r}")
    numbers = match.group(1).split(",")
    if len(numbers) != 3:
        raise AssertionError(f"invalid RCON position: {output!r}")
    return tuple(float(value.strip().removesuffix("d")) for value in numbers)


def border_details(data):
    if not data:
        return None
    reader = Reader(data[0])
    center = (reader.number(">d"), reader.number(">d"))
    old_size, new_size = reader.number(">d"), reader.number(">d")
    lerp_time = reader.varlong()
    max_size = reader.varint()
    warning_blocks, warning_time = reader.varint(), reader.varint()
    if reader.pos != len(reader.data):
        raise ValueError("trailing initial border fields")
    return {"center": center, "old_size": old_size, "new_size": new_size,
            "lerp_time": lerp_time, "max_size": max_size,
            "warning_blocks": warning_blocks, "warning_time": warning_time}


def cell(client, x, y, z):
    return client.block(x, y, z)


def block_wait(client, x, y, z, expected):
    until(client, lambda: (x // 16, z // 16) in client.chunks
          and cell(client, x, y, z) == expected,
          f"block ({x},{y},{z}) expected state {expected}")


def move_trace(client, steps, rcon_port):
    start = len(client.position_syncs)
    accepted, offsets, actual = 0, [], []
    for x, y, z, grounded in steps:
        client.send(0x1a, struct.pack(">ddd?", x, y, z, grounded))
        read_for(client, 0.055)
        authoritative = server_position(rcon_port)
        error = max(abs(a - b) for a, b in zip(authoritative, (x, y, z)))
        actual.append(authoritative)
        offsets.append(round(error, 8))
        if error <= 1e-5:
            accepted += 1
    read_for(client, 0.28)
    return {"sent": len(steps), "accepted": accepted,
            "max_server_offset": max(offsets, default=0),
            "server_positions": actual,
            "corrections_0x40": len(client.position_syncs) - start}


@contextmanager
def live_server(kind, binary, root, jar=JAR):
    """Bind one isolated test process to loopback, with per-run disposable data."""
    game = port()
    rcon_port = port()
    while rcon_port == game:
        rcon_port = port()
    env = dict(os.environ, TMPDIR=str(ROOT / "target/tmp"))
    if kind == "mc-spine":
        for link, source in (("config_packets.bin", ROOT / "config_packets.bin"),
                             ("assets", ROOT / "assets")):
            (root / link).symlink_to(source, target_is_directory=source.is_dir())
        env.update(MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
                   MC_SPINE_SEED=str(SEED), MC_SPINE_WG="old",
                   MC_SPINE_RESOURCE_PACK_URL="", MC_SPINE_RCON_PORT=str(rcon_port),
                   MC_SPINE_RCON_PASS=RCON_PASS, MC_SPINE_RCON_BIND_ADDR="127.0.0.1")
        argv = [str(binary)]
    else:
        if hashlib.sha1(jar.read_bytes()).hexdigest() != JAR_SHA1:
            raise AssertionError("pinned vanilla server.jar SHA-1 mismatch")
        if not JAVA.is_file():
            raise FileNotFoundError(JAVA)
        (root / "eula.txt").write_text("eula=true\n")
        options = {
            "server-port": game, "server-ip": "127.0.0.1", "online-mode": "false",
            "level-seed": SEED, "level-type": "minecraft:flat", "gamemode": "creative",
            "force-gamemode": "true", "spawn-protection": 0, "difficulty": "peaceful",
            "view-distance": 2, "simulation-distance": 2,
            "network-compression-threshold": 256, "enable-command-block": "false",
            "enable-rcon": "true", "rcon.port": rcon_port, "rcon.password": RCON_PASS,
        }
        (root / "server.properties").write_text("".join(f"{key}={value}\n" for key, value in options.items()))
        (root / "ops.json").write_text(json.dumps([{
            "uuid": str(uuid.UUID(bytes=offline_uuid(NAME))), "name": NAME,
            "level": 4, "bypassesPlayerLimit": True,
        }]))
        argv = [str(JAVA), f"-Djava.io.tmpdir={ROOT / 'target/tmp'}",
                "-XX:-UsePerfData", "-Xms256M", "-Xmx1300M", "-jar", str(jar), "nogui"]
    with (root / "server.log").open("w+") as log:
        proc = subprocess.Popen(argv, cwd=root, env=env, stdout=log, stderr=log)
        try:
            for _ in range(240):
                if proc.poll() is not None:
                    raise RuntimeError(f"{kind} exited {proc.returncode} before listener")
                try:
                    with socket.create_connection(("127.0.0.1", game), timeout=0.25):
                        break
                except OSError:
                    time.sleep(0.25)
            else:
                raise TimeoutError(f"{kind} listener not ready within 60s")
            yield game, rcon_port
        except Exception:
            log.flush()
            log.seek(0)
            print(f"{kind} server log tail:\n{log.read()[-2500:]}", file=sys.stderr)
            raise
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=8)


def exercise(game, rcon_port):
    """Execute one fixed trace. Every scenario exposes the actual observed packets."""
    result = {}
    with ParityClient(game) as client:
        client.login()
        until(client, lambda: client.pos is not None and (0, 0) in client.chunks,
              "spawn and origin chunk", timeout=30)
        read_for(client, 0.6)
        result["bootstrap"] = {
            "login_success": 0x02 in client.phase_packets["login"],
            "configuration_finish": 0x03 in client.phase_packets["configuration"],
            "join_game_0x2b": client.entity_id is not None,
            "border_0x25": 0x25 in client.phase_packets["play"],
            "border_details": border_details(client.border_initializations),
            "advancements_0x74": 0x74 in client.phase_packets["play"],
            "advancement_payload_bytes": client.advancement_payload_sizes[:],
            "advancement_payload_sha256": client.advancement_payload_hashes[:],
            "initial_position_0x40": bool(client.position_syncs),
            "position_flags_raw": client.position_flags[:],
            "compression_threshold": client.socket.threshold,
            "initial_position": client.pos,
            "login_packet_ids": [f"0x{pid:02x}" for pid in client.phase_packets["login"]],
            "configuration_packet_ids": [f"0x{pid:02x}" for pid in client.phase_packets["configuration"]],
        }
        result["chunks"] = {
            "decoded_0x27": len(client.raw_chunk) > 0,
            "chunks_decoded": len(client.raw_chunk),
            "chunks_active": len(client.chunks),
            "unmatched_unloads": client.unmatched_unloads,
            "origin_chunk": (0, 0) in client.chunks,
            "sections": client.section_count,
            "states_per_section": 4096,
        }

        command(rcon_port, "difficulty peaceful")
        command(rcon_port, f"gamemode creative {NAME}")
        command(rcon_port, "fill 0 239 0 11 246 4 minecraft:air")
        command(rcon_port, "fill 0 239 0 11 239 4 minecraft:stone")
        block_wait(client, 5, 239, 1, STONE)
        block_wait(client, 5, 240, 1, 0)
        command(rcon_port, f"tp {NAME} 1.5 240 1.5")
        until(client, lambda: client.pos == (1.5, 240.0, 1.5), "arena teleport")
        read_for(client, 0.15)
        walk = [(round(1.5 + 0.24 * i, 5), 240.0, 1.5, True)
                for i in range(1, 7)]
        result["grounded_walk"] = move_trace(client, walk, rcon_port)

        client.send(0x25, vint(client.entity_id) + vint(3) + vint(0))
        sprint = [(round(walk[-1][0] + 0.30 * i, 5), 240.0, 1.5, True)
                  for i in range(1, 7)]
        result["sprint"] = move_trace(client, sprint, rcon_port)
        client.send(0x25, vint(client.entity_id) + vint(4) + vint(0))

        jump = []
        y, velocity = 240.0, 0.42
        for _ in range(12):
            y = max(240.0, y + velocity)
            grounded = y == 240.0
            jump.append((sprint[-1][0], y, 1.5, grounded))
            velocity = 0.0 if grounded else (velocity - 0.08) * 0.98
        result["jump_arc"] = move_trace(client, jump, rcon_port)
        result["jump_arc"]["apex_y"] = round(max(row[1] for row in jump), 6)

        client.send(0x32, struct.pack(">h", 36) + vint(1) + vint(STONE_ITEM)
                    + vint(0) + vint(0))
        read_for(client, 0.15)
        target = (6, 240, 1)
        since = len(client.updates)
        client.send(0x38, vint(0) + position(6, 239, 1) + vint(1)
                    + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(81))
        block_wait(client, *target, STONE)
        placed_updates = client.updates[since:]
        result["block_place"] = {
            "state": cell(client, *target),
            "packet_0x09": (target, STONE) in placed_updates,
            "updates": [(list(pos), state) for pos, state in placed_updates if pos == target],
        }
        since = len(client.updates)
        client.send(0x24, vint(0) + position(*target) + bytes([1]) + vint(82))
        block_wait(client, *target, 0)
        broken_updates = client.updates[since:]
        result["block_break"] = {
            "state": cell(client, *target),
            "packet_0x09": (target, 0) in broken_updates,
            "updates": [(list(pos), state) for pos, state in broken_updates if pos == target],
        }

        command(rcon_port, "fill 7 240 0 11 245 4 minecraft:stone")
        command(rcon_port, "fill 8 241 1 10 244 3 minecraft:water")
        block_wait(client, 9, 243, 2, WATER)
        command(rcon_port, f"tp {NAME} 9.5 245.2 2.5")
        until(client, lambda: client.pos == (9.5, 245.2, 2.5), "fluid entry teleport")
        read_for(client, 0.12)
        result["fluid_entry"] = move_trace(client, [
            (9.5, 245.1, 2.5, False), (9.5, 244.95, 2.5, False),
            (9.5, 244.8, 2.5, False),
        ], rcon_port)
        result["fluid_entry"]["water_state"] = cell(client, 9, 244, 2)
        command(rcon_port, f"tp {NAME} 9.5 243.5 2.5")
        until(client, lambda: client.pos == (9.5, 243.5, 2.5), "submerged teleport")
        read_for(client, 0.12)
        water_steps = []
        y, speed = 243.5, 0.0
        for _ in range(8):
            y += speed
            speed = speed * 0.800000011920929 - 0.005
            water_steps.append((9.5, y, 2.5, False))
        result["submerged_ticks"] = move_trace(client, water_steps, rcon_port)
        result["submerged_ticks"].update({
            "water_state": cell(client, 9, 243, 2),
            "first_y": water_steps[0][1],
            "last_y": water_steps[-1][1],
            "tick_tolerance": 0.03,
        })
    return result


def run_target(kind, binary, jar=JAR):
    tmp = ROOT / "target/tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f"client-diff-{kind}-", dir=tmp) as folder:
        with live_server(kind, binary, Path(folder), jar) as (game, rcon_port):
            return exercise(game, rcon_port)
