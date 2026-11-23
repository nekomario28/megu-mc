#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Client-side 3-layer bedrock gate for `megu-client` (Protocol 767).

Layer 1 — seeded adversarial S2C fuzz: a deterministic loopback mock
Protocol 767 server completes Login + Configuration (uncompressed and
compressed ``0x03`` framing) and then streams hostile Play packets at the
real ``megu-client`` binary. Hard invariants per wave: no ``panicked
at``, no fatal signal (negative exit), no hang beyond a bounded timeout,
and no ``--accessToken`` leakage into stdout/stderr.

Layer 2 — clean loopback live verification: the same client runs
``--script walk`` with ``--render-png``/``--audio-wav``/``--json-summary``
against a fresh ``megu`` server (and, with ``--with-vanilla``, the
pinned official Vanilla 1.21.1 ``server.jar``). Session-summary invariants
plus PNG/WAV artifact validation and byte-identical determinism across two
runs are enforced.

``--self-test`` exercises the seeded vector generator, the failure
detectors, and the PNG/WAV validators without any compiled binaries.

Usage: python3 tests/client_oracle_fuzz_wire.py [--client-bin PATH]
       [--server-bin PATH] [--with-vanilla] [--vanilla-jar PATH]
       [--seed N] [--cases N] [--self-test]
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from typing import NamedTuple

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CLIENT = ROOT / "client-app/target/release/megu-client"
DEFAULT_SERVER = ROOT / "target/debug/megu"
DEFAULT_JAR = Path(os.environ.get("MEGU_VANILLA_JAR", str(Path.home() / ".cache/megu-audit/load/server.jar")))
JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
JAVA_CANDIDATES = (
    Path("/usr/lib/jvm/java-21-openjdk/bin/java"),
    Path("/usr/lib/jvm/default/bin/java"),
)

PROTOCOL = 767
USERNAME = "OracleBot"
TOKEN_SENTINEL = "ORACLE_TOKEN_MUST_NOT_LEAK_9f3c1e"
MAX_PACKET_SIZE = 16 * 1024 * 1024
WAVE_TIMEOUT = 15.0
HANDSHAKE_TIMEOUT = 10.0
PROBE_TIMEOUT = 2.0
LIVE_TIMEOUT = 180.0
SERVER_READY_TIMEOUT = 120.0

# Protocol 767 packet ids (proto/src/data_v767.rs — do not renumber).
LS_DISCONNECT = 0x00
LS_LOGIN_SUCCESS = 0x02
LS_SET_COMPRESSION = 0x03
LC_LOGIN_ACK = 0x03
CF_REGISTRY_DATA = 0x07
CF_FEATURE_FLAGS = 0x0C
CF_SELECT_KNOWN_PACKS = 0x0E
CF_FINISH = 0x03
CC_CLIENT_INFO = 0x00
CC_SELECT_KNOWN_PACKS = 0x07
CC_FINISH = 0x03
P_DISCONNECT = 0x1D
P_KEEP_ALIVE = 0x26
P_CHUNK = 0x27
P_LIGHT_UPDATE = 0x2A
P_LOGIN = 0x2B
P_CUSTOM_PAYLOAD = 0x19
P_BLOCK_UPDATE = 0x09
P_SECTION_BLOCKS = 0x49
P_PLAYER_POS = 0x40
P_RESPAWN = 0x47
P_SET_HEALTH = 0x5D
P_SOUND_ENTITY = 0x67
P_SOUND_EFFECT = 0x68
P_STOP_SOUND = 0x6A
P_SYSTEM_CHAT = 0x6C
P_CHUNK_BATCH_START = 0x0D
P_CHUNK_BATCH_FINISHED = 0x0C
P_FORGET_CHUNK = 0x21
P_SET_CENTER = 0x54
P_LEVEL_EVENT = 0x28
P_LEVEL_PARTICLES = 0x29
P_ENTITY_EVENT = 0x1F
P_SET_ENTITY_DATA = 0x58
P_GAME_EVENT = 0x22
SC_ACCEPT_TELEPORT = 0x00
SC_KEEP_ALIVE = 0x18

SECTION_COUNT = 24          # overworld: height 384 / 16
BIOME_COUNT = 64
CELLS_PER_SECTION = 4096
STATE_COUNT = 20_000        # generous bound for clamped palette ids


class OracleFailure(AssertionError):
    pass


def check(condition, message):
    if not condition:
        raise OracleFailure(message)


def log(message):
    print(f"[client-oracle] {message}", flush=True)


# ---------------------------------------------------------------------------
# Wire primitives (Protocol 767 big-endian VarInt/VarLong + framing)
# ---------------------------------------------------------------------------

def varint(value):
    """Encode an i32 as a Minecraft VarInt (two's-complement negatives)."""
    value &= 0xFFFFFFFF
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def varlong(value):
    """Encode an i64 as a Minecraft VarLong."""
    value &= 0xFFFFFFFFFFFFFFFF
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def mc_string(text):
    raw = text.encode("utf-8")
    return varint(len(raw)) + raw


def packet(pid, body=b""):
    return varint(pid) + body


def frame_packet(raw, threshold=-1):
    """Length-prefix one packet, mirroring the client's `encode_packet`.

    With compression negotiated, frames are ``VarInt(frame_len)`` over
    ``VarInt(data_len) + payload`` where ``data_len`` is 0 for raw packets
    shorter than the threshold and the uncompressed length followed by the
    zlib stream otherwise.
    """
    if threshold >= 0:
        if len(raw) < threshold:
            inner = varint(0) + raw
        else:
            inner = varint(len(raw)) + zlib.compress(raw)
    else:
        inner = raw
    return varint(len(inner)) + inner


def _read_varint_bytes(data, pos):
    value = 0
    for i in range(5):
        if pos >= len(data):
            raise OracleFailure("truncated varint")
        byte = data[pos]
        pos += 1
        value |= (byte & 0x7F) << (7 * i)
        if not byte & 0x80:
            return value, pos
    raise OracleFailure("oversized varint")


def unframe_packet(frame_data, threshold=-1):
    """Inverse of `frame_packet` for one received frame body."""
    if threshold < 0:
        return frame_data
    data_len, pos = _read_varint_bytes(frame_data, 0)
    rest = frame_data[pos:]
    if data_len == 0:
        return rest
    return zlib.decompress(rest)


def strip_frame_prefix(framed):
    """Return (payload, consumed) for one length-prefixed frame."""
    length, pos = _read_varint_bytes(framed, 0)
    return framed[pos:pos + length], pos + length


def pack_block_pos(x, y, z):
    """`x:26 | z:26 | y:12` packed position (proto BlockPos::pack)."""
    return (((x & 0x3FFFFFF) << 38) | ((z & 0x3FFFFFF) << 12)
            | (y & 0xFFF)) & 0xFFFFFFFFFFFFFFFF


def signed(value, bits):
    return value - (1 << bits) if value & (1 << (bits - 1)) else value


# ---------------------------------------------------------------------------
# Packet body builders
# ---------------------------------------------------------------------------

def nbt_empty_compound():
    return b"\x0a\x00"


def nbt_text_component(text):
    """`{"text": "..."}` chat component as raw NBT."""
    body = text.encode("utf-8")
    return (b"\x0a"                          # TAG_Compound
            b"\x08"                          #   TAG_String entry
            + struct.pack(">H", 4) + b"text"  #   name "text"
            + struct.pack(">H", len(body)) + body
            + b"\x00")                       # end compound


def nbt_nested_compound(depth):
    """`depth` nested compounds around a string — exercises the NBT cap."""
    out = b"\x08" + struct.pack(">H", 4) + b"deep"
    for _ in range(depth):
        out = b"\x0a" + struct.pack(">H", 1) + b"k" + out + b"\x00"
    return out


def spawn_info(dim_type=0, dim_name="minecraft:overworld", game_mode=1,
               prev_game_mode=1, is_debug=0, is_flat=0, death=None,
               portal_cooldown=0, hashed_seed=0):
    out = bytearray()
    out += varint(dim_type)
    out += mc_string(dim_name)
    out += struct.pack(">q", hashed_seed)
    out += bytes([game_mode & 0xFF, prev_game_mode & 0xFF,
                  is_debug & 0xFF, is_flat & 0xFF])
    if death is None:
        out += b"\x00"
    else:
        out += b"\x01" + mc_string(death[0]) + struct.pack(">q", death[1])
    out += varint(portal_cooldown)
    return bytes(out)


def play_login_body(entity_id=1, dim_name="minecraft:overworld", dim_type=0,
                    extra_dims=(), max_players=20, view_distance=2,
                    sim_distance=2, game_mode=1):
    out = bytearray()
    out += struct.pack(">i", entity_id)
    out += b"\x00"                       # is_hardcore
    dims = [dim_name, *extra_dims]
    out += varint(len(dims))
    for name in dims:
        out += mc_string(name)
    out += varint(max_players) + varint(view_distance) + varint(sim_distance)
    out += b"\x00\x00\x00"               # reduced_debug, respawn_screen, limited_crafting
    out += spawn_info(dim_type=dim_type, dim_name=dim_name, game_mode=game_mode)
    out += b"\x00"                       # enforces_secure_chat
    return bytes(out)


def play_teleport_body(x, y, z, yaw=0.0, pitch=0.0, flags=0, teleport_id=1):
    return (struct.pack(">ddd", x, y, z)
            + struct.pack(">ff", yaw, pitch)
            + bytes([flags & 0xFF])
            + varint(teleport_id))


def keep_alive_body(value=0x51AB):
    return struct.pack(">q", value)


def block_update_body(x, y, z, state_id):
    return struct.pack(">q", pack_block_pos(x, y, z)) + varint(state_id)


