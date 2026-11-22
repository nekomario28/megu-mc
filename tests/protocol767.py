"""Headless protocol-767 fixtures. No desktop, account credentials, or runtime deps.

Run directly (`python3 tests/protocol767.py [binary]`) for the 0x03 compression
and live wire parity suite: one isolated server per framing mode, the full
login/configuration/play login each time, and identical delivered payloads."""
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
import zlib

from wire_smoke import frame, port, recv_exact, recv_vint, status, vint

ROOT = Path(__file__).resolve().parents[1]
MIN_Y = -64
DEFAULT_HEIGHT = 384
DEFAULT_SECTIONS = DEFAULT_HEIGHT // 16
DEFAULT_LIGHT_SECTIONS = DEFAULT_SECTIONS + 2
BUILTIN_DIMENSION_TYPES = {
    "minecraft:overworld": (-64, 384, True),
    "minecraft:overworld_caves": (-64, 384, True),
    "minecraft:the_nether": (0, 256, False),
    "minecraft:the_end": (0, 256, False),
}
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


def frame_packet(raw, threshold=-1):
    """Length-prefix one packet (`raw` = VarInt packet id + body) for the wire.

    Vanilla 767 compression framing: once Set Compression has negotiated a
    non-negative threshold, every frame is `VarInt(frame_len)` over
    `VarInt(data_len) + payload`, where `data_len` is 0 for an uncompressed
    packet shorter than the threshold and the uncompressed length followed by
    the zlib stream otherwise."""
    if threshold >= 0:
        inner = (vint(0) + raw) if len(raw) < threshold else (
            vint(len(raw)) + zlib.compress(raw))
    else:
        inner = raw
    return vint(len(inner)) + inner


def unframe_packet(frame_data, threshold=-1):
    """Inverse of `frame_packet` for one received frame (length prefix removed)."""
    if threshold < 0:
        return frame_data
    reader = Reader(frame_data)
    data_len = reader.varint()
    rest = reader.take(len(reader.data) - reader.pos)
    if data_len == 0:
        assert len(rest) < threshold, "uncompressed packet meets compression threshold"
        return rest
    assert threshold <= data_len <= 16 * 1024 * 1024, "invalid compressed data length"
    try:
        packet = zlib.decompress(rest)
    except zlib.error as exc:
        raise AssertionError(f"invalid zlib stream: {exc}") from exc
    assert len(packet) == data_len, "decompressed length mismatch"
    return packet


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


def decode_chunk_light(body, biome_count, section_count=DEFAULT_SECTIONS, has_skylight=True):
    assert section_count > 0, "invalid section count"
    light_section_count = section_count + 2
    max_mask_longs = (light_section_count + 63) // 64
    reader = Reader(body)
    coord = (reader.number(">i"), reader.number(">i"))
    heightmaps = reader.nbt()
    assert isinstance(heightmaps, dict) and "MOTION_BLOCKING" in heightmaps
    data = Reader(reader.take(reader.varint()))
    sections = []
    if STRICT:
        for _ in range(section_count):
            non_air = data.number(">h")
            assert 0 <= non_air <= 4096
            states = palette(data, 4096, 8, 4, STATE_COUNT)
            if STRICT:
                assert non_air == sum(st not in AIR_STATES for st in states), "non-air section count"
            sections.append(states)
            palette(data, 64, 3, 1, biome_count)
        assert data.pos == len(data.data), "unparsed chunk section bytes"
    else:
        # A live server's palette ids need not match the fixture's, and a probe reads the light
        # below, so the section contents are skipped rather than asserted.
        sections = None
    for _ in range(reader.varint()):
        reader.take(3)  # packed local x/z and signed y
        reader.varint()  # block entity type
        reader.nbt()
    masks = []
    for _ in range(4):
        length = reader.varint()
        assert length <= max_mask_longs, f"light mask outside {light_section_count} sections"
        mask = 0
        for word_idx in range(length):
            mask |= reader.number(">Q") << (64 * word_idx)
        assert mask < (1 << light_section_count)
        masks.append(mask)
    assert not masks[0] & masks[2] and not masks[1] & masks[3], "overlapping light masks"
    if not has_skylight:
        assert masks[0] == 0, "unexpected sky light mask when has_skylight is False"
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


