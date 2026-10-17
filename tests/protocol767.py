"""Headless protocol-767 fixtures. No desktop, account credentials, or runtime deps."""
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time

from wire_smoke import frame, port, recv_exact, recv_vint, status, vint

ROOT = Path(__file__).resolve().parents[1]
MIN_Y = -64
WIRE_BLOCKS = json.loads((ROOT / "assets/blocks_1211.json").read_text())
STATE_COUNT = max(b["maxStateId"] for b in WIRE_BLOCKS) + 1
AIR_STATES = {st for b in WIRE_BLOCKS if b["name"] in ("air", "cave_air", "void_air")
              for st in range(b["minStateId"], b["maxStateId"] + 1)}


class ContractFailure(AssertionError):
    def __init__(self, code):
        self.code = code
        super().__init__(code)


def require(condition, code):
    if not condition:
        raise ContractFailure(code)


class Reader:
    def __init__(self, data):
        self.data, self.pos = data, 0

    def take(self, n):
        assert 0 <= n <= len(self.data) - self.pos, "truncated field"
        part = self.data[self.pos:self.pos + n]
        self.pos += n
        return part

    def number(self, fmt):
        return struct.unpack(fmt, self.take(struct.calcsize(fmt)))[0]

    def _var(self, limit, final_max):
        value = 0
        for i in range(limit):
            byte = self.number("B")
            if i == limit - 1:
                assert byte <= final_max, "variable integer overflow"
            value |= (byte & 127) << (7 * i)
            if not byte & 128:
                return value
        raise AssertionError("oversized variable integer")

    def varint(self):
        return self._var(5, 15)

    def varlong(self):
        return self._var(10, 1)

    def text(self):
        return self.take(self.number(">H")).decode("utf-8")

    def string(self):
        return self.take(self.varint()).decode("utf-8")

    def nbt(self, tag=None, depth=0):
        assert depth < 64, "NBT nesting limit"
        if tag is None:
            tag = self.number("B")
        if tag == 0:
            return None
        if tag in (1, 2, 3, 4, 5, 6):
            return self.number({1: "b", 2: ">h", 3: ">i", 4: ">q", 5: ">f", 6: ">d"}[tag])
        if tag == 8:
            return self.text()
        if tag == 10:
            result = {}
            while True:
                child = self.number("B")
                if child == 0:
                    return result
                name = self.text()
                assert name not in result, "duplicate NBT key"
                result[name] = self.nbt(child, depth + 1)
        if tag in (7, 9, 11, 12):
            child = self.number("B") if tag == 9 else {7: 1, 11: 3, 12: 4}[tag]
            length = self.number(">i")
            assert 0 <= length <= 1_048_576, "NBT array bound"
            return [self.nbt(child, depth + 1) for _ in range(length)]
        raise AssertionError(f"unknown NBT tag {tag}")