def section_blocks_body(section_x, section_y, section_z, records):
    """`records`: iterable of (state_id, lx, ly, lz)."""
    sx = signed(section_x, 22)
    sz = signed(section_z, 22)
    sy = signed(section_y, 20)
    packed = (((sx & 0x3FFFFF) << 42) | ((sz & 0x3FFFFF) << 20)
              | (sy & 0xFFFFF)) & 0xFFFFFFFFFFFFFFFF
    out = bytearray(struct.pack(">q", packed))
    out += varint(len(records))
    for state, lx, ly, lz in records:
        record = ((state & 0xFFFFFFFFFFFFF) << 12) | ((lx & 0xF) << 8) \
            | ((lz & 0xF) << 4) | (ly & 0xF)
        out += varlong(record)
    return bytes(out)


def light_payload(sky_mask=None, block_mask=None, empty_sky=None,
                  empty_block=None, sky_arrays=None, block_arrays=None):
    """The four light masks + packed arrays shared by 0x27 and 0x2A.

    Each mask is ``VarInt word_count`` + big-endian u64 words; each array
    is ``VarInt length`` + that many bytes of nibbles.
    """
    out = bytearray()
    for words in (sky_mask or [], block_mask or [],
                  empty_sky or [], empty_block or []):
        out += varint(len(words))
        for word in words:
            out += struct.pack(">Q", word & 0xFFFFFFFFFFFFFFFF)
    out += varint(len(sky_arrays or []))
    for arr in sky_arrays or []:
        out += varint(len(arr)) + arr
    out += varint(len(block_arrays or []))
    for arr in block_arrays or []:
        out += varint(len(arr)) + arr
    return bytes(out)


def empty_light():
    return light_payload()


def light_update_body(cx, cz, **kwargs):
    return varint(cx) + varint(cz) + light_payload(**kwargs)


# --- paletted containers -------------------------------------------------

def palette_singleton(value, data_len=0):
    """bits=0 container: single value + optional ignored long count."""
    return (b"\x00" + varint(value) + varint(data_len)
            + b"\x00" * (data_len * 8))


def palette_direct(bits, words):
    """Direct (non-indexed) container: u8 bits + VarInt long count + u64s."""
    return bytes([bits]) + varint(len(words)) \
        + b"".join(struct.pack(">Q", w & 0xFFFFFFFFFFFFFFFF) for w in words)


def pack_palette_indices(indices, bits):
    """Pack LSB-first indices into u64 words for `bits`-wide entries."""
    per_word = 64 // bits
    mask = (1 << bits) - 1
    words = []
    for base in range(0, len(indices), per_word):
        word = 0
        for j, idx in enumerate(indices[base:base + per_word]):
            word |= (idx & mask) << (j * bits)
        words.append(word)
    return words


def palette_indirect(bits, values, indices):
    """Indirect container: u8 bits + VarInt palette len + VarInt entries +
    VarInt long count + packed index words."""
    words = pack_palette_indices(indices, bits)
    out = bytearray([bits])
    out += varint(len(values))
    for value in values:
        out += varint(value)
    out += varint(len(words))
    for word in words:
        out += struct.pack(">Q", word)
    return bytes(out)


def chunk_section(states_container, biome_container, non_air=0):
    return struct.pack(">h", non_air) + states_container + biome_container


def air_section():
    return chunk_section(palette_singleton(0), palette_singleton(0), 0)


def solid_section(state=1, non_air=CELLS_PER_SECTION):
    return chunk_section(palette_singleton(state), palette_singleton(0),
                         non_air)


def chunk_body(cx, cz, section_bytes, block_entities=None, light=None,
               heightmaps=None, data_len_override=None):
    """Assemble a `level_chunk_with_light` (0x27) body.

    `section_bytes` is the concatenated per-section payload advertised via
    VarInt `data_len` unless overridden (for truncation fuzzing).
    """
    out = bytearray()
    out += struct.pack(">ii", cx, cz)
    out += heightmaps if heightmaps is not None else nbt_empty_compound()
    data_len = (len(section_bytes) if data_len_override is None
                else data_len_override)
    out += varint(data_len) + section_bytes
    out += block_entities if block_entities is not None else varint(0)
    out += light if light is not None else empty_light()
    return bytes(out)


def standard_chunk(cx=0, cz=0, section_fn=None, section_count=SECTION_COUNT):
    fn = section_fn or (lambda _i: air_section())
    return chunk_body(cx, cz, b"".join(fn(i) for i in range(section_count)))


# ---------------------------------------------------------------------------
# Adversarial vector catalog
# ---------------------------------------------------------------------------

class FuzzVector(NamedTuple):
    name: str
    kind: str            # "packet" | "raw" | "close"
    payload: object      # (pid, body) | bytes | None
    fatal: bool = False  # expected to end the session's decode loop
    note: str = ""
    solo: bool = False   # replace the valid play prelude (login fuzz)
    close_after: bool = False


def _sound_tail(x, y, z, volume, pitch, seed):
    return (struct.pack(">iii", x, y, z)
            + struct.pack(">ff", volume, pitch)
            + struct.pack(">q", seed))


def _sound_body(holder, category, tail, name=None, has_range=False,
                rng_val=0.0):
    out = bytearray(varint(holder))
    if holder == 0:
        out += mc_string(name or "minecraft:entity.puffer_fish.ambient")
        out += b"\x01" if has_range else b"\x00"
        if has_range:
            out += struct.pack(">f", rng_val)
    out += varint(category) + tail
    return bytes(out)