def decode_chunk(body, biome_count, section_count=DEFAULT_SECTIONS, has_skylight=True):
    coord, sections, _, _, _ = decode_chunk_light(
        body, biome_count, section_count=section_count, has_skylight=has_skylight)
    return coord, sections


def parse_spawn_info(reader):
    dimension_type = reader.varint()
    dimension_name = reader.string()
    reader.number(">q")  # hashed_seed
    reader.number("B")   # game_mode
    reader.number("b")   # previous_game_mode
    reader.number("B")   # is_debug
    reader.number("B")   # is_flat
    if reader.number("B"):  # has_death_location
        reader.string()     # death_dimension_name
        reader.take(8)      # death_location
    reader.varint()         # portal_cooldown
    return dimension_type, dimension_name


def parse_join_game(body):
    reader = Reader(body)
    reader.number(">i")  # entity_id
    reader.number("B")   # is_hardcore
    dim_count = reader.varint()
    assert dim_count <= 65536, "dimension list bound"
    for _ in range(dim_count):
        reader.string()
    reader.varint()      # max_players
    reader.varint()      # view_distance
    reader.varint()      # simulation_distance
    reader.number("B")   # reduced_debug_info
    reader.number("B")   # enable_respawn_screen
    reader.number("B")   # do_limited_crafting
    dim_type, dim_name = parse_spawn_info(reader)
    reader.number("B")   # enforces_secure_chat
    assert reader.pos == len(body), "unparsed join game packet"
    return dim_type, dim_name