def palette(reader, count, indirect_max, minimum_bits, registry_count):
    bits = reader.number("B")
    assert bits == 0 or minimum_bits <= bits <= indirect_max or bits == (registry_count - 1).bit_length(), f"invalid palette width {bits}"
    values = [reader.varint()] if bits == 0 else (
        [reader.varint() for _ in range(reader.varint())] if bits <= indirect_max else None)
    length = reader.varint()
    per_word = 64 // bits if bits else count
    assert length == (math.ceil(count / per_word) if bits else 0), "palette long count"
    words = [reader.number(">Q") for _ in range(length)]
    if not bits:
        assert 0 <= values[0] < registry_count, "unknown singleton palette id"
        return values * count
    indices = [(words[i // per_word] >> ((i % per_word) * bits)) & ((1 << bits) - 1)
               for i in range(count)]
    if values is not None:
        assert all(i < len(values) for i in indices), "palette index out of range"
        indices = [values[i] for i in indices]
    assert all(0 <= i < registry_count for i in indices), "unknown registry id in palette"
    return indices


STRICT = True


def decode_chunk_light(body, biome_count):
    reader = Reader(body)
    coord = (reader.number(">i"), reader.number(">i"))
    heightmaps = reader.nbt()
    assert isinstance(heightmaps, dict) and "MOTION_BLOCKING" in heightmaps
    data = Reader(reader.take(reader.varint()))
    sections = []
    for _ in range(24):
        non_air = data.number(">h")
        assert 0 <= non_air <= 4096
        states = palette(data, 4096, 8, 4, STATE_COUNT)
        if STRICT:
            assert non_air == sum(st not in AIR_STATES for st in states), "non-air section count"
        sections.append(states)
        palette(data, 64, 3, 1, biome_count)
    assert data.pos == len(data.data), "unparsed chunk section bytes"
    for _ in range(reader.varint()):
        reader.take(3)  # packed local x/z and signed y
        reader.varint()  # block entity type
        reader.nbt()
    masks = []
    for _ in range(4):
        length = reader.varint()
        assert length <= 1, "light mask outside 26 sections"
        mask = reader.number(">Q") if length else 0
        assert mask < 1 << 26
        masks.append(mask)
    assert not masks[0] & masks[2] and not masks[1] & masks[3], "overlapping light masks"
    arrays = []
    for mask in masks[:2]:
        length = reader.varint()
        require(length == mask.bit_count(), "light-mask-count")
        rows = []
        for _ in range(length):
            assert reader.varint() == 2048, "light array nibble count"
            rows.append(reader.take(2048))
        arrays.append(rows)
    assert reader.pos == len(body), "unparsed chunk packet bytes"
    return coord, sections, masks, arrays[0], arrays[1]


def decode_chunk(body, biome_count):
    coord, sections, _, _, _ = decode_chunk_light(body, biome_count)
    return coord, sections


def settings(view):
    return vint(5) + b"en_us" + struct.pack("b", view) + bytes([0, 1, 127, 1, 0, 1])


def string(value):
    raw = value.encode("utf-8")
    return vint(len(raw)) + raw


def position(x, y, z):
    return struct.pack(">Q", ((x & 0x3ffffff) << 38) | ((z & 0x3ffffff) << 12) | (y & 0xfff))


def signed(value, bits):
    return value - (1 << bits) if value & (1 << (bits - 1)) else value


def unpack_position(data):
    packed = struct.unpack(">Q", data)[0]
    return signed(packed >> 38, 26), signed(packed & 4095, 12), signed((packed >> 12) & 0x3ffffff, 26)


@contextlib.contextmanager
def local_server(binary):
    with tempfile.TemporaryDirectory(prefix="spine-wire-") as tmp:
        root = Path(tmp)
        (root / "config_packets.bin").symlink_to(ROOT / "config_packets.bin")
        (root / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
        game = port()
        env = dict(os.environ, MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
                   MC_SPINE_WG="old", MC_SPINE_RESOURCE_PACK_URL="")
        env.pop("MC_SPINE_RCON_PORT", None)
        with (root / "server.log").open("w+") as log:
            proc = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=log)
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
                yield game
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


class Client:
    def __init__(self, game, name="WireTester"):
        self.socket = socket.create_connection(("127.0.0.1", game), timeout=10)
        self.game, self.name = game, name
        self.chunks, self.messages, self.updates = {}, [], []
        self.light_updates = []
        self.chunk_light = {}
        self.pos, self.contract_started = None, False
        self.phase, self.registry_counts = "login", {}

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.socket.close()

    def send(self, pid, body=b""):
        self.socket.sendall(frame(pid, body))

    def read(self, timeout):
        self.socket.settimeout(timeout)
        response = Reader(recv_exact(self.socket, recv_vint(self.socket)))
        pid = response.varint()
        body = response.take(len(response.data) - response.pos)
        if self.phase != "play":
            if self.phase == "configuration" and pid == 7:
                reader = Reader(body)
                name, length = reader.string(), reader.varint()
                assert length <= 65536, "registry entry bound"
                self.registry_counts[name] = length
                for _ in range(length):
                    reader.string()
                    if reader.number("B"):
                        reader.nbt()
                assert reader.pos == len(body), "unparsed registry data"
            return pid, body
        if pid == 0x27:
            coord, sections, masks, sky, block = decode_chunk_light(
                body, self.registry_counts["minecraft:worldgen/biome"])
            assert coord not in self.chunks, f"duplicate chunk {coord}"
            self.chunks[coord] = sections
            self.chunk_light[coord] = (masks, sky, block)
        elif pid == 0x21:
            z, x = struct.unpack(">ii", body)
            assert (x, z) in self.chunks, f"unload of unknown chunk {(x, z)}"
            del self.chunks[(x, z)]
        elif pid == 0x09:
            x, y, z = unpack_position(body[:8])
            st = Reader(body[8:]).varint()
            self._block_update(x, y, z, st)
        elif pid == 0x49:
            reader = Reader(body)
            section = reader.number(">Q")
            sx = signed(section >> 42, 22)
            sz = signed((section >> 20) & 0x3fffff, 22)
            sy = signed(section & 0xfffff, 20)
            for _ in range(reader.varint()):
                record = reader.varlong()
                x, y, z = (sx * 16 + ((record >> 8) & 15), sy * 16 + (record & 15),
                           sz * 16 + ((record >> 4) & 15))
                self._block_update(x, y, z, record >> 12)
            assert reader.pos == len(body)
        elif pid == 0x2a:
            reader = Reader(body)
            cx = signed(reader.varint(), 32)
            cz = signed(reader.varint(), 32)
            masks = []
            for _ in range(4):
                length = reader.varint()
                assert length <= 1, "light-update mask outside 26 sections"
                masks.append(reader.number(">Q") if length else 0)
            assert not masks[0] & masks[2] and not masks[1] & masks[3], "overlapping light-update masks"
            for mask in masks[:2]:
                length = reader.varint()
                require(length == mask.bit_count(), "light-update-mask-count")
                for _ in range(length):
                    assert reader.varint() == 2048, "light-update array nibble count"
                    reader.take(2048)
            assert reader.pos == len(body), "unparsed light update packet"
            self.light_updates.append((cx, cz))
        elif pid == 0x40:
            reader = Reader(body)
            coords = [reader.number(">d") for _ in range(3)]
            reader.take(8)  # yaw/pitch
            flags = reader.number("B")
            assert flags == 0, "fixture expects an absolute teleport"
            self.pos = tuple(coords)
            self.send(0, vint(reader.varint()))
        elif pid == 0x26:
            self.send(0x18, body)
        elif pid == 0x0c:
            self.send(0x08, struct.pack(">f", 30.0))
        elif pid == 0x6c:
            text = Reader(body).nbt()
            self.messages.append(text if isinstance(text, str) else text.get("text", ""))
        return pid, body

    def _block_update(self, x, y, z, state):
        assert MIN_Y <= y < 320
        self.updates.append(((x, y, z), state))
        chunk = self.chunks.get((x // 16, z // 16))
        if chunk is not None:
            chunk[(y - MIN_Y) // 16][((y & 15) << 8) | ((z & 15) << 4) | (x & 15)] = state

    def until(self, predicate, contract=None):
        deadline = time.monotonic() + 10
        try:
            for _ in range(2048):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("response deadline")
                pid, body = self.read(remaining)
                if predicate(pid, body):
                    return body
            raise TimeoutError("bounded packet receive exhausted")
        except TimeoutError:
            if contract:
                raise ContractFailure(contract) from None
            raise

    def until_id(self, pid, contract=None):
        return self.until(lambda actual, _: actual == pid, contract)

    def login(self, view=2, late=False):
        self.send(0, vint(767) + string("localhost") + struct.pack(">H", self.game) + vint(2))
        uid = bytearray(hashlib.md5(("OfflinePlayer:" + self.name).encode()).digest())
        uid[6] = (uid[6] & 15) | 48
        uid[8] = (uid[8] & 63) | 128
        self.send(0, string(self.name) + uid)
        assert self.read(10)[0] == 2, "login success"
        self.send(3)
        self.phase = "configuration"
        self.until_id(0x0e)
        info, known = frame(0, settings(view)), frame(7, vint(0))
        self.socket.sendall(known + info if late else info + known)
        self.contract_started = True
        self.until_id(3)
        self.send(3)
        self.phase = "play"
        self.until_id(0x0c)

    def move(self, x, y, z):
        self.send(0x1a, struct.pack(">ddd?", x, y, z, True))

    def block(self, x, y, z):
        return self.chunks[(x // 16, z // 16)][(y - MIN_Y) // 16][((y & 15) << 8) | ((z & 15) << 4) | (x & 15)]

    def chat(self, text):
        self.send(6, string(text) + struct.pack(">qq", int(time.time() * 1000), 0) + bytes(5))