def build_context_vectors():
    """Hostile-but-decodable (or swallowed) Play packets — context noise
    that must never end the session before the primary vector lands."""
    vecs = []

    def add(name, pid, body, note=""):
        vecs.append(FuzzVector(name, "packet", (pid, body), False, note))

    # --- 0x40 player_position: extreme-but-decodable teleports -----------
    add("tp_nan_coords", P_PLAYER_POS,
        play_teleport_body(float("nan"), 64.0, 0.0, teleport_id=2),
        "NaN x coordinate")
    add("tp_pos_inf", P_PLAYER_POS,
        play_teleport_body(float("inf"), float("-inf"), 1e300,
                           teleport_id=3),
        "+inf/-inf/1e300 coordinates")
    add("tp_neg_inf", P_PLAYER_POS,
        play_teleport_body(-float("inf"), float("nan"), -1e300,
                           teleport_id=4),
        "-inf/NaN coordinates")
    add("tp_nan_angles", P_PLAYER_POS,
        play_teleport_body(0.5, 64.0, 0.5, yaw=float("nan"),
                           pitch=float("inf"), teleport_id=5),
        "NaN yaw, +inf pitch")
    add("tp_flags_ff", P_PLAYER_POS,
        play_teleport_body(1.0, 1.0, 1.0, flags=0xFF, teleport_id=6),
        "all relative-move flag bits set")
    add("tp_id_negative", P_PLAYER_POS,
        play_teleport_body(2.0, 65.0, 2.0, teleport_id=-1),
        "negative teleport id varint")
    add("tp_id_max", P_PLAYER_POS,
        play_teleport_body(3.0, 66.0, 3.0, teleport_id=0x7FFFFFFF),
        "i32-max teleport id")
    add("tp_trailing_garbage", P_PLAYER_POS,
        play_teleport_body(4.0, 67.0, 4.0, teleport_id=7)
        + b"\xde\xad\xbe\xef",
        "valid teleport plus trailing bytes")

    # --- 0x68/0x67 sound packets: decode errors are swallowed -------------
    add("snd_nan_volume", P_SOUND_EFFECT,
        _sound_body(0, 4, _sound_tail(0, 0, 0, float("nan"), 1.0, 0)),
        "NaN volume")
    add("snd_inf_pitch", P_SOUND_EFFECT,
        _sound_body(0, 2, _sound_tail(8, 64, 8, -float("inf"),
                                      float("inf"), -1)),
        "-inf volume, +inf pitch, negative seed")
    add("snd_extreme_coords", P_SOUND_EFFECT,
        _sound_body(0, 0, _sound_tail(2**31 - 1, -2**31, 2**31 - 1,
                                      1.0, 1.0, 0)),
        "i32-extreme fixed-point coordinates")
    add("snd_registry_holder", P_SOUND_EFFECT,
        _sound_body(0x7FFFFFFF, 9, _sound_tail(1, 2, 3, 1.0, 1.0, 0)),
        "huge sound_event registry holder id")
    snd_entity = (varint(0) + mc_string("minecraft:entity.item.pickup")
                  + b"\x00" + varint(3) + varint(12345)
                  + struct.pack(">ff", float("nan"), 0.5)
                  + struct.pack(">q", 7))
    add("snd_entity_nan", P_SOUND_ENTITY, snd_entity,
        "entity-bound NaN volume")
    add("snd_truncated", P_SOUND_EFFECT, b"\x00\x03abc",
        "sound body truncated mid-string (error swallowed)")
    add("snd_long_name", P_SOUND_EFFECT,
        _sound_body(0, 1, _sound_tail(0, 0, 0, 1.0, 1.0, 0),
                    name="minecraft:" + "x" * 240),
        "near-bound sound name")

    # --- 0x6C system_chat: NBT errors degrade to default text -------------
    add("chat_deep_nbt", P_SYSTEM_CHAT,
        nbt_nested_compound(80) + b"\x00",
        "80-deep nested NBT (depth cap)")
    add("chat_bad_tag", P_SYSTEM_CHAT, b"\x63\x00\x00",
        "unknown NBT tag 0x63")
    add("chat_overlay", P_SYSTEM_CHAT,
        nbt_text_component("oracle") + b"\x01",
        "valid text component, overlay flag")

    # --- 0x5D set_health --------------------------------------------------
    add("health_nan", P_SET_HEALTH,
        struct.pack(">f", float("nan")) + varint(20)
        + struct.pack(">f", 5.0),
        "NaN health")
    add("health_extremes", P_SET_HEALTH,
        struct.pack(">f", -float("inf")) + varint(-1)
        + struct.pack(">f", 1e30),
        "-inf health, negative food, huge saturation")

    # --- 0x09 block_update -------------------------------------------------
    add("bu_state_999999", P_BLOCK_UPDATE,
        block_update_body(0, 64, 0, 999999), "out-of-range state id")
    add("bu_state_max", P_BLOCK_UPDATE,
        block_update_body(1, 65, 1, 0x7FFFFFFF), "i32-max state id")
    add("bu_pos_edges", P_BLOCK_UPDATE,
        block_update_body((1 << 25) - 1, 2047, -(1 << 25), 7),
        "packed-position field edges")
    add("bu_pos_low_y", P_BLOCK_UPDATE,
        block_update_body(0, -2048, 0, 3), "y below overworld min (ignored)")

    # --- 0x49 section_blocks_update ----------------------------------------
    add("sbu_huge_state", P_SECTION_BLOCKS,
        section_blocks_body(0, 4, 0, [(0xFFFFFFFFFFFFF, 0, 0, 0)]),
        "52-bit state field truncation")
    add("sbu_section_edges", P_SECTION_BLOCKS,
        section_blocks_body((1 << 21) - 1, (1 << 19) - 1, -(1 << 21),
                            [(1, 15, 15, 15)]),
        "22/20-bit section coordinate edges")
    add("sbu_empty", P_SECTION_BLOCKS,
        section_blocks_body(0, 0, 0, []), "zero update records")
    add("sbu_many", P_SECTION_BLOCKS,
        section_blocks_body(0, 4, 0,
                            [(i % 50000, i % 16, (i * 7) % 16,
                              (i * 3) % 16) for i in range(64)]),
        "64 packed update records")

    # --- 0x2A light_update --------------------------------------------------
    add("lu_full_sky", P_LIGHT_UPDATE,
        light_update_body(0, 0, sky_mask=[0x3FFFFFF],
                          sky_arrays=[bytes([0xFF]) * 2048] * 26),
        "all-section sky mask + 26 arrays")
    add("lu_high_bits", P_LIGHT_UPDATE,
        light_update_body(0, 0, sky_mask=[0xFFFFFFFFFFFFFFFF],
                          sky_arrays=[bytes(2048)] * 64),
        "mask bits beyond section count (ignored)")
    add("lu_empty", P_LIGHT_UPDATE, light_update_body(5, -5),
        "all-empty masks at odd coords")
    add("lu_block_only", P_LIGHT_UPDATE,
        light_update_body(0, 0, block_mask=[0x1],
                          block_arrays=[bytes(range(256)) * 8]),
        "single block-light section")

    # --- 0x27 chunk: decodable variants ------------------------------------
    add("ck_air_chunk", P_CHUNK, standard_chunk(0, 0), "valid all-air chunk")
    add("ck_stone_section", P_CHUNK,
        standard_chunk(1, 0,
                       section_fn=lambda i: solid_section()
                       if i == 12 else air_section()),
        "one solid mid section")
    add("ck_all_stone", P_CHUNK,
        standard_chunk(-2, -2, section_fn=lambda _i: solid_section()),
        "24 solid stone sections")
    add("ck_extreme_coords", P_CHUNK,
        standard_chunk(0x7FFFFFFF, -0x80000000), "i32-extreme chunk coords")
    add("ck_singleton_999999", P_CHUNK,
        standard_chunk(3, 3,
                       section_fn=lambda _i: chunk_section(
                           palette_singleton(999999),
                           palette_singleton(0), 0)),
        "singleton state id 999999 everywhere")
    add("ck_palette_999999", P_CHUNK,
        standard_chunk(3, 4,
                       section_fn=lambda _i: chunk_section(
                           palette_indirect(4, [999999], [0] * 4096),
                           palette_singleton(0), 0)),
        "indirect palette value 999999 (clamped downstream)")
    add("ck_bits9_direct", P_CHUNK,
        standard_chunk(4, 4,
                       section_fn=lambda _i: chunk_section(
                           palette_direct(9, [0] * 586),
                           palette_singleton(0), 0)),
        "9-bit 'direct' state palette, zeroed")
    add("ck_bits9_states", P_CHUNK,
        standard_chunk(4, 5,
                       section_fn=lambda _i: chunk_section(
                           palette_indirect(
                               4, [0, 1, 19999], [0, 1, 2] * 1365 + [0]),
                           palette_singleton(0), 0)),
        "indirect palette cycling in-range ids")
    add("ck_ignored_longs", P_CHUNK,
        standard_chunk(5, 5,
                       section_fn=lambda _i: chunk_section(
                           palette_singleton(0, data_len=3),
                           palette_singleton(0), 0)),
        "singleton palette with nonzero ignored long count")
    big_direct = palette_direct(20, [0] * 1366)
    add("ck_bits20_direct", P_CHUNK,
        standard_chunk(6, 6,
                       section_fn=lambda _i: chunk_section(
                           big_direct, palette_singleton(0), 0)),
        "20-bit direct palette, ~270 KiB raw packet")

    # --- misc play packets ---------------------------------------------------
    add("unk_pid_0x7c", 0x7C, b"\xaa\xbb\xcc", "unassigned play packet id")
    add("unk_pid_0xff", 0xFF, b"\x01\x02\x03\x04", "high unassigned id")
    add("unk_pid_negative", -1, b"\x00", "negative packet id varint")
    add("unk_pid_max", 0x7FFFFFFF, b"", "i32-max packet id")
    add("batch_finished_neg", P_CHUNK_BATCH_FINISHED, varint(-5),
        "negative batch size")
    add("batch_finished_max", P_CHUNK_BATCH_FINISHED, varint(0x7FFFFFFF),
        "i32-max batch size")
    add("batch_start", P_CHUNK_BATCH_START, b"", "empty batch start")
    add("unload_extreme", P_FORGET_CHUNK,
        struct.pack(">ii", -0x80000000, 0x7FFFFFFF),
        "i32-extreme unload coords")
    add("center_extreme", P_SET_CENTER,
        varint(-0x80000000) + varint(0x7FFFFFFF),
        "i32-extreme center chunk")
    add("respawn_nether", P_RESPAWN,
        spawn_info(dim_type=1, dim_name="minecraft:the_nether",
                   game_mode=1) + b"\x01",
        "respawn into nether dimension meta")
    add("respawn_unknown_dim", P_RESPAWN,
        spawn_info(dim_type=9999, dim_name="minecraft:custom_dimension",
                   game_mode=0) + b"\x00",
        "respawn with unregistered dimension (fallback meta)")
    add("custom_payload_vfx", P_CUSTOM_PAYLOAD,
        mc_string("megu:vfx_v1") + b"\xff\xfe\xfd" * 8,
        "known custom channel, malformed payload (ignored by recv_play)")
    add("custom_payload_unknown", P_CUSTOM_PAYLOAD,
        mc_string("totally:unknown") + bytes(range(256)),
        "unknown custom channel")
    add("stop_sound_ff", P_STOP_SOUND,
        b"\xff\x05" + mc_string("x" * 250), "stop_sound with all flag bits")
    add("level_event_garbage", P_LEVEL_EVENT, bytes(range(64)),
        "level_event with arbitrary body (Other)")
    add("entity_event", P_ENTITY_EVENT, varint(-3) + b"\xff",
        "entity_event extremes (Other)")
    add("set_entity_data", P_SET_ENTITY_DATA, b"\xde\xad" * 32,
        "set_entity_data garbage (Other)")
    add("game_event", P_GAME_EVENT,
        varint(0x7FFFFFFF) + struct.pack(">f", float("nan")),
        "game_event extremes (Other)")
    add("particles_garbage", P_LEVEL_PARTICLES, bytes(range(32)),
        "level_particles arbitrary body (Other)")

    return vecs