def parse_respawn(body):
    reader = Reader(body)
    dim_type, dim_name = parse_spawn_info(reader)
    reader.number("B")   # data_kept
    assert reader.pos == len(body), "unparsed respawn packet"
    return dim_type, dim_name


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
def local_server(binary, root=None, extra_env=None):
    """Run the server on a loopback port. `root` keeps the world: a caller that hands
    in the same directory twice is a restart, which is the only way to observe what a
    save actually persisted. `extra_env` overrides the spawned environment; a value
    of None deletes the variable so the server's own default is what runs."""
    if root is None:
        with tempfile.TemporaryDirectory(prefix="spine-wire-") as tmp:
            with local_server(binary, Path(tmp), extra_env=extra_env) as game:
                yield game
        return
    root = Path(root)
    for link, target in (("config_packets.bin", ROOT / "config_packets.bin"),
                         ("assets", ROOT / "assets")):
        if not (root / link).exists():
            (root / link).symlink_to(target, target_is_directory=target.is_dir())
    game = port()
    env = dict(os.environ, MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
               MEGU_WG="old", MEGU_RESOURCE_PACK_URL="")
    env.pop("MEGU_RCON_PORT", None)
    # These clients warp across chunks in single packets to exercise the protocol;
    # the anti-cheat has its own live tests (cargo test anticheat_live).
    env.setdefault("MEGU_ANTICHEAT", "off")
    for key, value in (extra_env or {}).items():
        if value is None:
            env.pop(key, None)
        else:
            env[key] = value
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
    min_y = MIN_Y
    height = DEFAULT_HEIGHT
    max_y = MIN_Y + DEFAULT_HEIGHT
    section_count = DEFAULT_SECTIONS
    light_section_count = DEFAULT_LIGHT_SECTIONS
    has_skylight = True
    dimension_name = "minecraft:overworld"

    def __init__(self, game, name="WireTester", threshold=-1):
        self.socket = socket.create_connection(("127.0.0.1", game), timeout=10)
        self.compression_threshold = threshold
        self.game, self.name = game, name
        self.chunks, self.messages, self.updates = {}, [], []
        # how many Chunk Batch Finished frames arrived, and the batch sizes they
        # declared: the delivery is paced by this client's acknowledgements
        self.chunk_batches = []
        self.light_updates = []
        self.chunk_light = {}
        self.raw_chunk = {}
        # how many received frames carried a zlib body vs. a plain data_len==0
        # body while compression framing was negotiated (probes, not decoding)
        self.compressed_frames = 0
        self.uncompressed_frames = 0
        self.pos, self.contract_started = None, False
        self.phase, self.registry_counts = "login", {}
        self.dimension_types_by_id = {}
        self.dimension_types_by_name = {}
        self._set_dimension(MIN_Y, DEFAULT_HEIGHT, True, "minecraft:overworld")

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.socket.close()

    def _compression(self):
        return getattr(self, "compression_threshold", -1)

    def send(self, pid, body=b""):
        self.socket.sendall(frame_packet(vint(pid) + body, self._compression()))

    def _set_dimension(self, min_y, height, has_skylight, dimension_name, reset_chunks=False):
        assert height > 0 and height % 16 == 0, f"invalid dimension height {height}"
        assert min_y % 16 == 0, f"invalid dimension min_y {min_y}"
        section_count = height // 16
        light_section_count = section_count + 2
        old_dim = (self.min_y, self.height, self.has_skylight, self.dimension_name)
        self.min_y = min_y
        self.height = height
        self.max_y = min_y + height
        self.section_count = section_count
        self.light_section_count = light_section_count
        self.has_skylight = bool(has_skylight)
        self.dimension_name = dimension_name
        new_dim = (self.min_y, self.height, self.has_skylight, self.dimension_name)
        if reset_chunks and old_dim != new_dim:
            for attr in ("chunks", "raw_chunk", "chunk_light"):
                if hasattr(self, attr):
                    getattr(self, attr).clear()

    def _register_dimension_type(self, idx, entry_name, entry_nbt):
        if not hasattr(self, "dimension_types_by_id"):
            self.dimension_types_by_id = {}
        if not hasattr(self, "dimension_types_by_name"):
            self.dimension_types_by_name = {}
        meta = None
        if isinstance(entry_nbt, dict) and {"min_y", "height", "has_skylight"} <= entry_nbt.keys():
            meta = (int(entry_nbt["min_y"]), int(entry_nbt["height"]), bool(entry_nbt["has_skylight"]))
        elif entry_name in BUILTIN_DIMENSION_TYPES:
            meta = BUILTIN_DIMENSION_TYPES[entry_name]
        if meta is not None:
            min_y, height, _ = meta
            assert height > 0 and height % 16 == 0, f"invalid dimension_type height {height}"
            assert min_y % 16 == 0, f"invalid dimension_type min_y {min_y}"
            self.dimension_types_by_id[idx] = meta
            self.dimension_types_by_name[entry_name] = meta

    def _apply_spawn_dimension(self, dim_type_id, dim_name, reset_chunks=False):
        by_id = getattr(self, "dimension_types_by_id", {})
        by_name = getattr(self, "dimension_types_by_name", {})
        if by_id:
            assert dim_type_id in by_id, f"unknown dimension_type id {dim_type_id}"
            meta = by_id[dim_type_id]
        elif dim_name in by_name:
            meta = by_name[dim_name]
        elif dim_name in BUILTIN_DIMENSION_TYPES:
            meta = BUILTIN_DIMENSION_TYPES[dim_name]
        else:
            meta = (MIN_Y, DEFAULT_HEIGHT, True)
        self._set_dimension(*meta, dim_name, reset_chunks=reset_chunks)

    def read(self, timeout):
        self.socket.settimeout(timeout)
        frame_data = recv_exact(self.socket, recv_vint(self.socket))
        if self._compression() >= 0:
            if Reader(frame_data).varint():
                self.compressed_frames = getattr(self, "compressed_frames", 0) + 1
            else:
                self.uncompressed_frames = getattr(self, "uncompressed_frames", 0) + 1
        response = Reader(unframe_packet(frame_data, self._compression()))
        pid = response.varint()
        body = response.take(len(response.data) - response.pos)
        if self.phase != "play":
            if self.phase == "configuration" and pid == 7:
                reader = Reader(body)
                name, length = reader.string(), reader.varint()
                assert length <= 65536, "registry entry bound"
                if not hasattr(self, "registry_counts"):
                    self.registry_counts = {}
                self.registry_counts[name] = length
                if name == "minecraft:dimension_type":
                    self.dimension_types_by_id = {}
                    self.dimension_types_by_name = {}
                for idx in range(length):
                    entry_name = reader.string()
                    entry_nbt = reader.nbt() if reader.number("B") else None
                    if name == "minecraft:dimension_type":
                        self._register_dimension_type(idx, entry_name, entry_nbt)
                assert reader.pos == len(body), "unparsed registry data"
            return pid, body
        if pid == 0x2b:
            dim_type, dim_name = parse_join_game(body)
            self._apply_spawn_dimension(dim_type, dim_name, reset_chunks=False)
        elif pid == 0x47:
            dim_type, dim_name = parse_respawn(body)
            self._apply_spawn_dimension(dim_type, dim_name, reset_chunks=True)
        elif pid == 0x27:
            biome_count = self.registry_counts["minecraft:worldgen/biome"]
            if self.section_count == DEFAULT_SECTIONS and self.has_skylight:
                coord, sections, masks, sky, block = decode_chunk_light(body, biome_count)
            else:
                coord, sections, masks, sky, block = decode_chunk_light(
                    body, biome_count, section_count=self.section_count,
                    has_skylight=self.has_skylight)
            # The server re-sends a held chunk when the occlusion lane flips;
            # a resent 0x27 replaces the client's copy rather than erroring.
            self.chunks[coord] = sections
            self.raw_chunk[coord] = body
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
            light_sections = self.light_section_count
            max_mask_longs = (light_sections + 63) // 64
            masks = []
            for _ in range(4):
                length = reader.varint()
                assert length <= max_mask_longs, f"light-update mask outside {light_sections} sections"
                mask = 0
                for word_idx in range(length):
                    mask |= reader.number(">Q") << (64 * word_idx)
                assert mask < (1 << light_sections), f"light-update mask outside {light_sections} sections"
                masks.append(mask)
            assert not masks[0] & masks[2] and not masks[1] & masks[3], "overlapping light-update masks"
            if not self.has_skylight:
                assert masks[0] == 0, "unexpected sky light update when has_skylight is False"
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
            self.chunk_batches.append(Reader(body).varint())
            self.send(0x08, struct.pack(">f", 30.0))
        elif pid == 0x6c:
            text = Reader(body).nbt()
            self.messages.append(text if isinstance(text, str) else text.get("text", ""))
        return pid, body

    def _block_update(self, x, y, z, state):
        assert self.min_y <= y < self.max_y
        self.updates.append(((x, y, z), state))
        chunk = self.chunks.get((x // 16, z // 16))
        if chunk is not None:
            chunk[(y - self.min_y) // 16][((y & 15) << 8) | ((z & 15) << 4) | (x & 15)] = state

    def until(self, predicate, contract=None):
        # Probes over slower worldgen can raise the bound; tests keep 10s.
        deadline = time.monotonic() + float(os.environ.get("MC_PROBE_DEADLINE", "10"))
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

    def until_chunks(self, expected, contract=None):
        """Read until the decoded chunk set is exactly `expected`.

        Delivery is paced: the reader above answers every Chunk Batch Finished
        with the client's requested chunks/tick, and the server sends the next
        batch on that answer. "The whole window is here" is therefore a state to
        reach, not a single packet to wait for.
        """
        expected = set(expected)
        self.until(lambda *_: set(self.chunks) == expected, contract)
        return set(self.chunks)

    def login(self, view=2, late=False):
        self.send(0, vint(767) + string("localhost") + struct.pack(">H", self.game) + vint(2))
        uid = bytearray(hashlib.md5(("OfflinePlayer:" + self.name).encode()).digest())
        uid[6] = (uid[6] & 15) | 48
        uid[8] = (uid[8] & 63) | 128
        self.send(0, string(self.name) + uid)
        pid, body = self.read(10)
        if pid == 0x03:
            # Set Compression: every frame after this one is zlib-framed.
            reader = Reader(body)
            threshold = reader.varint()
            assert 0 <= threshold and reader.pos == len(body), "invalid set compression"
            self.compression_threshold = threshold
            pid, body = self.read(10)
        assert pid == 2, "login success"
        self.send(3)
        self.phase = "configuration"
        self.until_id(0x0e)
        info = frame_packet(vint(0) + settings(view), self._compression())
        known = frame_packet(vint(7) + vint(0), self._compression())
        self.socket.sendall(known + info if late else info + known)
        self.contract_started = True
        self.until_id(3)
        self.send(3)
        self.phase = "play"
        self.until_id(0x0c)

    def move(self, x, y, z):
        self.send(0x1a, struct.pack(">ddd?", x, y, z, True))

    def block(self, x, y, z):
        assert self.min_y <= y < self.max_y
        return self.chunks[(x // 16, z // 16)][(y - self.min_y) // 16][((y & 15) << 8) | ((z & 15) << 4) | (x & 15)]

    def chat(self, text):
        self.send(6, string(text) + struct.pack(">qq", int(time.time() * 1000), 0) + bytes(5))


def spawn_window(radius=2):
    """Chunk coords a fresh login at the default view must end up holding."""
    return {(x, z) for x in range(-radius, radius + 1)
            for z in range(-radius, radius + 1)}


def wire_session(binary, extra_env):
    """One isolated server (temp dir, ephemeral port, killed on exit) driven
    through login and the paced spawn-window delivery. Returns what the wire
    actually delivered so two framing modes can be compared packet for packet."""
    observed = {}
    with local_server(binary, extra_env=extra_env) as game, Client(game) as client:
        client.login()
        client.until_chunks(spawn_window(), "spawn-window")
        if client.pos is None:
            client.until_id(0x40, "spawn-teleport")
        require(client.chunk_batches, "chunk-batch-finished")
        observed.update(
            threshold=client.compression_threshold,
            compressed_frames=getattr(client, "compressed_frames", 0),
            uncompressed_frames=getattr(client, "uncompressed_frames", 0),
            chunks=set(client.chunks),
            raw_chunks=dict(client.raw_chunk),
            batches=list(client.chunk_batches),
            pos=client.pos,
        )
    return observed


def live_wire_parity(binary):
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    # Deleting any ambient override makes this session see the server's own
    # default: vanilla 1.21.1 negotiates the 256-byte threshold over 0x03.
    compressed = wire_session(binary, {"MEGU_COMPRESSION_THRESHOLD": None})
    require(compressed["threshold"] == 256, "0x03-default-threshold")
    require(compressed["chunks"] == spawn_window(), "0x03-window")
    # Real 0x03 wire evidence: some frames (chunk data) ride zlib bodies while
    # short packets (login success, teleports, batch markers) stay data_len==0.
    require(compressed["compressed_frames"] > 0, "0x03-no-zlib-frames")
    require(compressed["uncompressed_frames"] > 0, "0x03-no-plain-frames")

    # 0x03 negotiated above every packet's size: the compression wrapper is on
    # the wire yet no body is large enough to compress.
    wrapped = wire_session(binary, {"MEGU_COMPRESSION_THRESHOLD": "1048576"})
    require(wrapped["threshold"] == 1048576, "0x03-high-threshold")
    require(wrapped["chunks"] == spawn_window(), "0x03-high-window")
    require(wrapped["compressed_frames"] == 0, "0x03-high-zlib-frames")
    require(wrapped["uncompressed_frames"] > 0, "0x03-high-plain-frames")

    # Compression off: no 0x03, every frame is a bare length-prefixed packet.
    plain = wire_session(binary, {"MEGU_COMPRESSION_THRESHOLD": "-1"})
    require(plain["threshold"] == -1, "uncompressed-negotiated")
    require(plain["chunks"] == spawn_window(), "uncompressed-window")
    require(plain["compressed_frames"] == 0 and plain["uncompressed_frames"] == 0,
            "uncompressed-framing")

    # A fresh world always resolves seed 0, so the three sessions generated the
    # same spawn area: the transport must not change a single delivered byte.
    require(wrapped["raw_chunks"] == compressed["raw_chunks"], "0x03-high-payload-parity")
    require(plain["raw_chunks"] == compressed["raw_chunks"], "uncompressed-payload-parity")
    require(plain["pos"] == compressed["pos"], "spawn-position-parity")


def main(binary):
    require(binary.is_file(), f"missing-binary-{binary}")
    live_wire_parity(binary)


if __name__ == "__main__":
    target = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target/debug/megu"
    try:
        main(target)
    except ContractFailure as error:
        print(f"protocol767 live wire: FAIL {error.code}", file=sys.stderr)
        raise
    print("protocol767 live wire: 0x03 compressed, wrapped, and uncompressed paths PASS")