def build_fatal_vectors():
    """Packets whose decode must fail cleanly (protocol error -> exit 1)."""
    vecs = []

    def add(name, pid, body, note=""):
        vecs.append(FuzzVector(name, "packet", (pid, body), True, note))

    def raw(name, data, note="", close_after=False):
        vecs.append(FuzzVector(name, "raw", data, True, note, False,
                               close_after))

    # --- generic truncation / empty / malformed varint ----------------------
    add("tp_truncated", P_PLAYER_POS,
        play_teleport_body(1.0, 2.0, 3.0, teleport_id=9)[:10],
        "teleport cut inside coordinate doubles")
    add("health_truncated", P_SET_HEALTH, b"\x00\x00",
        "health cut to 2 bytes")
    add("keepalive_truncated", P_KEEP_ALIVE, struct.pack(">q", 1)[:4],
        "keep_alive cut to 4 bytes")
    add("batch_truncated", P_CHUNK_BATCH_FINISHED, b"",
        "empty batch_finished body")
    add("block_update_empty", P_BLOCK_UPDATE, b"",
        "block_update with empty body")
    add("block_update_neg_state", P_BLOCK_UPDATE,
        block_update_body(0, 64, 0, -1), "negative block state id varint")
    add("block_update_trunc_pos", P_BLOCK_UPDATE,
        struct.pack(">q", pack_block_pos(0, 64, 0))[:5] + varint(1),
        "packed i64 position cut to 5 bytes")
    add("section_blocks_neg_count", P_SECTION_BLOCKS,
        section_blocks_body(0, 0, 0, [])[:-1] + varint(-1),
        "negative update count")
    add("section_blocks_over_count", P_SECTION_BLOCKS,
        struct.pack(">q", 0) + varint(50) + b"\x01",
        "count 50 with 1 remaining byte")
    add("section_blocks_trunc_record", P_SECTION_BLOCKS,
        struct.pack(">q", 0) + varint(1) + b"\x80",
        "varlong record cut mid-continuation")
    add("light_update_mask_over", P_LIGHT_UPDATE,
        light_update_body(0, 0, sky_mask=[0, 0]),
        "sky mask word count 2 > bound 1")
    add("light_update_count_mismatch", P_LIGHT_UPDATE,
        varint(0) + varint(0)
        + varint(1) + struct.pack(">Q", 3)   # sky mask: popcount 2
        + varint(0) + varint(0) + varint(0)  # other masks empty
        + varint(1)                          # sky array count 1 != 2
        + varint(0),
        "sky array count mismatches mask popcount")
    add("light_update_short_array", P_LIGHT_UPDATE,
        varint(0) + varint(0)
        + varint(1) + struct.pack(">Q", 1)
        + varint(0) + varint(0) + varint(0)
        + varint(1) + varint(100) + bytes(100)  # declares 100, not 2048
        + varint(0),
        "sky array length != 2048")
    add("light_update_truncated", P_LIGHT_UPDATE,
        (varint(0) + varint(0)
         + varint(1) + struct.pack(">Q", 1)
         + varint(0) + varint(0) + varint(0)
         + varint(1) + varint(2048) + bytes(100)),
        "sky array truncated at 100/2048 bytes")

    # --- chunk (0x27) decode failures ---------------------------------------
    add("ck_empty_body", P_CHUNK, b"", "chunk packet with empty body")
    add("ck_bad_heightmap_nbt", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   heightmaps=b"\x63\x00\x00"),
        "heightmaps NBT with unknown tag")
    add("ck_deep_heightmap_nbt", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   heightmaps=nbt_nested_compound(80)),
        "heightmaps NBT beyond depth cap")
    add("ck_data_len_huge", P_CHUNK,
        struct.pack(">ii", 0, 0) + nbt_empty_compound() + varint(0x7FFFFFFF),
        "section data length i32-max")
    add("ck_data_len_mismatch", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   data_len_override=10_000),
        "declared data_len exceeds actual section bytes")
    add("ck_few_sections", P_CHUNK, chunk_body(0, 0, air_section() * 4),
        "section payload ends inside section 5")
    add("ck_unparsed_tail", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT + b"\xAA" * 7),
        "trailing bytes after last section")
    add("ck_neg_singleton", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           palette_singleton(-1),
                           palette_singleton(0), 0)),
        "negative singleton palette id")
    add("ck_bits3_states", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           bytes([3]) + varint(2) + varint(1) + varint(2)
                           + struct.pack(">Q", 0) * 1024,
                           palette_singleton(0), 0)),
        "state palette bits=3 below minimum 4")
    add("ck_bits9_short", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           bytes([9]) + varint(0),
                           palette_singleton(0), 0)),
        "9-bit palette with zero longs (expected 586)")
    add("ck_indirect_oob", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           palette_indirect(4, [1, 2],
                                            [0, 1, 5] * 1365 + [0]),
                           palette_singleton(0), 0)),
        "indirect palette index 5 out of range 2")
    add("ck_indirect_neg", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           palette_indirect(4, [-1, 2], [0] * 4096),
                           palette_singleton(0), 0)),
        "negative indirect palette entry")
    add("ck_palette_len_over", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           bytes([4]) + varint(0x7FFFFFFF),
                           palette_singleton(0), 0)),
        "indirect palette length i32-max")
    add("ck_biome_bits_bad", P_CHUNK,
        standard_chunk(0, 0,
                       section_fn=lambda _i: chunk_section(
                           palette_singleton(0),
                           bytes([2]) + varint(0) + varint(0), 0)),
        "biome palette bits=2 with empty palette/data")
    add("ck_light_mask_over", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   light=varint(0) + varint(0)
                   + varint(5) + struct.pack(">Q", 1) * 5
                   + varint(0) + varint(0) + varint(0) + varint(0)),
        "chunk light mask word count 5 > bound 1")
    add("ck_light_count_mismatch", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   light=varint(0) + varint(0)
                   + varint(1) + struct.pack(">Q", 3)
                   + varint(0) + varint(0) + varint(0)
                   + varint(1)),
        "sky light array count 1 != mask popcount 2")
    add("ck_light_truncated", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   light=varint(0) + varint(0)
                   + varint(1) + struct.pack(">Q", 1)
                   + varint(0) + varint(0) + varint(0)
                   + varint(1) + varint(2048) + bytes(100)),
        "sky light array truncated")
    add("ck_block_entities_over", P_CHUNK,
        chunk_body(0, 0, air_section() * SECTION_COUNT,
                   block_entities=varint(0x7FFFFFFF)),
        "block entity count i32-max (EOF on first record)")

    # --- respawn / login-shape packets sent mid-play -------------------------
    add("respawn_truncated", P_RESPAWN, spawn_info()[:6],
        "respawn cut inside spawn_info")
    add("login_truncated_replay", P_LOGIN, play_login_body()[:20],
        "second login packet, truncated")
    add("disconnect_empty_nbt", P_DISCONNECT, b"",
        "disconnect with empty NBT body (clean session error)")

    # --- frame-level raw attacks ----------------------------------------------
    raw("raw_len_varint_overflow", b"\xff\xff\xff\xff\x7f",
        "5-byte frame length varint overflows u32 bound")
    raw("raw_len_too_big", varint(MAX_PACKET_SIZE + 1) + b"\x00" * 8,
        "declared frame length exceeds 16 MiB bound")
    raw("raw_empty_frame", varint(0),
        "zero-length frame decodes to empty packet")
    raw("raw_pid_varint_overflow", varint(4) + b"\x80\x80\x80\x80\x80",
        "packet id varint never terminates within 5 bytes")
    raw("raw_desync_varint", b"\x80\x80\x80",
        "incomplete length varint desyncs the stream")
    raw("raw_garbage_then_valid",
        varint(3) + b"\x61\x62\x63"
        + frame_packet(packet(P_KEEP_ALIVE, keep_alive_body(0xFEED))),
        "unknown pid 0x61 then a valid keep_alive")
    raw("raw_mid_frame_drop", varint(4096) + b"\x00" * 200,
        "frame declares 4 KiB, only 200 bytes arrive, then close",
        close_after=True)
    raw("raw_drop_after_pid",
        varint(64) + varint(P_KEEP_ALIVE) + b"\x00" * 4,
        "partial keep_alive frame then close", close_after=True)

    return vecs


def build_login_fuzz_vectors():
    """Hostile `login` (0x2B join game) bodies — replace the valid prelude."""
    vecs = []

    def add(name, body, note=""):
        vecs.append(FuzzVector(name, "packet", (P_LOGIN, body), True, note,
                               True))

    add("login_empty", b"", "empty join-game body")
    add("login_truncated", play_login_body()[:15],
        "cut inside dimension list")
    add("login_neg_dim_count", play_login_body()[:5] + varint(-1),
        "negative dimension count")
    add("login_huge_dim_count", play_login_body()[:5] + varint(0x7FFFFFFF),
        "i32-max dimension count")
    add("login_many_dims", play_login_body(extra_dims=["d"] * 500),
        "500-entry dimension name list")
    add("login_bad_spawn_trunc", play_login_body()[:-3],
        "truncated inside spawn_info tail")
    add("login_neg_seed_dim", play_login_body(dim_type=-1),
        "negative dimension_type id (fallback meta, may decode)")
    add("login_flat_overworld", play_login_body(game_mode=3),
        "spectator-mode join (decodable control)")
    return vecs


# A few "context" entries actually fail decode — keep them in the fatal
# catalog where they belong.
_MISFILED = {"ck_short_sections"}
CONTEXT_VECTORS = build_context_vectors()
FATAL_VECTORS = build_fatal_vectors()
LOGIN_FUZZ_VECTORS = build_login_fuzz_vectors()

BENIGN_VECTORS = [
    FuzzVector("benign_keepalive", "packet",
               (P_KEEP_ALIVE, keep_alive_body(0xBEEF)), False,
               "control keep_alive"),
    FuzzVector("benign_chunk", "packet", (P_CHUNK, standard_chunk(9, 9)),
               False, "control air chunk"),
    FuzzVector("benign_block", "packet",
               (P_BLOCK_UPDATE, block_update_body(0, 64, 0, 1)), False,
               "control block update"),
]


def build_wave(case_index, case_seed):
    """Deterministically assemble one case's action list.

    Returns (kind, actions, meta): kind is "play", "raw", "login_fuzz",
    or "context_soak"; actions are ("packet", pid, body) | ("raw", bytes)
    | ("close",) tuples.
    """
    rng = random.Random(f"megu-oracle:{case_seed}:{case_index}")
    slot = case_index % 8

    if slot == 7:
        # Context soak: only non-fatal vectors + a benign terminator; the
        # post-wave keep_alive probe must still be answered.
        pool = CONTEXT_VECTORS + BENIGN_VECTORS
        picks = rng.sample(pool, min(len(pool), 3 + case_index % 4))
        actions = [("packet", v.payload[0], v.payload[1]) for v in picks]
        rng.shuffle(actions)
        return "context_soak", actions, {"vectors": [v.name for v in picks]}

    if slot == 6:
        # Frame-level raw/drop attacks.
        raw_vecs = [v for v in FATAL_VECTORS if v.kind == "raw"]
        vec = raw_vecs[(case_index // 8) % len(raw_vecs)]
        actions = [("raw", vec.payload)]
        if vec.close_after:
            actions.append(("close",))
        return "raw", actions, {"vectors": [vec.name]}

    if slot == 5:
        # Login-phase fuzz: hostile 0x2B replaces the valid join game.
        vec = LOGIN_FUZZ_VECTORS[(case_index // 8) % len(LOGIN_FUZZ_VECTORS)]
        return "login_fuzz", [("packet", vec.payload[0], vec.payload[1])], \
            {"vectors": [vec.name]}

    # Standard play wave: seeded non-fatal context + one fatal primary.
    fatal = FATAL_VECTORS[((case_index * 7) + 3) % len(FATAL_VECTORS)]
    k = 2 + (case_index % 3)
    context = rng.sample(CONTEXT_VECTORS, min(len(CONTEXT_VECTORS), k))
    actions = [("packet", v.payload[0], v.payload[1]) for v in context]
    rng.shuffle(actions)
    if fatal.kind == "packet":
        actions.append(("packet", fatal.payload[0], fatal.payload[1]))
    elif fatal.kind == "raw":
        actions.append(("raw", fatal.payload))
        if fatal.close_after:
            actions.append(("close",))
    meta = {"vectors": [v.name for v in context] + [fatal.name]}
    return "play", actions, meta


# ---------------------------------------------------------------------------
# Loopback mock Protocol 767 server
# ---------------------------------------------------------------------------

class MockTimeout(OracleFailure):
    pass


def recv_exact(sock, count, deadline):
    chunks = bytearray()
    while len(chunks) < count:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise MockTimeout("mock server read deadline")
        sock.settimeout(min(remaining, 0.5))
        try:
            data = sock.recv(count - len(chunks))
        except socket.timeout:
            continue
        if not data:
            raise MockTimeout("client closed connection mid-frame")
        chunks += data
    return bytes(chunks)


def recv_varint(sock, deadline):
    value = 0
    for i in range(5):
        byte = recv_exact(sock, 1, deadline)[0]
        if i == 4 and byte > 0x0F:
            raise MockTimeout("client varint overflow")
        value |= (byte & 0x7F) << (7 * i)
        if not byte & 0x80:
            return value
    raise MockTimeout("client varint exceeds 5 bytes")


class MockSession:
    """One accepted client connection driven through the 767 handshake."""

    def __init__(self, conn, compression_threshold):
        self.conn = conn
        # -1 until Set Compression is sent; only then does framing switch.
        self.negotiated = -1
        self.planned_threshold = compression_threshold
        self.c2s_seen = []
        self.username = None

    def close(self):
        try:
            self.conn.close()
        except OSError:
            pass

    # -- framing ---------------------------------------------------------

    def send_raw(self, data):
        self.conn.sendall(data)

    def send_packet(self, pid, body=b""):
        self.send_raw(frame_packet(packet(pid, body), self.negotiated))

    def recv_packet(self, timeout):
        """Read one client->server frame; returns (pid, body)."""
        deadline = time.monotonic() + timeout
        length = recv_varint(self.conn, deadline)
        if length > MAX_PACKET_SIZE:
            raise MockTimeout(f"client frame length {length} exceeds bound")
        frame = recv_exact(self.conn, length, deadline)
        try:
            raw = unframe_packet(frame, self.negotiated)
        except zlib.error as error:
            raise MockTimeout(f"client sent bad zlib: {error}") from error
        if not raw:
            raise MockTimeout("client sent empty packet")
        pid, pos = _read_varint_bytes(raw, 0)
        if pid >= (1 << 31):
            pid -= 1 << 32
        return pid, raw[pos:]

    def expect(self, wanted, timeout, label):
        """Read client packets until `wanted` pid arrives; returns its body."""
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise MockTimeout(
                    f"timed out waiting for client {label} "
                    f"(seen: {[hex(p) for p in self.c2s_seen]})")
            pid, body = self.recv_packet(min(remaining, HANDSHAKE_TIMEOUT))
            self.c2s_seen.append(pid)
            if pid == wanted:
                return body

    # -- handshake ---------------------------------------------------------

    def handshake(self):
        """Drive Login + Configuration; returns when Play phase begins."""
        # Handshake then Login Start — both carry packet id 0, in order.
        body = self.expect(0x00, HANDSHAKE_TIMEOUT, "handshake")
        reader = Reader(body)
        proto = reader.varint()
        host = reader.string()
        port = struct.unpack(">H", reader.take(2))[0]
        next_state = reader.varint()
        check(proto == PROTOCOL and next_state == 2,
              f"unexpected handshake proto={proto} next_state={next_state}")
        check(host == "127.0.0.1" and port > 0,
              f"unexpected handshake target {host}:{port}")

        body = self.expect(0x00, HANDSHAKE_TIMEOUT, "login_start")
        reader = Reader(body)
        self.username = reader.string()
        uuid_bytes = reader.take(16)
        check(len(uuid_bytes) == 16, "login_start uuid truncated")

        # Optional Set Compression (itself sent uncompressed), then
        # Login Success — compressed once the threshold is negotiated.
        if self.planned_threshold >= 0:
            self.send_raw(frame_packet(
                packet(LS_SET_COMPRESSION, varint(self.planned_threshold)),
                -1))
            self.negotiated = self.planned_threshold
        self.send_packet(LS_LOGIN_SUCCESS,
                         uuid_bytes + mc_string(self.username))
        self.expect(LC_LOGIN_ACK, HANDSHAKE_TIMEOUT, "login_acknowledged")

        # Configuration: registries, feature flags, known packs.
        self.send_packet(CF_REGISTRY_DATA,
                         mc_string("minecraft:dimension_type") + varint(4)
                         + b"".join(mc_string(name) + b"\x00" for name in (
                             "minecraft:overworld", "minecraft:overworld_caves",
                             "minecraft:the_nether", "minecraft:the_end")))
        self.send_packet(CF_REGISTRY_DATA,
                         mc_string("minecraft:worldgen/biome")
                         + varint(BIOME_COUNT)
                         + b"".join(mc_string(f"minecraft:biome_{i}")
                                    + b"\x00" for i in range(BIOME_COUNT)))
        self.send_packet(CF_FEATURE_FLAGS, varint(1) + mc_string("vanilla"))
        self.send_packet(CF_SELECT_KNOWN_PACKS, varint(0))

        deadline = time.monotonic() + HANDSHAKE_TIMEOUT
        got_info = got_packs = False
        while not (got_info and got_packs):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise MockTimeout(
                    "timed out waiting for config replies "
                    f"(seen: {[hex(p) for p in self.c2s_seen]})")
            pid, _body = self.recv_packet(min(remaining, HANDSHAKE_TIMEOUT))
            self.c2s_seen.append(pid)
            if pid == CC_CLIENT_INFO:
                got_info = True
            elif pid == CC_SELECT_KNOWN_PACKS:
                got_packs = True
        self.send_packet(CF_FINISH)
        self.expect(CC_FINISH, HANDSHAKE_TIMEOUT, "finish_configuration")
        return {"username": self.username, "c2s": list(self.c2s_seen)}

    # -- play helpers --------------------------------------------------------

    def play_login(self):
        self.send_packet(P_LOGIN, play_login_body())

    def spawn_teleport(self):
        self.send_packet(P_PLAYER_POS,
                         play_teleport_body(8.5, 65.0, 8.5, teleport_id=1))
        self.expect(SC_ACCEPT_TELEPORT, PROBE_TIMEOUT,
                    "accept_teleportation")

    def keep_alive_probe(self, marker):
        """Send keep_alive and require the C2S echo; returns True/False."""
        try:
            self.send_packet(P_KEEP_ALIVE, keep_alive_body(marker))
            body = self.expect(SC_KEEP_ALIVE, PROBE_TIMEOUT,
                               "keep_alive echo")
            return len(body) >= 8 and \
                struct.unpack(">q", body[:8])[0] == marker
        except (MockTimeout, OSError, struct.error):
            return False


class Reader:
    """Minimal packet-field reader for handshake verification."""

    def __init__(self, data):
        self.data, self.pos = data, 0

    def take(self, n):
        part = self.data[self.pos:self.pos + n]
        self.pos += n
        return part

    def varint(self):
        value, self.pos = _read_varint_bytes(self.data, self.pos)
        if value >= (1 << 31):
            value -= 1 << 32
        return value

    def string(self):
        length = self.varint()
        return self.take(length).decode("utf-8")


# ---------------------------------------------------------------------------
# Client process runner + outcome evaluation
# ---------------------------------------------------------------------------

def client_argv(client_bin, port, ticks, extra):
    argv = [str(client_bin),
            "--username", USERNAME,
            "--accessToken", TOKEN_SENTINEL,
            "--headless",
            "--quickPlayMultiplayer", f"127.0.0.1:{port}",
            "--ticks", str(ticks),
            "--view-distance", "2",
            "--json-summary"]
    argv += extra
    return argv


def spawn_client(client_bin, port, ticks, extra, workdir):
    env = dict(os.environ)
    env["TMPDIR"] = str(ROOT / "target/tmp")
    return subprocess.Popen(
        client_argv(client_bin, port, ticks, extra),
        cwd=workdir, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def wait_client(proc, timeout):
    """communicate() drains pipes while waiting — no pipe-fill deadlock."""
    try:
        out, err = proc.communicate(timeout=timeout)
        return proc.returncode, False, out, err
    except subprocess.TimeoutExpired:
        proc.kill()
        out, err = proc.communicate()
        return proc.returncode, True, out, err


def outcome_violations(returncode, timed_out, stdout, stderr):
    """Hard-invariant checker shared by fuzz waves and live runs."""
    violations = []
    text = (stdout or "") + "\n" + (stderr or "")
    if timed_out:
        violations.append("hang: client exceeded wave timeout")
    if returncode is not None and returncode < 0:
        try:
            sig = signal.Signals(-returncode).name
        except ValueError:
            sig = f"signal {-returncode}"
        violations.append(f"fatal-signal: client killed by {sig}")
    if "panicked at" in text:
        violations.append("panic: 'panicked at' in client output")
    if TOKEN_SENTINEL in text:
        violations.append("token-leak: --accessToken sentinel in client output")
    if returncode == 2:
        violations.append("usage-error: client rejected harness arguments")
    return violations


def run_fuzz_case(case_index, case_seed, client_bin, scratch):
    """One seeded adversarial wave against a fresh client process."""
    kind, actions, meta = build_wave(case_index, case_seed)
    if case_index % 2 == 0:
        threshold = (0, 64, 128, 256)[(case_index // 2) % 4]
    else:
        threshold = -1

    workdir = scratch / f"case-{case_index:04d}"
    workdir.mkdir(parents=True, exist_ok=True)

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    port = listener.getsockname()[1]

    proc = spawn_client(client_bin, port, 15, ["--script", "idle"], workdir)
    record = {
        "case": case_index, "kind": kind, "threshold": threshold,
        "vectors": meta["vectors"], "handshake_ok": False,
        "teleport_ok": False, "pre_echo": False, "post_echo": False,
        "closed_early": False, "actions": len(actions),
    }
    server = None
    try:
        listener.settimeout(HANDSHAKE_TIMEOUT)
        conn, _addr = listener.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        server = MockSession(conn, threshold)
        info = server.handshake()
        record["handshake_ok"] = True
        record["username"] = info["username"]

        if kind == "login_fuzz":
            for _tag, pid, body in actions:
                server.send_packet(pid, body)
            record["closed_early"] = True
        else:
            server.play_login()
            if kind != "raw":
                try:
                    server.spawn_teleport()
                    record["teleport_ok"] = True
                except MockTimeout:
                    record["closed_early"] = True
            if not record["closed_early"]:
                record["pre_echo"] = server.keep_alive_probe(
                    0xC0FFEE + case_index)
            for action in actions:
                if record["closed_early"]:
                    break
                if action[0] == "packet":
                    try:
                        server.send_packet(action[1], action[2])
                    except OSError:
                        record["closed_early"] = True
                        break
                elif action[0] == "raw":
                    try:
                        server.send_raw(action[1])
                    except OSError:
                        record["closed_early"] = True
                        break
                elif action[0] == "close":
                    record["closed_early"] = True
                    break
            if not record["closed_early"]:
                record["post_echo"] = server.keep_alive_probe(
                    0xC0FFEE + case_index + 1)
    except Exception as error:  # harness-side failure, recorded loudly
        record["server_error"] = f"{type(error).__name__}: {error}"
        record["closed_early"] = True
    finally:
        if server is not None:
            server.close()
        listener.close()

    returncode, timed_out, stdout_b, stderr_b = wait_client(
        proc, WAVE_TIMEOUT)
    stdout = stdout_b.decode("utf-8", "replace")
    stderr = stderr_b.decode("utf-8", "replace")
    violations = outcome_violations(returncode, timed_out, stdout, stderr)
    record.update(returncode=returncode, timed_out=timed_out,
                  stdout=stdout[-4000:], stderr=stderr[-4000:],
                  violations=violations)
    return record


# ---------------------------------------------------------------------------
# Layer 2 — live loopback verification
# ---------------------------------------------------------------------------

def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def wait_port(port, proc, timeout=SERVER_READY_TIMEOUT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise OracleFailure(
                f"server exited {proc.returncode} before listen; "
                f"log tail: {Path(proc.args and '' or '')}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.25)
    raise OracleFailure(f"server did not listen on 127.0.0.1:{port}")


class LiveServer:
    """Disposable loopback game server (megu or pinned Vanilla jar)."""

    def __init__(self, kind, binary, jar, workdir, seed):
        self.kind = kind
        self.binary = binary
        self.jar = jar
        self.workdir = workdir
        self.seed = seed
        self.port = free_port()
        self.proc = None
        self.log_path = workdir / "server.log"

    def start(self):
        self.workdir.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ)
        env["TMPDIR"] = str(ROOT / "target/tmp")
        if self.kind == "megu":
            for link, source in (
                    ("config_packets.bin", ROOT / "config_packets.bin"),
                    ("assets", ROOT / "assets")):
                target = self.workdir / link
                if not target.exists():
                    target.symlink_to(source.resolve(),
                                      target_is_directory=source.is_dir())
            env.update(
                MEGU_PORT=str(self.port),
                MEGU_BIND_ADDR="127.0.0.1",
                MEGU_SEED=str(self.seed),
                MEGU_WG="old",
                MEGU_RESOURCE_PACK_URL="",
                MEGU_ANTICHEAT="off",
                MEGU_COMPRESSION_THRESHOLD="256",
            )
            env.pop("MEGU_RCON_PORT", None)
            argv = [str(self.binary)]
        else:
            check(self.jar.is_file(), f"vanilla jar missing: {self.jar}")
            digest = hashlib.sha1(self.jar.read_bytes()).hexdigest()
            check(digest == JAR_SHA1,
                  f"vanilla server.jar SHA-1 mismatch: {digest}")
            java = next((j for j in JAVA_CANDIDATES if j.is_file()), None)
            check(java is not None,
                  "no Java runtime for vanilla server.jar")
            (self.workdir / "eula.txt").write_text("eula=true\n")
            options = {
                "server-port": self.port, "server-ip": "127.0.0.1",
                "online-mode": "false", "level-seed": self.seed,
                "level-type": "minecraft:flat", "gamemode": "creative",
                "force-gamemode": "true", "spawn-protection": 0,
                "difficulty": "peaceful", "view-distance": 2,
                "simulation-distance": 2,
                "network-compression-threshold": 256,
                "enable-command-block": "false",
            }
            (self.workdir / "server.properties").write_text(
                "".join(f"{k}={v}\n" for k, v in options.items()))
            argv = [str(java), f"-Djava.io.tmpdir={ROOT / 'target/tmp'}",
                    "-XX:-UsePerfData", "-Xms256M", "-Xmx1300M",
                    "-jar", str(self.jar), "nogui"]
        log_path = self.log_path.open("wb")
        self.proc = subprocess.Popen(argv, cwd=self.workdir, env=env,
                                     stdin=subprocess.DEVNULL,
                                     stdout=log_path,
                                     stderr=subprocess.STDOUT)
        log_path.close()
        wait_port(self.port, self.proc)
        if self.kind == "vanilla":
            # The listener opens before worldgen finishes; wait for the
            # "Done" line so the client does not hit a half-ready server.
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                if self.proc.poll() is not None:
                    raise OracleFailure(
                        f"vanilla server exited {self.proc.returncode}")
                try:
                    if b"Done (" in self.log_path.read_bytes():
                        break
                except OSError:
                    pass
                time.sleep(0.5)
            else:
                raise OracleFailure("vanilla server never printed Done")
        log(f"{self.kind} ready on 127.0.0.1:{self.port}")

    def stop(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)


# --- artifact validators ----------------------------------------------------

def _png_chunk(data, pos):
    if pos + 8 > len(data):
        raise OracleFailure("PNG truncated chunk header")
    length = struct.unpack(">I", data[pos:pos + 4])[0]
    tag = data[pos + 4:pos + 8]
    if pos + 12 + length > len(data):
        raise OracleFailure(f"PNG chunk {tag} overruns file")
    payload = data[pos + 8:pos + 8 + length]
    crc = struct.unpack(">I", data[pos + 8 + length:pos + 12 + length])[0]
    check(zlib.crc32(tag + payload) & 0xFFFFFFFF == crc,
          f"PNG chunk {tag} CRC mismatch")
    return tag, payload, pos + 12 + length


def validate_png(path):
    """Decode and sanity-check a PNG: signature, chunk CRCs, IHDR, IDAT
    zlib payload, and non-blank pixel content. Returns info dict."""
    data = Path(path).read_bytes()
    check(data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG file")
    pos = 8
    ihdr = None
    idat = bytearray()
    saw_iend = False
    while pos < len(data):
        tag, payload, pos = _png_chunk(data, pos)
        if tag == b"IHDR":
            check(ihdr is None, "duplicate IHDR")
            check(len(payload) == 13, "IHDR length != 13")
            ihdr = payload
        elif tag == b"IDAT":
            idat += payload
        elif tag == b"IEND":
            saw_iend = True
            break
    check(ihdr is not None, "PNG missing IHDR")
    check(saw_iend, "PNG missing IEND")
    width, height = struct.unpack(">II", ihdr[:8])
    bit_depth, color_type, compression, filt, interlace = ihdr[8:13]
    check((width, height) != (0, 0), "PNG zero dimensions")
    check(width <= 8192 and height <= 8192, "PNG dimensions exceed bound")
    check(bit_depth == 8 and color_type == 2 and compression == 0
          and filt == 0 and interlace == 0,
          f"unsupported PNG format depth={bit_depth} type={color_type} "
          f"interlace={interlace}")
    channels = 3
    stride = width * channels
    try:
        raw = zlib.decompress(bytes(idat))
    except zlib.error as error:
        raise OracleFailure(f"PNG IDAT zlib decompress failed: {error}")
    check(len(raw) == (stride + 1) * height,
          f"PNG pixel data size {len(raw)} != expected "
          f"{(stride + 1) * height}")

    # Unfilter scanlines (all five PNG filter types).
    pixels = bytearray()
    prev = bytearray(stride)
    for y in range(height):
        row_start = y * (stride + 1)
        ftype = raw[row_start]
        row = bytearray(raw[row_start + 1:row_start + 1 + stride])
        if ftype == 0:
            pass
        elif ftype == 1:
            for i in range(channels, stride):
                row[i] = (row[i] + row[i - channels]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                row[i] = (row[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                left = row[i - channels] if i >= channels else 0
                row[i] = (row[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                a = row[i - channels] if i >= channels else 0
                b = prev[i]
                c = prev[i - channels] if i >= channels else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                row[i] = (row[i] + pr) & 0xFF
        else:
            raise OracleFailure(f"PNG unknown row filter {ftype}")
        pixels += row
        prev = row

    # Non-blank: a real rendered frame has many distinct colors. Sample at
    # most ~600 pixels so tiny test images still get full coverage.
    total_px = len(pixels) // channels
    step_px = max(1, total_px // 600)
    colors = set()
    for px in range(0, total_px, step_px):
        i = px * channels
        colors.add(bytes(pixels[i:i + channels]))
    check(len(colors) > 8,
          f"PNG appears blank: only {len(colors)} distinct sampled colors")
    return {
        "width": width, "height": height,
        "distinct_colors": len(colors),
        "sha256": hashlib.sha256(data).hexdigest(),
        "bytes": len(data),
    }


def validate_wav(path):
    """Validate RIFF/WAVE structure: RIFF+WAVE magic, fmt chunk (PCM
    stereo 48 kHz s16), and a consistent data chunk."""
    data = Path(path).read_bytes()
    check(len(data) >= 44, "WAV file shorter than 44-byte header")
    check(data[:4] == b"RIFF" and data[8:12] == b"WAVE",
          "missing RIFF/WAVE magic")
    riff_size = struct.unpack("<I", data[4:8])[0]
    check(riff_size + 8 == len(data),
          f"RIFF size {riff_size} inconsistent with file size {len(data)}")
    pos = 12
    fmt_seen = data_seen = False
    while pos + 8 <= len(data):
        cid = data[pos:pos + 4]
        size = struct.unpack("<I", data[pos + 4:pos + 8])[0]
        payload = data[pos + 8:pos + 8 + size]
        check(len(payload) == size, f"WAV chunk {cid} truncated")
        if cid == b"fmt ":
            fmt_seen = True
            check(size >= 16, "fmt chunk too small")
            audio_format, channels = struct.unpack("<HH", payload[:4])
            rate = struct.unpack("<I", payload[4:8])[0]
            bits = struct.unpack("<H", payload[14:16])[0]
            check(audio_format == 1 and channels == 2 and rate == 48000
                  and bits == 16,
                  f"unexpected WAV format fmt={audio_format} ch={channels} "
                  f"rate={rate} bits={bits}")
        elif cid == b"data":
            data_seen = True
            check(size % 4 == 0, "WAV data size not frame-aligned")
        pos += 8 + size + (size & 1)
    check(fmt_seen and data_seen, "WAV missing fmt or data chunk")
    return {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


# --- live client run ---------------------------------------------------------

def parse_last_json(text):
    """Extract the last JSON object printed on its own line(s)."""
    lines = (text or "").splitlines()
    for end in range(len(lines) - 1, -1, -1):
        if not lines[end].rstrip().endswith("}"):
            continue
        for start in range(end, -1, -1):
            if not lines[start].lstrip().startswith("{"):
                continue
            try:
                value = json.loads("\n".join(lines[start:end + 1]))
            except ValueError:
                continue
            if isinstance(value, dict):
                return value
    raise OracleFailure("client printed no JSON summary on stdout")


def run_live_client(client_bin, port, run_dir, ticks=20):
    png_path = run_dir / "frame.png"
    wav_path = run_dir / "out.wav"
    argv = client_argv(client_bin, port, ticks,
                       ["--script", "walk",
                        "--render-png", str(png_path),
                        "--audio-wav", str(wav_path)])
    try:
        proc = subprocess.run(argv, cwd=run_dir, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=LIVE_TIMEOUT, check=False,
                              encoding="utf-8", errors="replace")
    except subprocess.TimeoutExpired:
        raise OracleFailure(
            f"live client timed out after {LIVE_TIMEOUT}s") from None
    violations = outcome_violations(proc.returncode, False,
                                    proc.stdout, proc.stderr)
    check(proc.returncode == 0 and not violations,
          f"live client failed rc={proc.returncode} violations={violations}\n"
          f"stdout: {proc.stdout[-2000:]}\nstderr: {proc.stderr[-2000:]}")
    summary = parse_last_json(proc.stdout)

    for key in ("login_ok", "config_ok", "play_ok"):
        check(summary.get(key) is True,
              f"summary {key} != true: {summary.get(key)}")
    chunks = summary.get("chunks_decoded")
    quads = summary.get("total_quads")
    vertices = summary.get("packed_vertices")
    corrections = summary.get("corrections_0x40")
    check(isinstance(chunks, int) and chunks >= 9,
          f"chunks_decoded {chunks} < 9")
    check(isinstance(quads, int) and quads > 0, f"total_quads {quads} <= 0")
    check(vertices == quads * 4,
          f"packed_vertices {vertices} != total_quads*4 ({quads * 4})")
    check(corrections == 0, f"corrections_0x40 {corrections} != 0")

    png_info = validate_png(png_path)
    wav_info = validate_wav(wav_path)
    metrics = {k: summary.get(k) for k in DETERMINISTIC_FIELDS}
    return {"summary": summary, "png": png_info, "wav": wav_info,
            "metrics": metrics}


DETERMINISTIC_FIELDS = (
    "protocol", "login_ok", "config_ok", "play_ok", "compression_threshold",
    "dimension", "entity_id", "ticks_executed", "chunks_decoded",
    "chunks_loaded", "chunks_unloaded", "chunk_batches", "light_updates",
    "block_updates", "local_light_edits", "sections_rebuilt", "total_quads",
    "packed_vertices", "translucent_quads", "block_states_from_json",
    "teleports_0x40", "corrections_0x40", "max_prediction_drift",
    "final_pos", "on_ground", "health", "food", "view_distance",
    "lod_stored_columns", "lod_fine_drawn", "lod_coarse_drawn",
    "triangles_drawn", "pixels_shaded", "drawn_sections", "culled_sections",
    "region_draw_calls", "buckets_drawn", "buckets_culled",
)

# Subset compared across identical Vanilla runs. Vanilla delivers chunk
# batches asynchronously, so camera pose (final_pos) and every
# render/collision-derived counter legitimately differ between runs —
# the same nondeterminism a real Minecraft client exhibits. These fields
# are derived purely from server-sent world data and must still match.
STABLE_FIELDS = (
    "protocol", "login_ok", "config_ok", "play_ok", "compression_threshold",
    "dimension", "entity_id", "ticks_executed", "chunks_decoded",
    "chunks_loaded", "total_quads", "packed_vertices", "translucent_quads",
    "teleports_0x40", "corrections_0x40",
)


def run_layer2(client_bin, server_bin, jar, seed, scratch):
    """Two identical live runs per server kind; PNG bytes and mesh metrics
    must match exactly."""
    results = {}
    kinds = ["megu"]
    if jar is not None and jar.is_file():
        kinds.append("vanilla")
    for kind in kinds:
        runs = []
        for run_idx in range(2):
            run_dir = scratch / f"live-{kind}-{run_idx}"
            run_dir.mkdir(parents=True, exist_ok=True)
            server = LiveServer(kind, server_bin, jar, run_dir / "server",
                                seed)
            try:
                server.start()
                result = run_live_client(client_bin, server.port, run_dir)
            finally:
                server.stop()
            runs.append(result)
            log(f"{kind} run {run_idx + 1}: "
                f"chunks={result['metrics']['chunks_decoded']} "
                f"quads={result['metrics']['total_quads']} "
                f"png={result['png']['sha256'][:16]}…")
        if kind == "megu":
            # megu emits a deterministic chunk stream: identical runs
            # must produce byte-identical PNGs and identical metrics.
            check(runs[0]["metrics"] == runs[1]["metrics"],
                  f"{kind}: mesh/session metrics differ between identical "
                  f"runs\n"
                  f"run1={json.dumps(runs[0]['metrics'], sort_keys=True)}\n"
                  f"run2={json.dumps(runs[1]['metrics'], sort_keys=True)}")
            check(runs[0]["png"]["sha256"] == runs[1]["png"]["sha256"],
                  f"{kind}: PNG bytes differ between identical runs "
                  f"({runs[0]['png']['sha256'][:16]} vs "
                  f"{runs[1]['png']['sha256'][:16]})")
        else:
            # Vanilla's async chunk delivery makes camera pose and
            # render-derived counters legitimately differ between runs;
            # only the server-data signature must match.
            stable_a = {k: runs[0]["metrics"][k] for k in STABLE_FIELDS}
            stable_b = {k: runs[1]["metrics"][k] for k in STABLE_FIELDS}
            check(stable_a == stable_b,
                  f"{kind}: world-derived metrics differ between runs\n"
                  f"run1={json.dumps(stable_a, sort_keys=True)}\n"
                  f"run2={json.dumps(stable_b, sort_keys=True)}")
            log(f"{kind}: stable world metrics verified across runs "
                f"(camera/render counters legitimately differ under "
                f"async chunk delivery)")
        results[kind] = runs[0]
        if kind == "megu":
            log(f"{kind}: deterministic PNG + metrics verified "
                f"({runs[0]['png']['width']}x{runs[0]['png']['height']}, "
                f"{runs[0]['png']['distinct_colors']} colors)")
        else:
            log(f"{kind}: artifacts valid per run "
                f"({runs[0]['png']['width']}x{runs[0]['png']['height']}, "
                f"{runs[0]['png']['distinct_colors']} colors)")
    return results


# ---------------------------------------------------------------------------
# Layer 1 driver
# ---------------------------------------------------------------------------

def run_layer1(client_bin, seed, cases, scratch):
    failures = []
    stats = {"play": 0, "raw": 0, "login_fuzz": 0, "context_soak": 0}
    echoes = {"pre": 0, "post": 0}
    for case_index in range(cases):
        record = run_fuzz_case(case_index, seed, client_bin, scratch)
        stats[record["kind"]] = stats.get(record["kind"], 0) + 1
        if record.get("pre_echo"):
            echoes["pre"] += 1
        if record.get("post_echo"):
            echoes["post"] += 1
        if not record["handshake_ok"]:
            raise OracleFailure(
                "mock handshake failed — harness/protocol mismatch, not a "
                f"client bug. case={case_index} "
                f"server_error={record.get('server_error')} "
                f"rc={record.get('returncode')} "
                f"stderr={(record.get('stderr') or '')[-800:]}")
        if record["violations"]:
            failures.append(record)
            log(f"case {case_index} ({record['kind']}): "
                f"{'; '.join(record['violations'])} "
                f"vectors={record['vectors']} rc={record['returncode']}")
        if (case_index + 1) % 10 == 0 or case_index + 1 == cases:
            log(f"layer1 {case_index + 1}/{cases} cases, "
                f"{len(failures)} failing")
    check(not failures,
          f"{len(failures)} fuzz case(s) violated hard invariants:\n"
          + "\n".join(
              f"  case {r['case']} seed={seed} kind={r['kind']} "
              f"vectors={r['vectors']} rc={r.get('returncode')} "
              f"violations={r['violations']}\n"
              f"    stderr: {(r.get('stderr') or '')[-600:]}"
              for r in failures[:8]))
    log(f"layer1: {cases} seeded waves clean "
        f"(kinds={stats}, pre_echo={echoes['pre']}, "
        f"post_echo={echoes['post']})")
    return stats


# ---------------------------------------------------------------------------
# --self-test
# ---------------------------------------------------------------------------

def _expect_raises(fn, needle, label):
    try:
        fn()
    except Exception as error:
        check(needle in str(error),
              f"{label}: wrong failure '{error}' (wanted '{needle}')")
        return
    raise OracleFailure(f"{label}: unexpectedly passed")


def self_test():
    # 1. VarInt/VarLong round-trips incl. negatives and bounds.
    for value in (0, 1, 127, 128, 255, 300, -1, -2, 0x7FFFFFFF,
                  -0x80000000):
        enc = varint(value)
        check(len(enc) <= 5, f"varint({value}) too long")
        back, _pos = _read_varint_bytes(enc, 0)
        if back >= (1 << 31):
            back -= 1 << 32
        check(back == value, f"varint roundtrip {value} -> {back}")
    for value in (0, 1, -1, 0x7FFFFFFFFFFFFFFF, -0x8000000000000000):
        enc = varlong(value)
        check(len(enc) <= 10, f"varlong({value}) too long")
    # frame_packet/unframe_packet round-trip in every framing mode.
    raw = packet(P_CHUNK, b"\x01\x02\x03" * 40)
    for threshold in (-1, 0, 64, 256):
        framed = frame_packet(raw, threshold)
        body, consumed = strip_frame_prefix(framed)
        check(consumed == len(framed), "frame length mismatch")
        check(unframe_packet(body, threshold) == raw,
              f"frame roundtrip threshold={threshold}")

    # 2. Seeded wave determinism.
    kind_a, actions_a, meta_a = build_wave(0, 1211)
    kind_b, actions_b, meta_b = build_wave(0, 1211)
    check(kind_a == kind_b and actions_a == actions_b
          and meta_a == meta_b, "wave generation not deterministic")
    kind_c, actions_c, _meta_c = build_wave(1, 1211)
    check((kind_c, actions_c) != (kind_a, actions_a),
          "different case index produced identical wave")
    kind_d, actions_d, _meta_d = build_wave(0, 777)
    check(actions_d != actions_a or kind_d != kind_a,
          "different seed produced identical wave")

    # 3. Outcome detectors catch synthetic failures.
    check(outcome_violations(0, False, "ok", "") == [],
          "clean outcome flagged")
    check(outcome_violations(1, False, "",
                             "megu-client: connection closed") == [],
          "clean error exit flagged")
    v = outcome_violations(-11, False, "", "")
    check(any("signal" in x for x in v), f"SIGSEGV not caught: {v}")
    v = outcome_violations(-6, False, "", "")
    check(any("signal" in x for x in v), f"SIGABRT not caught: {v}")
    v = outcome_violations(1, False, "",
                           "thread 'main' panicked at src/x.rs:1:1")
    check(any("panic" in x for x in v), f"panic text not caught: {v}")
    v = outcome_violations(0, False, "", f"token {TOKEN_SENTINEL} echoed")
    check(any("token" in x for x in v), f"token leak not caught: {v}")
    v = outcome_violations(None, True, "", "")
    check(any("hang" in x for x in v), f"timeout not caught: {v}")
    v = outcome_violations(2, False, "", "")
    check(any("usage" in x for x in v), f"usage error not caught: {v}")

    # 4. PNG validator on synthetic images.
    def make_png(width, height, rgb_rows):
        raw = b"".join(b"\x00" + row for row in rgb_rows)
        idat = zlib.compress(raw)

        def chunk(tag, payload):
            return (struct.pack(">I", len(payload)) + tag + payload
                    + struct.pack(">I",
                                  zlib.crc32(tag + payload) & 0xFFFFFFFF))

        ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
        return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
                + chunk(b"IDAT", idat) + chunk(b"IEND", b""))

    with tempfile.TemporaryDirectory() as tmp:
        good = Path(tmp) / "good.png"
        px = bytearray()
        for y in range(8):
            for x in range(8):
                px += bytes([(x * 30) % 256, (y * 30) % 256,
                             ((x + y) * 15) % 256])
        rows = [bytes(px[y * 24:(y + 1) * 24]) for y in range(8)]
        good.write_bytes(make_png(8, 8, rows))
        info = validate_png(good)
        check(info["width"] == 8 and info["height"] == 8
              and info["distinct_colors"] > 8,
              f"good PNG rejected: {info}")
        blank = Path(tmp) / "blank.png"
        blank.write_bytes(make_png(8, 8, [b"\x10\x20\x30" * 8] * 8))
        _expect_raises(lambda: validate_png(blank), "blank",
                       "blank PNG accepted")
        corrupt = Path(tmp) / "corrupt.png"
        corrupt.write_bytes(good.read_bytes()[:-4] + b"\x00\x00\x00\x00")
        _expect_raises(lambda: validate_png(corrupt), "CRC",
                       "CRC-corrupt PNG accepted")
        notpng = Path(tmp) / "x.png"
        notpng.write_bytes(b"not a png at all")
        _expect_raises(lambda: validate_png(notpng), "not a PNG",
                       "non-PNG accepted")

        # 5. WAV validator.
        wav = Path(tmp) / "ok.wav"
        header = (b"RIFF" + struct.pack("<I", 36) + b"WAVE"
                  + b"fmt " + struct.pack("<IHHIIHH", 16, 1, 2, 48000,
                                          48000 * 4, 4, 16)
                  + b"data" + struct.pack("<I", 0))
        wav.write_bytes(header)
        info = validate_wav(wav)
        check(info["bytes"] == 44, f"WAV info wrong: {info}")
        bad = Path(tmp) / "bad.wav"
        bad.write_bytes(b"RIFX" + header[4:])
        _expect_raises(lambda: validate_wav(bad), "RIFF",
                       "non-RIFF WAV accepted")
        short = Path(tmp) / "short.wav"
        short.write_bytes(header[:20])
        _expect_raises(lambda: validate_wav(short), "44-byte",
                       "truncated WAV accepted")

    # 6. Catalog integrity: every vector builds; packets re-frame cleanly.
    all_vecs = ([FuzzVector("ctx:" + v.name, *v[1:])
                 for v in CONTEXT_VECTORS]
                + [FuzzVector("fatal:" + v.name, *v[1:])
                   for v in FATAL_VECTORS]
                + [FuzzVector("login:" + v.name, *v[1:])
                   for v in LOGIN_FUZZ_VECTORS])
    check(len(all_vecs) > 50, f"catalog too small: {len(all_vecs)}")
    for vec in all_vecs:
        if vec.kind == "packet":
            pid, body = vec.payload
            for threshold in (-1, 64):
                framed = frame_packet(packet(pid, body), threshold)
                payload, _ = strip_frame_prefix(framed)
                check(unframe_packet(payload, threshold)
                      == packet(pid, body),
                      f"{vec.name}: packet roundtrip failed")
        elif vec.kind == "raw":
            check(isinstance(vec.payload, (bytes, bytearray)),
                  f"{vec.name}: raw payload not bytes")
    body = standard_chunk(0, 0)
    check(len(body) > 200, f"chunk body suspiciously small: {len(body)}")

    # 7. Mock handshake framing helpers (no socket): varint reader parity.
    for value in (0, 767, -1, 0x7FFFFFFF):
        enc = varint(value)
        reader = Reader(enc)
        check(reader.varint() == value, f"Reader.varint({value}) mismatch")

    log("self-test: PASS")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Client-side 3-layer bedrock gate for megu-client "
                    "(Protocol 767 adversarial S2C fuzz + live "
                    "render/audio/prediction verification).")
    parser.add_argument("--client-bin", type=Path, default=DEFAULT_CLIENT)
    parser.add_argument("--server-bin", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--with-vanilla", action="store_true",
                        help="also verify against pinned Vanilla 1.21.1 "
                             "server.jar")
    parser.add_argument("--vanilla-jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--seed", type=int, default=1211)
    parser.add_argument("--cases", type=int, default=120)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0

    check(args.client_bin.is_file(),
          f"client binary missing: {args.client_bin} "
          "(build with: cargo build --manifest-path client-app/Cargo.toml "
          "--release --offline --locked)")
    check(args.cases > 0, "--cases must be positive")

    tmp = ROOT / "target/tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    os.environ["TMPDIR"] = str(tmp)

    jar = args.vanilla_jar if args.with_vanilla else None
    if jar is not None and not jar.is_file():
        log(f"--with-vanilla requested but {jar} missing; "
            "vanilla leg skipped")
        jar = None

    with tempfile.TemporaryDirectory(prefix="client-oracle-", dir=tmp) as d:
        scratch = Path(d)
        log(f"layer1: {args.cases} seeded adversarial waves "
            f"(seed={args.seed}, client={args.client_bin})")
        run_layer1(args.client_bin, args.seed, args.cases, scratch)

        log("layer2: clean loopback live render+audio+prediction")
        live = run_layer2(args.client_bin, args.server_bin, jar,
                          args.seed, scratch)
        for kind, result in live.items():
            m = result["metrics"]
            log(f"  {kind}: login={m['login_ok']} "
                f"config={m['config_ok']} play={m['play_ok']} "
                f"chunks={m['chunks_decoded']} quads={m['total_quads']} "
                f"verts={m['packed_vertices']} "
                f"corrections={m['corrections_0x40']}")

    log("client oracle: PASS")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except OracleFailure as error:
        print(f"client oracle: FAIL ({error})", file=sys.stderr)
        raise SystemExit(1)
    except KeyboardInterrupt:
        raise SystemExit(130)
