#!/usr/bin/env python3
"""Vanilla 1.21.1 differential survival+creative parity & save-restart matrix.

Drives one deterministic scenario script against ``mc-spine`` and, with
``--with-vanilla``, the pinned official ``server.jar`` (SHA-1
59353fb40c36d304f2035d51e7d6e6baa98dc05c). Each target runs a full
Save-v2 stop -> relaunch -> fresh-client reconnect so persisted world,
sign, container, inventory and gamemode state is verified, not just RAM.

The wire codec is reused from ``tests/protocol767.py``,
``tools/client_diff/parity_client.py``, ``tests/container_wire.py`` and
``tests/gp3_gp4_gp5_wire.py``; nothing here re-implements packet formats.
All coordinates are recorded relative to a per-run arena origin so the two
servers' different spawn points compare equal.
"""

import contextlib
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
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/client_diff"))
sys.path.insert(0, str(ROOT / "tests"))

from parity_client import (JAVA, JAR, JAR_SHA1, NAME as PROBE_NAME, RCON_PASS,  # noqa: E402
                           ParityClient, command, offline_uuid, read_for, until)
from protocol767 import (AIR_STATES, WIRE_BLOCKS, Reader, position,  # noqa: E402
                         signed, unpack_position, vint, palette, STATE_COUNT)
from gp3_gp4_gp5_wire import block_properties  # noqa: E402
from wire_smoke import port, rcon_frame, rcon_response  # noqa: E402

CLIENT_NAME = "MatrixProbe"
SEED = 4242
SIGN_LINES = ["Oracle", "Second", "Third", ""]

BLOCKS = {b["name"]: b for b in WIRE_BLOCKS}
STATE_TO_NAME = {}
for _b in WIRE_BLOCKS:
    for _s in range(_b["minStateId"], _b["maxStateId"] + 1):
        STATE_TO_NAME[_s] = _b["name"]


def item_id(block_name):
    return BLOCKS[block_name]["drops"][0]


ITEM = {
    "stone": 1, "dirt": 28, "oak_planks": 36, "cobblestone": 35,
    "torch": item_id("torch"), "lantern": item_id("lantern"),
    "rail": item_id("rail"), "poppy": item_id("poppy"),
    "oak_sign": item_id("oak_sign"), "oak_door": item_id("oak_door"),
    "oak_trapdoor": item_id("oak_trapdoor"), "oak_fence_gate": item_id("oak_fence_gate"),
    "lever": item_id("lever"), "stone_button": item_id("stone_button"),
    "chest": item_id("chest"), "furnace": item_id("furnace"),
    "diamond_pickaxe": 840, "diamond_shovel": 839, "diamond_axe": 841,
    "coal": 803, "diamond_sword": 838,
}


def props_of(state):
    name = STATE_TO_NAME.get(state)
    return block_properties(name, state) if name else {}


def is_air(state):
    return state in AIR_STATES


# ---------------------------------------------------------------------------
# Slot / component codecs. Wire parsing uses the tolerant stack reader below
# (gp3's `stack` is intentionally strict and drops packets on components it
# does not know, e.g. damage type 3 emitted after tool use).
# ---------------------------------------------------------------------------

def slot_payload(item=0, count=0, components=()):
    """ItemStack wire form: empty is a lone VarInt 0; otherwise count, item id,
    added-component count, removed-component count, then (type varint + raw
    payload) per component. `components` is a list of (type_id, payload)."""
    if count == 0:
        return vint(0)
    out = vint(count) + vint(item) + vint(len(components)) + vint(0)
    for ty, payload in components:
        out += vint(ty) + payload
    return out


def nbt_string(text):
    raw = text.encode("utf-8")
    return b"\x08" + struct.pack(">H", len(raw)) + raw


def enchant_payload(entries, show=1):
    out = vint(len(entries))
    for enchant_id, level in entries:
        out += vint(enchant_id) + vint(level)
    return out + bytes([show])


def canon(value):
    """JSON-stable normalization for parsed stacks / NBT."""
    if isinstance(value, dict):
        return {str(k): canon(v) for k, v in sorted(value.items(), key=lambda kv: str(kv[0]))}
    if isinstance(value, (list, tuple)):
        return [canon(v) for v in value]
    if isinstance(value, float):
        return round(value, 6)
    if isinstance(value, (int, str, bool)) or value is None:
        return value
    return str(value)


def canon_stack(stack):
    item, count, components = stack
    return {"item": item, "count": count, "components": canon(components)}


EMPTY_STACK = (0, 0, {})


# ---------------------------------------------------------------------------
# Tolerant item-stack parser. gp3's `stack` is intentionally strict (it raises
# on any component outside its fixture set); live servers legitimately emit
# damage (3), custom_data (0), lore (7), attribute_modifiers (12), potion
# contents (31), armor_trim (35), banner_patterns (49) etc. This parser mirrors
# mc-spine's own `slot_read_component` skip table so every Protocol-767
# component payload is consumed correctly; kinds we can value cheaply are
# returned semantically, opaque kinds as their exact payload hex.
# ---------------------------------------------------------------------------

def _vint(r):
    return r.varint()


def _nbt(r):
    return r.nbt()


def _bool(r):
    return r.number("B") != 0


def _bytes(r, n):
    return r.take(n).hex()


def _string(r):
    return r.string()


def _optional_nbt(r):
    if r.number("B") == 0:
        return None
    return r.nbt()


def _holder_set(r):
    kind = r.varint()
    if kind == 0:
        r.string()
    else:
        for _ in range(kind - 1):
            r.varint()


def _sound_event(r):
    r.string()
    if _bool(r):
        r.take(4)


def _sound_holder(r):
    if r.varint() == 0:
        _sound_event(r)


def _block_predicate(r):
    if _bool(r):
        _holder_set(r)
    if _bool(r):
        for _ in range(r.varint()):
            r.string()
            if _bool(r):
                r.string()
            else:
                r.string()
                r.string()
    _optional_nbt(r)


def _potion_effect_details(r, depth=0):
    if depth == 16:
        raise ValueError("potion effect depth")
    r.varint()          # amplifier
    r.varint()          # duration
    for _ in range(3):
        _bool(r)        # ambient, visible, icon
    if _bool(r):
        _potion_effect_details(r, depth + 1)


def _opaque(r, start):
    return {"opaque": r.data[start:r.pos].hex()}


def _component_value(r, ty, depth):
    start = r.pos
    if ty == 0:
        return r.nbt()
    if ty in (1, 2, 3, 13, 16, 26, 28, 41, 50):
        return r.varint()
    if ty == 8:
        return r.varint()
    if ty in (4, 18):
        return _bool(r)
    if ty in (5, 6):
        return r.nbt()
    if ty == 7:
        return [_optional_nbt(r) for _ in range(r.varint())]
    if ty in (9, 23):
        entries = {}
        for _ in range(r.varint()):
            entries[r.varint()] = r.varint()
        return {"entries": entries, "tooltip": _bool(r)}
    if ty in (10, 11):
        for _ in range(r.varint()):
            _block_predicate(r)
        _bool(r)
        return _opaque(r, start)
    if ty == 12:  # attribute_modifiers
        n = r.varint()
        for _ in range(n):
            r.varint()              # attribute id
            id_len = r.varint()
            r.take(id_len)          # modifier uuid bytes
            r.take(8)               # amount f64
            r.varint()              # operation
            r.varint()              # slot group
        _bool(r)                    # show_in_tooltip
        return _opaque(r, start)
    if ty in (14, 15, 17, 21):
        return True                # unit components
    if ty in (19, 27, 36, 37, 38, 39, 43, 55, 56):
        return r.nbt()
    if ty == 20:  # food
        r.varint(); r.take(4); _bool(r); r.take(4)
        tolerant_stack(r, depth + 1)
        for _ in range(r.varint()):
            r.varint(); r.take(4)
        return _opaque(r, start)
    if ty == 22:  # recipes
        for _ in range(r.varint()):
            _holder_set(r)
            if _bool(r):
                r.take(4)
            if _bool(r):
                _bool(r)
        r.take(4)
        r.varint()
        return _opaque(r, start)
    if ty == 24:
        raw = r.take(4).hex()
        return {"bytes": raw, "flag": _bool(r)}
    if ty == 25:
        return r.take(4).hex()
    if ty in (29, 30, 52):
        return [tolerant_stack(r, depth + 1) for _ in range(r.varint())]
    if ty == 31:  # potion_contents
        if _bool(r):
            r.varint()
        if _bool(r):
            r.take(4)
        for _ in range(r.varint()):
            r.varint()
            _potion_effect_details(r)
        # optional_string: bool + string
        if _bool(r):
            r.string()
        return _opaque(r, start)
    if ty == 32:
        return [[r.varint(), r.varint()] for _ in range(r.varint())]
    if ty == 33:
        out = []
        for _ in range(r.varint()):
            s = r.string()
            extra = r.string() if _bool(r) else None
            out.append([s, extra])
        return out
    if ty == 34:  # writable_book_content
        r.string()
        if _bool(r):
            r.string()
        r.string()
        r.varint()
        for _ in range(r.varint()):
            r.nbt()
            _optional_nbt(r)
        _bool(r)
        return _opaque(r, start)
    if ty == 35:  # armor_trim
        if r.varint() == 0:
            r.string(); r.varint()
            for _ in range(r.varint()):
                r.varint(); r.string()
            r.nbt()
        if r.varint() == 0:
            r.string(); r.varint(); r.nbt(); _bool(r)
        _bool(r)
        return _opaque(r, start)
    if ty == 40:  # instrument
        if r.varint() == 0:
            _sound_holder(r); r.take(8); r.nbt()
        return _opaque(r, start)
    if ty == 42:  # jukebox_song
        if _bool(r):
            if r.varint() == 0:
                _sound_holder(r); r.nbt(); r.take(4); r.varint()
        else:
            r.string()
        _bool(r)
        return _opaque(r, start)
    if ty == 44:
        if _bool(r):
            r.string(); r.take(8)
        _bool(r)
        return _opaque(r, start)
    if ty == 45:
        if not (0 <= r.varint() <= 4):
            raise ValueError("firework shape")
        for _ in range(2):
            r.take(r.varint() * 4)
        _bool(r); _bool(r)
        return _opaque(r, start)
    if ty == 46:
        r.varint()
        for _ in range(r.varint()):
            _component_value(r, 45, depth)
        return _opaque(r, start)
    if ty == 47:
        if _bool(r):
            r.string()
        if _bool(r):
            r.take(16)
        for _ in range(r.varint()):
            r.string(); r.string()
            if _bool(r):
                r.string()
        return _opaque(r, start)
    if ty == 48:
        return r.string()
    if ty == 49:  # banner_patterns
        for _ in range(r.varint()):
            if r.varint() == 0:
                r.string(); r.string()
            r.varint()
        return _opaque(r, start)
    if ty == 51:
        return [r.varint() for _ in range(r.varint())]
    if ty == 53:
        return [[r.string(), r.string()] for _ in range(r.varint())]
    if ty == 54:
        return [{"nbt": r.nbt(), "a": r.varint(), "b": r.varint()}
                for _ in range(r.varint())]
    if ty == 56:
        return r.nbt()
    raise ValueError(f"unknown component type {ty}")


def tolerant_stack(reader, depth=0):
    """Like gp3's `stack` but consumes every Protocol-767 component type."""
    if depth > 16:
        raise ValueError("stack nesting too deep")
    count = reader.varint()
    if not count:
        return (0, 0, {})
    item = reader.varint()
    added, removed = reader.varint(), reader.varint()
    components = {}
    for _ in range(added):
        ty = reader.varint()
        components[ty] = _component_value(reader, ty, depth)
    for _ in range(removed):
        reader.varint()
    return item, count, components


def _snbt_entries(text):
    """Top-level {...} compounds inside the first [...] of an SNBT reply."""
    i = text.find("[")
    if i < 0:
        return []
    entries, depth, start, in_str, esc = [], 0, -1, False, False
    for j in range(i + 1, len(text)):
        ch = text[j]
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
            continue
        if ch == '"':
            in_str = True
            continue
        if ch == "{":
            if depth == 0:
                start = j
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0 and start >= 0:
                entries.append(text[start:j + 1])
                start = -1
        elif ch == "]" and depth == 0:
            break
    return entries


def canonical_inv_slot(kind, slot):
    """Map a `/data get entity Inventory` Slot index to a functional location.
    mc-spine reports protocol window slots (hotbar 36-44, armor 5-8, offhand
    45); vanilla reports its internal indices (hotbar 0-8, armor 36-39,
    offhand 40). Both normalize to the same hotbar:/main:/armor:/offhand keys."""
    if kind == "vanilla":
        if 0 <= slot <= 8:
            return f"hotbar:{slot}"
        if 9 <= slot <= 35:
            return f"main:{slot - 9}"
        if 36 <= slot <= 39:
            return f"armor:{slot - 36}"
        if slot == 40 or slot == -106:
            return "offhand"
        return f"other:{slot}"
    if 36 <= slot <= 44:
        return f"hotbar:{slot - 36}"
    if 9 <= slot <= 35:
        return f"main:{slot - 9}"
    if 5 <= slot <= 8:
        return f"armor:{slot - 5}"
    if slot == 45:
        return "offhand"
    return f"other:{slot}"


def inventory_dataget(rcon_port, name, kind=None):
    """Authoritative player inventory via `/data get entity <name> Inventory`:
    {slot-or-functional-key: {"id": item-name, "count": n, "components": ...}}.
    With `kind` set, slot indices are canonicalized across server conventions."""
    try:
        out = command(rcon_port, f"data get entity {name} Inventory")
    except Exception:
        return None
    inv = {}
    for entry in _snbt_entries(out):
        ms = re.search(r'"?Slot"?\s*:\s*(\d+)', entry)
        mi = re.search(r'"?id"?\s*:\s*"([^"]+)"', entry)
        mc = re.search(r'"?count"?\s*:\s*(\d+)', entry)
        if not (ms and mi and mc):
            continue
        comps = None
        # vanilla emits unquoted SNBT keys (`components: {...}`), mc-spine
        # quotes them — accept both.
        km = re.search(r'"?components"?\s*:', entry)
        k = km.end() - 1 if km else -1
        if k >= 0:
            b = entry.find("{", k)
            if b >= 0:
                depth, instr, esc = 0, False, False
                for j in range(b, len(entry)):
                    ch = entry[j]
                    if instr:
                        if esc:
                            esc = False
                        elif ch == "\\":
                            esc = True
                        elif ch == '"':
                            instr = False
                        continue
                    if ch == '"':
                        instr = True
                    elif ch == "{":
                        depth += 1
                    elif ch == "}":
                        depth -= 1
                        if depth == 0:
                            # whitespace + SNBT quote style are serialization
                            # details, not item state — normalize both away
                            comps = re.sub(r"[\s\"]+", "",
                                           entry[b:j + 1])
                            break
        item_name = mi.group(1).split(":")[-1]
        key = canonical_inv_slot(kind, int(ms.group(1))) if kind else int(ms.group(1))
        inv[key] = {"id": item_name, "count": int(mc.group(1)),
                    "components": comps}
    return inv


def inventory_item_total(inv, item_name):
    if not inv:
        return 0
    return sum(s["count"] for s in inv.values() if s["id"] == item_name)


def settled_inventory(rcon_port, name, kind=None, timeout=15.0):
    """mc-spine refreshes the player `Inventory` NBT lazily (observed ~5s
    cadence after the triggering change), so a single read can be mid-cycle
    stale. Poll until two consecutive reads agree."""
    prev = inventory_dataget(rcon_port, name, kind)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(1.0)
        cur = inventory_dataget(rcon_port, name, kind)
        if cur == prev:
            return cur
        prev = cur
    return prev


def inventory_view_of(client):
    """Player-inventory view as {slot: canon_stack}. Prefers the window-0
    SetContainerContent (0x13) when the server sends one, overlaid with every
    SetContainerSlot (0x15) echo — mc-spine only emits the echoes, vanilla
    emits both, and the merged view is identical either way."""
    w0 = client.contents.get(0)
    base = w0["slots"] if w0 else []
    out = {}
    for i in range(46):
        if i in client.inv_echo:
            stack = client.inv_echo[i]
        elif i < len(base):
            stack = base[i]
        else:
            stack = EMPTY_STACK
        out[i] = canon_stack(stack)
    return out


def join_game_mode(body):
    """game_mode byte from Join Game (0x2b)."""
    r = Reader(body)
    r.number(">i")              # entity_id
    r.number("B")               # hardcore
    for _ in range(r.varint()):
        r.string()              # dimension names
    r.varint(); r.varint(); r.varint()   # max_players, view, simulation
    r.number("B"); r.number("B"); r.number("B")  # debug, respawn, limited crafting
    r.varint()                  # dimension_type
    r.string()                  # dimension_name
    r.number(">q")              # hashed_seed
    return r.number("B")        # game_mode


def parse_light_tail(reader, section_count, light_section_count, has_skylight):
    """Consume the light-mask + light-array tail of a 0x27 body exactly as
    protocol767.decode_chunk_light does. Returns True iff it lands on EOF."""
    max_mask_longs = (light_section_count + 63) // 64
    try:
        masks = []
        for _ in range(4):
            length = reader.varint()
            if length > max_mask_longs:
                return False
            mask = 0
            for word_idx in range(length):
                mask |= reader.number(">Q") << (64 * word_idx)
            if mask >= (1 << light_section_count):
                return False
            masks.append(mask)
        if masks[0] & masks[2] or masks[1] & masks[3]:
            return False
        if not has_skylight and masks[0] != 0:
            return False
        for mask in masks[:2]:
            length = reader.varint()
            if length != mask.bit_count():
                return False
            for _ in range(length):
                if reader.varint() != 2048:
                    return False
                reader.take(2048)
        return reader.pos == len(reader.data)
    except (AssertionError, IndexError, struct.error):
        return False


def chunk_block_entities(body, biome_count, section_count=24,
                         light_section_count=26, has_skylight=True):
    """Parse the block-entity array of a raw 0x27 body.

    Each entry is packedXZ u8 (x<<4|z, chunk-local), y i16 (absolute),
    type varint, then NBT. Both anonymous-NBT (tag byte + payload) and
    boolean-prefixed optional-NBT encodings are tried; the hypothesis whose
    light tail parses to EOF wins. Returns (entries, encoding) where entries
    are ((x,y,z), type, nbt)."""
    r = Reader(body)
    coord = (r.number(">i"), r.number(">i"))
    r.nbt()                       # heightmaps
    data = Reader(r.take(r.varint()))
    for _ in range(section_count):
        data.number(">h")
        palette(data, 4096, 8, 4, STATE_COUNT)
        palette(data, 64, 3, 1, biome_count)
    count = r.varint()
    base = r.pos

    def attempt(optional):
        rr = Reader(body)
        rr.pos = base
        entries = []
        for _ in range(count):
            packed = rr.take(1)[0]
            y = rr.number(">h")
            btype = rr.varint()
            lx, lz = packed >> 4, packed & 15
            pos = (coord[0] * 16 + lx, y, coord[1] * 16 + lz)
            if optional:
                present = rr.number("B")
                nbt = rr.nbt() if present else None
            else:
                nbt = rr.nbt()
            entries.append((pos, btype, nbt))
        if parse_light_tail(rr, section_count, light_section_count, has_skylight):
            return entries
        return None

    for optional in (False, True):
        try:
            entries = attempt(optional)
        except (AssertionError, IndexError, struct.error, ValueError):
            entries = None
        if entries is not None:
            return entries, ("optional" if optional else "direct")
    return [], "unparsed"


# ---------------------------------------------------------------------------
# Headless client: tolerant container/component parsing + observation model.
# ---------------------------------------------------------------------------

class MatrixClient(ParityClient):
    def __init__(self, game, name=CLIENT_NAME):
        super().__init__(game, name)
        self.sequence = 0
        self.block_packets = []      # (pid, pos, state) for every 0x09/0x49 change
        self.block_entities = {}     # pos -> {"type": int, "nbt": ...} from 0x07
        self.screens = []            # {"window","type"} opens and closes
        self.open_window = None
        self.contents = {}           # window -> {"state","slots","cursor"}
        self.menu_state = {}         # window -> last stateId seen
        self.slot_echoes = []        # (window, state, slot, stack)
        self.sign_editor_packets = []
        self.join_mode = None
        self.gm_seen = None           # last game_event(3) gamemode float -> int
        self.inv_echo = {}            # window-0 slot -> latest 0x15 stack

    # -- packet taps ---------------------------------------------------------
    def read(self, timeout):
        before = len(self.updates)
        pid, body = super().read(timeout)
        if self.phase == "play":
            if pid in (0x09, 0x49):
                for pos, state in self.updates[before:]:
                    self.block_packets.append((pid, pos, state))
        return pid, body

    def observe_gameplay(self, packet, body):
        r = Reader(body)
        try:
            if packet == 0x2b:
                self.join_mode = join_game_mode(body)
            elif packet == 0x22 and len(body) >= 5 and body[0] == 3:
                # game_event 3 = gamemode change, f32 value
                self.gm_seen = int(struct.unpack(">f", body[1:5])[0])
            elif packet == 0x13:
                wid = r.number("B")
                state = r.varint()
                count = r.varint()
                slots = [tolerant_stack(r) for _ in range(count)]
                cursor = tolerant_stack(r)
                if r.pos != len(body):
                    raise ValueError("container content trailing bytes")
                self.contents[wid] = {"state": state, "slots": slots, "cursor": cursor}
                self.menu_state[wid] = state
            elif packet == 0x15:
                wid = r.number("B")
                state = r.varint()
                slot = r.number(">h")
                value = tolerant_stack(r)
                if r.pos != len(body):
                    raise ValueError("set slot trailing bytes")
                self.slot_echoes.append((wid, state, slot, value))
                if wid == 0 and slot >= 0:
                    self.inv_echo[slot] = value
                owner = wid if wid != 255 else (self.open_window or 0)
                self.menu_state[owner] = state
                if slot == -1:
                    # cursor-carried stack echo; keep click()'s `carried`
                    # field truthful or the server sees a desync
                    if owner in self.contents:
                        self.contents[owner]["cursor"] = value
                elif wid in self.contents:
                    slots = self.contents[wid]["slots"]
                    if 0 <= slot < len(slots):
                        slots[slot] = value
            elif packet == 0x33:
                wid = r.varint()
                mtype = r.varint()
                title_raw = body[r.pos:]
                self.screens.append({"window": wid, "type": mtype,
                                     "title_len": len(title_raw)})
                self.open_window = wid
            elif packet == 0x12:
                wid = body[0] if body else None
                self.screens.append({"window": wid, "type": "close"})
                if wid == self.open_window:
                    self.open_window = None
            elif packet == 0x07:
                pos = unpack_position(r.take(8))
                btype = r.varint()
                nbt = r.nbt()
                if r.pos != len(body):
                    raise ValueError("tile entity trailing bytes")
                self.block_entities[pos] = {"type": btype, "nbt": nbt}
            elif packet == 0x34:
                self.sign_editor_packets.append(body)
        except Exception as exc:  # keep the session alive; surface the failure
            self.parse_errors = getattr(self, "parse_errors", [])
            self.parse_errors.append(f"0x{packet:02x}: {exc}")

    # -- actions -------------------------------------------------------------
    def use_on(self, pos, face=1, cursor=(0.5, 1.0, 0.5), hand=0):
        self.sequence += 1
        self.send(0x38, vint(hand) + position(*pos) + vint(face)
                  + struct.pack(">fff?", *cursor, False) + vint(self.sequence))

    def player_action(self, action, pos, face=1):
        self.sequence += 1
        self.send(0x24, vint(action) + position(*pos) + bytes([face & 0xFF])
                  + vint(self.sequence))

    def set_held(self, hotbar_index):
        self.send(0x2f, struct.pack(">h", hotbar_index))

    def creative_slot(self, slot, item=0, count=0, components=()):
        self.send(0x32, struct.pack(">h", slot)
                  + slot_payload(item, count, components))

    def sign_update(self, pos, lines, front=True):
        # Protocol 767 UpdateSign carries the four lines as plain strings
        # (verified against minecraft-data pc/1.21.1 packet_update_sign and
        # live vanilla: the server JSON-encodes them into the sign NBT).
        body = position(*pos) + bytes([1 if front else 0])
        for line in (list(lines) + [""] * 4)[:4]:
            raw = line.encode("utf-8")
            body += vint(len(raw)) + raw
        self.send(0x35, body)

    def click(self, window, slot, button=0, mode=0):
        state = self.menu_state.get(window, 0)
        item, count, _ = self.contents.get(window, {}).get("cursor", EMPTY_STACK)
        carried = slot_payload(item, count) if count else vint(0)
        self.send(0x0e, bytes([window]) + vint(state) + struct.pack(">h", slot)
                  + bytes([button]) + vint(mode) + vint(0) + carried)
        return state

    def close_window(self, window):
        self.send(0x0f, bytes([window]))


# ---------------------------------------------------------------------------
# Server lifecycle: one disposable world dir per target, SIGTERM restart.
# ---------------------------------------------------------------------------

@contextlib.contextmanager
def server_session(kind, binary, root, jar=JAR):
    """Launch one server on loopback with RCON; SIGTERM on exit = Save-v2 stop.

    Vanilla runs survival default + force-gamemode=false so the `gamemode`
    command's persisted value is what the restart join observes — the same
    semantics mc-spine has."""
    root = Path(root)
    root.mkdir(parents=True, exist_ok=True)
    game, rcon_port = port(), port()
    while rcon_port == game:
        rcon_port = port()
    env = dict(os.environ, TMPDIR=str(ROOT / "target/tmp"))
    if kind == "mc-spine":
        for link, source in (("config_packets.bin", ROOT / "config_packets.bin"),
                             ("assets", ROOT / "assets")):
            if not (root / link).exists():
                (root / link).symlink_to(source,
                                         target_is_directory=source.is_dir())
        env.update(MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
                   MC_SPINE_SEED=str(SEED), MC_SPINE_WG="old",
                   MC_SPINE_RESOURCE_PACK_URL="",
                   MC_SPINE_RCON_PORT=str(rcon_port), MC_SPINE_RCON_PASS=RCON_PASS,
                   MC_SPINE_RCON_BIND_ADDR="127.0.0.1")
        env.pop("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS", None)
        argv = [str(binary)]
    else:
        if hashlib.sha1(jar.read_bytes()).hexdigest() != JAR_SHA1:
            raise RuntimeError("pinned vanilla server.jar SHA-1 mismatch")
        if not JAVA.is_file():
            raise FileNotFoundError(JAVA)
        (root / "eula.txt").write_text("eula=true\n")
        options = {
            "server-port": game, "server-ip": "127.0.0.1", "online-mode": "false",
            "level-seed": SEED, "level-type": "minecraft:flat",
            "gamemode": "survival", "force-gamemode": "false",
            "spawn-protection": 0, "difficulty": "peaceful",
            "view-distance": 2, "simulation-distance": 2,
            "network-compression-threshold": 256,
            "enable-command-block": "false",
            "enable-rcon": "true", "rcon.port": rcon_port,
            "rcon.password": RCON_PASS,
        }
        (root / "server.properties").write_text(
            "".join(f"{k}={v}\n" for k, v in options.items()))
        (root / "ops.json").write_text(json.dumps([{
            "uuid": str(uuid.UUID(bytes=offline_uuid(CLIENT_NAME))),
            "name": CLIENT_NAME, "level": 4, "bypassesPlayerLimit": True,
        }]))
        argv = [str(JAVA), f"-Djava.io.tmpdir={ROOT / 'target/tmp'}",
                "-XX:-UsePerfData", "-Xms256M", "-Xmx1300M",
                "-jar", str(jar), "nogui"]
    with (root / "server.log").open("w+") as log:
        proc = subprocess.Popen(argv, cwd=root, env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 90
            while True:
                if proc.poll() is not None:
                    log.flush(); log.seek(0)
                    tail = log.read()[-2500:]
                    raise RuntimeError(f"{kind} exited {proc.returncode} before listener\n{tail}")
                try:
                    with socket.create_connection(("127.0.0.1", game), timeout=0.25):
                        break
                except OSError:
                    if time.monotonic() > deadline:
                        raise TimeoutError(f"{kind} listener not ready")
                    time.sleep(0.25)
            yield game, rcon_port
        finally:
            proc.terminate()          # SIGTERM -> vanilla/mc-spine Save v2
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=10)


# ---------------------------------------------------------------------------
# Run environment: client + rcon + arena origin + observation helpers.
# ---------------------------------------------------------------------------

class Env:
    def __init__(self, client, rcon_port, origin, kind=None):
        self.c = client
        self.rcon = rcon_port
        self.origin = origin          # (ax, ay, az); floor top = ay-1, feet = ay
        self.kind = kind              # "mc-spine" | "vanilla" | None
        self.checks = []
        self.errors = []
        self.touched = set()          # absolute positions ever placed/toggled
        self.sign_pos = None
        self.chest_pos = None
        self.furnace_pos = None
        self.chest_slots = None
        self.furnace_slots = None
        self.mode = None              # last gamemode we commanded (0 surv, 1 creative)

    # coordinate helpers -----------------------------------------------------
    def abs(self, rel):
        return (rel[0] + self.origin[0], rel[1] + self.origin[1], rel[2] + self.origin[2])

    def rel(self, pos):
        return (pos[0] - self.origin[0], pos[1] - self.origin[1], pos[2] - self.origin[2])

    def rel_key(self, pos):
        return ",".join(str(v) for v in self.rel(pos))

    # checks -----------------------------------------------------------------
    def check(self, ok, label, detail=None):
        self.checks.append({"label": label, "ok": bool(ok),
                            **({"detail": detail} if detail is not None else {})})
        return ok

    # server helpers ---------------------------------------------------------
    def cmd(self, text):
        return command(self.rcon, text)

    def soft_cmd(self, text):
        try:
            return command(self.rcon, text)
        except Exception as exc:
            self.errors.append(f"rcon {text!r}: {exc}")
            return ""

    def tp(self, pos):
        self.cmd(f"tp {self.c.name} {pos[0]} {pos[1]} {pos[2]}")
        until(self.c, lambda: self.c.pos == (float(pos[0]), float(pos[1]), float(pos[2])),
              f"tp {pos}", timeout=15)
        read_for(self.c, 0.15)

    def stand_for(self, pos, dz=1):
        """Teleport so the player's feet sit in the air cell `dz` cells south of
        the target block, at the target's own y — always inside interaction
        reach and inside the cleared arena."""
        x, y, z = pos
        self.tp((x + 0.5, y, z + dz + 0.5))

    def gamemode(self, mode):
        want = 1 if mode else 0
        if self.c.gm_seen == want:
            self.mode = want
            return
        self.cmd(f"gamemode {'creative' if want else 'survival'} {self.c.name}")
        try:
            until(self.c, lambda: self.c.gm_seen == want,
                  f"gamemode->{want} game_event", timeout=10)
        except AssertionError:
            self.errors.append(f"gamemode {want} not confirmed by game_event")
        self.mode = want
        read_for(self.c, 0.2)

    # block helpers ----------------------------------------------------------
    def state(self, pos):
        return self.c.block(*pos)

    def wait_state(self, pos, pred, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if pred(self.state(pos)):
                    return True
            except (KeyError, IndexError, AssertionError):
                pass
            try:
                self.c.read(min(0.4, max(0.01, deadline - time.monotonic())))
            except (socket.timeout, AssertionError):
                pass
        return False

    def updates_for(self, mark, pos):
        seen = [(f"0x{pid:02x}", st) for pid, p, st in self.c.block_packets[mark:] if p == pos]
        seen.sort()
        return seen

    def place_via_use_on(self, support_pos, target_pos, hotbar=None):
        """Stand next to target, select hotbar, click the top face of
        support_pos; the item lands at target_pos."""
        self.stand_for(target_pos)
        if hotbar is not None:
            self.c.set_held(hotbar)
            read_for(self.c, 0.15)
        self.touched.add(target_pos)
        mark = len(self.c.block_packets)
        self.c.use_on(support_pos, face=1)
        ok = self.wait_state(target_pos, lambda s: not is_air(s), timeout=8)
        state = self.state(target_pos) if ok else None
        return {"ok": ok, "state": state, "props": props_of(state) if state is not None else {},
                "updates": self.updates_for(mark, target_pos),
                "rel": list(self.rel(target_pos))}

    def survival_break(self, pos, hotbar, wait, label="break"):
        """Timed dig in survival: start-destroy, wait, stop-destroy, expect air."""
        self.stand_for(pos)
        self.c.set_held(hotbar)
        read_for(self.c, 0.15)
        self.touched.add(pos)
        mark = len(self.c.block_packets)
        self.c.player_action(0, pos, face=1)
        time.sleep(wait)
        self.c.player_action(2, pos, face=0)
        ok = self.wait_state(pos, is_air, timeout=6)
        if not ok:
            self.c.player_action(0, pos, face=1)
            time.sleep(wait)
            self.c.player_action(2, pos, face=0)
            ok = self.wait_state(pos, is_air, timeout=6)
        return {"ok": ok,
                "updates": self.updates_for(mark, pos),
                "rel": list(self.rel(pos))}

    def creative_break(self, pos):
        self.stand_for(pos)
        self.touched.add(pos)
        mark = len(self.c.block_packets)
        self.c.player_action(0, pos, face=1)
        ok = self.wait_state(pos, is_air, timeout=6)
        return {"ok": ok, "updates": self.updates_for(mark, pos)}

    def toggle_use_on(self, pos):
        self.stand_for(pos)
        mark = len(self.c.block_packets)
        self.c.use_on(pos, face=1)
        deadline = time.monotonic() + 5
        changed = False
        while time.monotonic() < deadline and not changed:
            try:
                st = self.state(pos)
                changed = True
            except (KeyError, IndexError, AssertionError):
                try:
                    self.c.read(0.3)
                except socket.timeout:
                    pass
        read_for(self.c, 0.4)
        st = self.state(pos)
        return {"state": st, "props": props_of(st),
                "updates": self.updates_for(mark, pos)}

    def inventory_view(self):
        return inventory_view_of(self.c)

    def wait_inventory_total(self, item_name, at_least, timeout=8.0):
        """Poll the authoritative `/data get entity Inventory` until the named
        item reaches `at_least` (drop pickup has no guaranteed wire echo)."""
        deadline = time.monotonic() + timeout
        total = 0
        while time.monotonic() < deadline:
            total = inventory_item_total(
                inventory_dataget(self.rcon, self.c.name), item_name)
            if total >= at_least:
                return total
            read_for(self.c, 0.6)
        return total


# ---------------------------------------------------------------------------
# Scenarios. Each returns an observation dict; env.check records assertions
# evaluated from server replies only. All coords in obs are arena-relative.
# ---------------------------------------------------------------------------

def sc_survival_place_break_drop(env):
    c = env.c
    obs = {"mode": "survival"}
    env.gamemode(0)

    placed = {}
    for name, hotbar, rel in (("stone", 0, (1, 0, 1)), ("oak_planks", 1, (3, 0, 1))):
        support = env.abs((rel[0], -1, rel[2]))
        target = env.abs(rel)
        before = env.inventory_view().get(36 + hotbar, {}).get("count", 0)
        res = env.place_via_use_on(support, target, hotbar=hotbar)
        read_for(c, 0.4)
        after = env.inventory_view().get(36 + hotbar, {}).get("count", 0)
        obs[f"place_{name}"] = res
        env.check(res["ok"], f"{name} placed", res["state"])
        env.check(after == before - 1, f"{name} consumed one item",
                  {"before": before, "after": after})
        placed[name] = target

    # break stone with the diamond pickaxe; expect cobblestone drop + pickup
    stone_pos = placed["stone"]
    brk = env.survival_break(stone_pos, hotbar=2, wait=1.0, label="stone")
    obs["break_stone"] = brk
    env.check(brk["ok"], "stone broke to air")
    rel = env.rel(stone_pos)
    env.tp((rel[0] + env.origin[0] + 0.5, rel[1] + env.origin[1],
            rel[2] + env.origin[2] + 0.5))
    cobble = env.wait_inventory_total("cobblestone", 1)
    obs["pickup"] = {"cobblestone_total": cobble,
                     "wire_slot_echoes": len(c.slot_echoes)}
    env.check(cobble >= 1, "cobblestone pickup", cobble)

    # break oak_planks (3s by hand-speed tools); expect planks drop + pickup
    planks_pos = placed["oak_planks"]
    inv_before = inventory_item_total(inventory_dataget(env.rcon, c.name), "oak_planks")
    brk2 = env.survival_break(planks_pos, hotbar=2, wait=4.5, label="planks")
    obs["break_planks"] = brk2
    env.check(brk2["ok"], "planks broke to air")
    rel = env.rel(planks_pos)
    env.tp((rel[0] + env.origin[0] + 0.5, rel[1] + env.origin[1],
            rel[2] + env.origin[2] + 0.5))
    inv_after = env.wait_inventory_total("oak_planks", inv_before + 1)
    obs["pickup_planks"] = {"oak_planks_delta": inv_after - inv_before}
    env.check(inv_after - inv_before >= 1, "oak_planks pickup", inv_after - inv_before)
    return obs


def sc_support_cascade(env):
    c = env.c
    obs = {}
    env.gamemode(0)
    cases = [
        ("torch", 4, (4, 0, 4)), ("lantern", 5, (6, 0, 4)),
        ("rail", 6, (8, 0, 4)), ("poppy", 7, (10, 0, 4)),
    ]
    results = {}
    for name, hotbar, rel in cases:
        support = env.abs(rel)                          # dirt pillar cell (feet level)
        fragile = env.abs((rel[0], rel[1] + 1, rel[2]))
        env.cmd(f"setblock {support[0]} {support[1]} {support[2]} minecraft:dirt")
        env.touched.add(support)
        env.touched.add(fragile)
        read_for(c, 0.4)
        place = env.place_via_use_on(support, fragile, hotbar=hotbar)
        read_for(c, 0.3)
        brk = env.survival_break(support, hotbar=3, wait=1.0, label=f"{name}-support")
        read_for(c, 0.4)
        fragile_state = env.state(fragile)
        results[name] = {
            "rel": [rel[0], rel[1] + 1, rel[2]],
            "place": place, "support_break": brk,
            "fragile_final": fragile_state,
            "fragile_props": props_of(fragile_state),
        }
        env.check(place["ok"], f"{name} placed on support", place["state"])
        env.check(brk["ok"], f"{name} support broke")
        env.check(is_air(fragile_state), f"{name} cascade popped", fragile_state)
    obs["cases"] = results
    return obs


def sc_interactive_redstone(env):
    c = env.c
    obs = {}
    env.gamemode(1)
    c.send(0x1c, struct.pack(">ff", 0.0, 0.0) + bytes([0]))  # yaw 0, deterministic facing
    read_for(c, 0.2)

    cases = [
        ("oak_door", ITEM["oak_door"], (4, 0, 6)),
        ("oak_trapdoor", ITEM["oak_trapdoor"], (6, 0, 6)),
        ("oak_fence_gate", ITEM["oak_fence_gate"], (8, 0, 6)),
        ("lever", ITEM["lever"], (10, 0, 6)),
        ("stone_button", ITEM["stone_button"], (12, 0, 6)),
    ]
    out = {}
    for name, item, rel in cases:
        target = env.abs(rel)
        support = env.abs((rel[0], -1, rel[2]))
        env.touched.add(target)
        extra = env.abs((rel[0], rel[1] + 1, rel[2])) if name == "oak_door" else None
        if extra:
            env.touched.add(extra)
        c.creative_slot(36, item, 1)
        read_for(c, 0.25)
        place = env.place_via_use_on(support, target, hotbar=0)
        read_for(c, 0.3)
        entry = {"rel": list(rel), "place": place}
        entry["toggle_on"] = env.toggle_use_on(target)
        read_for(c, 0.3)
        entry["toggle_off"] = env.toggle_use_on(target)
        read_for(c, 0.3)
        if extra:
            st = env.state(extra)
            entry["upper"] = {"state": st, "props": props_of(st)}
        out[name] = entry
        env.check(place["ok"], f"{name} placed", place["state"])
        env.check(entry["toggle_on"]["state"] is not None
                  and entry["toggle_off"]["state"] is not None,
                  f"{name} toggles readable")
    obs["blocks"] = out
    return obs


def sc_sign(env):
    c = env.c
    obs = {}
    env.gamemode(1)
    rel = (2, 0, 10)
    target = env.abs(rel)
    support = env.abs((rel[0], -1, rel[2]))
    env.touched.add(target)
    env.sign_pos = target
    c.creative_slot(36, ITEM["oak_sign"], 1)
    read_for(c, 0.25)
    place = env.place_via_use_on(support, target, hotbar=0)
    read_for(c, 0.5)
    obs["place"] = place
    obs["sign_state"] = {"state": env.state(target), "props": props_of(env.state(target))}
    obs["editor_open_0x34"] = len(c.sign_editor_packets) > 0
    env.check(place["ok"], "sign placed", place["state"])

    # Sign text edits are survival/adventure-only on both servers.
    env.gamemode(0)
    pre_be = c.block_entities.get(target)
    pre_msgs = canon(pre_be["nbt"].get("front_text", {}).get("messages")) \
        if pre_be else None
    c.sign_update(target, SIGN_LINES)
    deadline = time.monotonic() + 6
    broadcast = None
    while time.monotonic() < deadline:
        be = c.block_entities.get(target)
        if be is not None:
            msgs = be["nbt"].get("front_text", {}).get("messages")
            if msgs is not None:
                if pre_msgs is None:
                    # first sighting becomes the baseline; keep waiting for
                    # the post-edit broadcast to differ from it
                    pre_msgs = canon(msgs)
                    continue
                # an all-empty messages list is truthy but stale — require
                # the content to actually change
                if canon(msgs) != pre_msgs:
                    broadcast = be
                    break
        try:
            c.read(0.4)
        except socket.timeout:
            continue
    if broadcast is None:
        broadcast = c.block_entities.get(target)
    obs["be_after_edit"] = canon(broadcast["nbt"]) if broadcast else None
    obs["lines"] = sign_lines_from_nbt(broadcast["nbt"]) if broadcast else None
    obs["be_count"] = len(c.block_entities)
    env.check(broadcast is not None, "0x07 sign broadcast received")
    env.check(broadcast is not None and canon(broadcast["nbt"]) is not None,
              "sign NBT parseable")
    return obs


def sc_creative_slots(env):
    c = env.c
    obs = {}
    env.gamemode(1)
    c.creative_slot(36, ITEM["stone"], 5)
    read_for(c, 0.3)
    c.creative_slot(37, ITEM["stone"], 1, [(5, nbt_string('"Hero"'))])
    read_for(c, 0.3)
    c.creative_slot(38, ITEM["diamond_sword"], 1, [(9, enchant_payload([(32, 5)]))])
    read_for(c, 0.3)
    mark = len(c.slot_echoes)
    c.creative_slot(46, ITEM["stone"], 7)
    read_for(c, 0.6)
    echoes = c.slot_echoes[mark:]
    obs["recent_echoes"] = [
        {"window": e[0], "state": e[1], "slot": e[2], "stack": canon_stack(e[3])}
        for e in c.slot_echoes[-8:]]
    obs["out_of_range_echoes"] = [
        {"window": e[0], "slot": e[2]} for e in echoes if e[2] == 46]
    env.check(not any(e[2] == 46 for e in echoes), "slot 46 produced no echo")
    inv = env.inventory_view()
    obs["slots"] = {str(k): v for k, v in inv.items() if k in (36, 37, 38)}
    env.check(inv.get(36, {}).get("item") == ITEM["stone"]
              and inv.get(36, {}).get("count") == 5, "slot36 stone x5")
    env.check(inv.get(37, {}).get("item") == ITEM["stone"]
              and bool(inv.get(37, {}).get("components")), "slot37 named components")
    env.check(inv.get(38, {}).get("item") == ITEM["diamond_sword"]
              and bool(inv.get(38, {}).get("components")), "slot38 enchanted sword")
    return obs


def open_window_and_read(env, pos, menu_marker, take, keep_open=False):
    """Stand by `pos`, right-click it, wait for a fresh screen + content, return
    canonical container slots (first `take`) and window id. The window is left
    open when `keep_open` so the caller can click inside it."""
    c = env.c
    env.stand_for(pos)
    mark = len(c.screens)
    c.use_on(pos, face=1)
    try:
        until(c, lambda: any(e.get("type") != "close" for e in c.screens[mark:]),
              f"open {pos}", timeout=8)
    except AssertionError:
        pass
    read_for(c, 0.5)
    opens = [e for e in c.screens[mark:] if e.get("type") != "close"]
    if not opens:
        return {"window": None, "slots": None}
    win = opens[-1]["window"]
    if win not in c.contents:
        return {"window": win, "slots": None}
    slots = [canon_stack(s) for s in c.contents[win]["slots"][:take]]
    if not keep_open:
        c.close_window(win)
        read_for(c, 0.3)
    return {"window": win, "slots": slots}


def settle_cursor(client, win, timeout=5):
    """Wait until the server-confirmed carried stack is empty so a following
    close_window cannot return items to the player inventory."""
    try:
        until(client, lambda: client.contents.get(win, {}).get(
            "cursor", EMPTY_STACK) == EMPTY_STACK, "cursor settle", timeout=timeout)
    except AssertionError:
        pass


def sc_containers(env):
    c = env.c
    obs = {}
    env.gamemode(1)

    chest_rel = (2, 0, 12)
    chest = env.abs(chest_rel)
    env.chest_pos = chest
    env.touched.add(chest)
    c.creative_slot(36, ITEM["chest"], 1)
    read_for(c, 0.25)
    place = env.place_via_use_on(env.abs((chest_rel[0], -1, chest_rel[2])), chest, hotbar=0)
    read_for(c, 0.3)
    obs["chest_place"] = place
    env.check(place["ok"], "chest placed", place["state"])

    # stock two hotbar stacks then click them into chest slots 0 and 1
    c.creative_slot(36, ITEM["stone"], 64)
    c.creative_slot(37, ITEM["dirt"], 3)
    read_for(c, 0.4)
    opened = open_window_and_read(env, chest, "chest", 27, keep_open=True)
    obs["chest_open"] = {"window": opened["window"], "slot_count":
                         len(c.contents.get(opened["window"], {}).get("slots", []))}
    env.check(opened["slots"] is not None, "chest opened")
    win = opened["window"]
    if win is not None and win in c.contents:
        n = len(c.contents[win]["slots"])
        hotbar0, hotbar1 = n - 9, n - 8
        c.click(win, hotbar0); read_for(c, 0.4)
        c.click(win, 0); read_for(c, 0.4)
        c.click(win, hotbar1); read_for(c, 0.4)
        c.click(win, 1); read_for(c, 0.4)
        chest_slots = [canon_stack(s) for s in c.contents[win]["slots"][:27]]
        obs["chest_contents"] = chest_slots
        env.chest_slots = chest_slots
        env.check(chest_slots[0]["item"] == ITEM["stone"] and chest_slots[0]["count"] == 64,
                  "chest slot0 stone64", chest_slots[0])
        env.check(chest_slots[1]["item"] == ITEM["dirt"] and chest_slots[1]["count"] == 3,
                  "chest slot1 dirt3", chest_slots[1])
        settle_cursor(c, win)
        c.close_window(win)
        read_for(c, 0.3)

    fur_rel = (4, 0, 12)
    fur = env.abs(fur_rel)
    env.furnace_pos = fur
    env.touched.add(fur)
    c.creative_slot(36, ITEM["furnace"], 1)
    read_for(c, 0.25)
    place2 = env.place_via_use_on(env.abs((fur_rel[0], -1, fur_rel[2])), fur, hotbar=0)
    read_for(c, 0.3)
    obs["furnace_place"] = place2
    env.check(place2["ok"], "furnace placed", place2["state"])

    c.creative_slot(36, ITEM["dirt"], 1)
    c.creative_slot(37, ITEM["coal"], 1)
    read_for(c, 0.4)
    opened2 = open_window_and_read(env, fur, "furnace", 3, keep_open=True)
    obs["furnace_open"] = {"window": opened2["window"],
                           "slot_count": len(c.contents.get(opened2["window"], {}).get("slots", []))}
    env.check(opened2["slots"] is not None, "furnace opened")
    win2 = opened2["window"]
    if win2 is not None and win2 in c.contents:
        n2 = len(c.contents[win2]["slots"])
        fh0, fh1 = n2 - 9, n2 - 8
        c.click(win2, fh0); read_for(c, 0.3)
        c.click(win2, 0); read_for(c, 0.3)     # input
        c.click(win2, fh1); read_for(c, 0.3)
        c.click(win2, 1); read_for(c, 0.3)     # fuel
        fur_slots = [canon_stack(s) for s in c.contents[win2]["slots"][:3]]
        obs["furnace_contents"] = fur_slots
        env.furnace_slots = fur_slots
        env.check(fur_slots[0]["item"] == ITEM["dirt"] and fur_slots[0]["count"] == 1,
                  "furnace input dirt", fur_slots[0])
        env.check(fur_slots[1]["item"] == ITEM["coal"] and fur_slots[1]["count"] == 1,
                  "furnace fuel coal", fur_slots[1])
        settle_cursor(c, win2)
        c.close_window(win2)
        read_for(c, 0.3)
    return obs


def capture_snapshot(env):
    """End-of-phase-A world/player state, all coords relative to origin."""
    c = env.c
    blocks = {}
    for pos in sorted(env.touched):
        try:
            st = env.state(pos)
        except Exception:
            st = None
        blocks[env.rel_key(pos)] = st
    sign = None
    if env.sign_pos is not None:
        be = c.block_entities.get(env.sign_pos)
        sign = {"pos": list(env.rel(env.sign_pos)),
                "nbt": canon(be["nbt"]) if be else None}
    containers = {}
    for key, pos, take in (("chest", env.chest_pos, 27),
                           ("furnace", env.furnace_pos, 3)):
        if pos is None:
            continue
        opened = open_window_and_read(env, pos, key, take)
        containers[key] = {"pos": list(env.rel(pos)), "slots": opened["slots"]}
    position_out = None
    try:
        pos_out = command(env.rcon, f"data get entity {c.name} Pos")
    except Exception as exc:
        env.errors.append(f"data get entity Pos: {exc}")
        pos_out = ""
    m = re.search(r"\[([^\]]+)\]", pos_out)
    if m:
        position_out = [round(float(v.strip().removesuffix("d").removesuffix("f")), 4)
                        for v in m.group(1).split(",")]
    return {
        "blocks": blocks,
        "sign": sign,
        "containers": containers,
        "inventory": settled_inventory(env.rcon, c.name, env.kind) or {},
        "gamemode": env.mode,
        "position": position_out,
        "origin": list(env.origin),
    }


def sign_lines_from_nbt(nbt):
    """Canonical sign text: front_text.messages as plain strings."""
    if not isinstance(nbt, dict):
        return None
    msgs = nbt.get("front_text", {}).get("messages")
    if not isinstance(msgs, list):
        return None
    lines = []
    for msg in msgs:
        text = msg
        if isinstance(msg, str):
            try:
                parsed = json.loads(msg)
                if isinstance(parsed, str):
                    text = parsed
                elif isinstance(parsed, dict):
                    text = parsed.get("text", "")
            except (ValueError, TypeError):
                pass
        lines.append(text if isinstance(text, str) else str(text))
    return {"messages": lines,
            "color": nbt.get("front_text", {}).get("color"),
            "has_glowing_text": nbt.get("front_text", {}).get("has_glowing_text"),
            "is_waxed": nbt.get("is_waxed")}


def verify_snapshot(client, rcon_port, name, snap, origin, kind=None):
    """Phase-B: fresh client on the restarted server; re-read everything."""
    out = {"mismatches": [], "observed": {}}
    until(client, lambda: client.pos is not None, "respawn pos", timeout=30)
    read_for(client, 0.8)

    needed = set()
    ax, ay, az = origin
    for key in snap["blocks"]:
        rx, ry, rz = (int(v) for v in key.split(","))
        needed.add(((rx + ax) // 16, (rz + az) // 16))
    deadline = time.monotonic() + 40
    while not needed <= set(client.chunks) and time.monotonic() < deadline:
        try:
            client.read(0.5)
        except socket.timeout:
            continue
    out["observed"]["chunks_ready"] = needed <= set(client.chunks)

    block_diffs = {}
    for key, expected in snap["blocks"].items():
        rx, ry, rz = (int(v) for v in key.split(","))
        pos = (rx + ax, ry + ay, rz + az)
        try:
            actual = client.block(*pos)
        except Exception:
            actual = None
        if actual != expected:
            block_diffs[key] = {"expected": expected, "actual": actual}
    out["observed"]["block_diffs"] = block_diffs
    if block_diffs:
        out["mismatches"].append({"field": "blocks", "count": len(block_diffs),
                                  "diffs": block_diffs})

    # sign: parse block-entity NBT out of the delivered chunk data
    sign_obs = None
    sign_parse = "absent"
    if snap.get("sign"):
        sp = snap["sign"]["pos"]
        pos = (sp[0] + ax, sp[1] + ay, sp[2] + az)
        biome = client.registry_counts.get("minecraft:worldgen/biome", 64)
        scount = client.section_count
        lcount = client.light_section_count
        skylight = client.has_skylight
        found = None
        encodings = set()
        for coord, body in client.raw_chunk.items():
            entries, enc = chunk_block_entities(body, biome, scount, lcount, skylight)
            encodings.add(enc)
            for bpos, btype, nbt in entries:
                if bpos == pos:
                    found = {"type": btype, "nbt": canon(nbt),
                             "lines": sign_lines_from_nbt(nbt)}
        sign_obs = found
        sign_parse = "+".join(sorted(encodings)) or "none"
        expected_lines = sign_lines_from_nbt(snap["sign"].get("nbt"))
        actual_lines = found["lines"] if found else None
        out["observed"]["sign"] = {"chunk_parse": sign_parse, "sign": sign_obs}
        if actual_lines != expected_lines:
            out["mismatches"].append({"field": "sign",
                                      "expected": expected_lines,
                                      "actual": actual_lines})
    else:
        out["observed"]["sign"] = None

    # containers: reopen each and compare container slots
    cont_obs = {}
    env2 = Env(client, rcon_port, origin, kind=kind)
    for key, meta in snap.get("containers", {}).items():
        rel = meta["pos"]
        pos = (rel[0] + ax, rel[1] + ay, rel[2] + az)
        take = 27 if key == "chest" else 3
        opened = open_window_and_read(env2, pos, key, take)
        cont_obs[key] = opened
        if opened["slots"] != meta.get("slots"):
            out["mismatches"].append({"field": f"container_{key}",
                                      "expected": meta.get("slots"),
                                      "actual": opened["slots"]})
    out["observed"]["containers"] = cont_obs

    inv = settled_inventory(rcon_port, name, kind) or {}
    snap_inv = dict(snap["inventory"])
    out["observed"]["inventory"] = inv
    wire_inv = inventory_view_of(client)
    out["observed"]["inventory_wire"] = wire_inv
    if inv != snap_inv:
        diffs = {k: {"expected": snap_inv.get(k), "actual": inv.get(k)}
                 for k in set(inv) | set(snap_inv)
                 if inv.get(k) != snap_inv.get(k)}
        out["mismatches"].append({"field": "inventory", "diffs": diffs})

    out["observed"]["gamemode"] = client.join_mode
    if client.join_mode != snap["gamemode"]:
        out["mismatches"].append({"field": "gamemode",
                                  "expected": snap["gamemode"], "actual": client.join_mode})
    # arena origins differ between target runs — the cross-target obs uses
    # origin-relative coordinates; the same-run restart check above already
    # validated the absolute position against the pre-restart snapshot
    out["observed"]["position"] = (
        [round(client.pos[i] - snap["origin"][i], 4) for i in range(3)]
        if client.pos and snap.get("origin") else
        (list(client.pos) if client.pos else None))
    if snap.get("position") and client.pos:
        deltas = [abs(a - b) for a, b in zip(client.pos, snap["position"])]
        out["observed"]["position_delta"] = max(deltas)
        if max(deltas) > 0.05:
            out["mismatches"].append({"field": "position",
                                      "expected": snap["position"],
                                      "actual": list(client.pos)})
    out["status"] = "PASS" if not out["mismatches"] else "MISMATCH"
    return out


# ---------------------------------------------------------------------------
# Scenario table
# ---------------------------------------------------------------------------

SCENARIOS = [
    ("survival_block_place_break_drop", sc_survival_place_break_drop),
    ("survival_support_cascade", sc_support_cascade),
    ("interactive_redstone_blocks", sc_interactive_redstone),
    ("sign_front_edit_and_persist", sc_sign),
    ("creative_slot_and_components", sc_creative_slots),
    ("container_chest_furnace_persist", sc_containers),
]
RESTART_SCENARIO = "save_restart_roundtrip"
REQUIRED_SCENARIOS = [name for name, _ in SCENARIOS] + [RESTART_SCENARIO]


def build_arena(client, rcon_port):
    """Flat stone platform at y=240 on one 16x16 chunk column near spawn."""
    until(client, lambda: client.pos is not None, "spawn pos", timeout=30)
    read_for(client, 0.5)
    px, py, pz = client.pos
    ax = (int(px) // 16) * 16 + 32
    az = (int(pz) // 16) * 16 + 32
    ay = 240
    command(rcon_port, f"fill {ax} {ay-1} {az} {ax+15} {ay+12} {az+15} minecraft:air")
    command(rcon_port, f"fill {ax} {ay-1} {az} {ax+15} {ay-1} {az+15} minecraft:stone")
    tp_pos = (ax + 2.5, float(ay), az + 2.5)
    command(rcon_port, f"tp {client.name} {tp_pos[0]} {tp_pos[1]} {tp_pos[2]}")
    until(client, lambda: client.pos == tp_pos, "arena teleport", timeout=15)
    need = {(ax // 16, az // 16)}
    deadline = time.monotonic() + 30
    while not need <= set(client.chunks) and time.monotonic() < deadline:
        try:
            client.read(0.5)
        except socket.timeout:
            continue
    if not need <= set(client.chunks):
        raise RuntimeError(f"arena chunks not delivered: {need - set(client.chunks)}")
    until(client, lambda: client.block(ax, ay - 1, az) == 1, "arena floor", timeout=10)
    return (ax, ay, az)


def stock_hotbar(client):
    """Creative-mode inventory stocking via 0x32; echoes observed by client."""
    client.creative_slot(36, ITEM["stone"], 8)          # hotbar0
    client.creative_slot(37, ITEM["oak_planks"], 8)    # hotbar1
    client.creative_slot(38, ITEM["diamond_pickaxe"], 1)
    client.creative_slot(39, ITEM["diamond_shovel"], 1)
    client.creative_slot(40, ITEM["torch"], 6)
    client.creative_slot(41, ITEM["lantern"], 4)
    client.creative_slot(42, ITEM["rail"], 4)
    client.creative_slot(43, ITEM["poppy"], 4)
    client.creative_slot(44, ITEM["oak_sign"], 2)
    read_for(client, 0.6)


def run_target(kind, binary, jar, root):
    """Full phase-A scenarios + Save-v2 restart + phase-B verification."""
    root = Path(root)
    report = {"kind": kind, "scenarios": {}, "restart": None, "errors": []}
    # ---------- phase A ----------
    with server_session(kind, binary, root, jar) as (game, rcon_port):
        with MatrixClient(game, CLIENT_NAME) as client:
            client.login()
            env0 = Env(client, rcon_port, (0, 0, 0), kind=kind)
            for text in ("difficulty peaceful", "gamerule doMobSpawning false",
                         "gamerule doDaylightCycle false"):
                env0.soft_cmd(text)
            origin = build_arena(client, rcon_port)
            env = Env(client, rcon_port, origin, kind=kind)
            env.gamemode(1)
            stock_hotbar(client)
            setup_errors = list(env0.errors) + list(env.errors)
            try:
                until(client, lambda: len(client.slot_echoes) >= 9,
                      "hotbar stock echoes", timeout=10)
            except AssertionError:
                setup_errors.append(
                    f"hotbar stock incomplete: {len(client.slot_echoes)} echoes")
            report["setup_errors"] = setup_errors
            for name, fn in SCENARIOS:
                env.checks = []
                env.errors = []
                try:
                    obs = fn(env)
                except Exception as exc:
                    obs = {"fatal": repr(exc)}
                    env.errors.append(repr(exc))
                fails = [ch for ch in env.checks if not ch["ok"]]
                status = "PASS" if not fails and not env.errors else "FAIL"
                report["scenarios"][name] = {
                    "status": status, "checks": env.checks,
                    "errors": env.errors, "obs": obs,
                }
                if env.errors:
                    idx = [n for n, _ in SCENARIOS].index(name)
                    for later, _ in SCENARIOS[idx + 1:]:
                        report["scenarios"][later] = {
                            "status": "ERROR", "checks": [],
                            "errors": ["skipped: earlier scenario faulted"], "obs": {}}
                    break
            # ---------- end of phase A: fixed spot, survival, snapshot ------
            env.gamemode(0)
            center = (origin[0] + 8, origin[1], origin[2] + 8)
            command(rcon_port, f"tp {CLIENT_NAME} {center[0]+0.5} {center[1]} {center[2]+0.5}")
            until(client, lambda: client.pos == (float(center[0] + 0.5), float(center[1]),
                                                 float(center[2] + 0.5)),
                  "final tp", timeout=15)
            read_for(client, 0.5)
            snap = capture_snapshot(env)
            report["parse_errors"] = getattr(client, "parse_errors", [])
    # server terminated (SIGTERM) -> Save v2 flush
    time.sleep(1.0)
    # ---------- phase B: restart, same world dir, fresh client -------------
    with server_session(kind, binary, root, jar) as (game2, rcon_port2):
        with MatrixClient(game2, CLIENT_NAME) as client2:
            client2.login()
            verify = verify_snapshot(client2, rcon_port2, CLIENT_NAME, snap, origin, kind=kind)
            persisted = {k: v for k, v in snap.items() if k != "origin"}
            if persisted.get("position") and snap.get("origin"):
                persisted["position"] = [
                    round(persisted["position"][i] - snap["origin"][i], 4)
                    for i in range(3)]
            verify["persisted"] = persisted
            report["parse_errors"] += getattr(client2, "parse_errors", [])
    report["restart"] = verify
    report["scenarios"][RESTART_SCENARIO] = {
        "status": verify["status"],
        "checks": [{"label": f"restart {m['field']}", "ok": False,
                    "detail": m} for m in verify["mismatches"]],
        "errors": [],
        "obs": {"persisted": verify["persisted"], "observed": verify["observed"]},
    }
    return report


# ---------------------------------------------------------------------------
# Comparator + report
# ---------------------------------------------------------------------------

def diff_values(a, b, path=""):
    diffs = []

    def child(key):
        return f"{path}.{key}" if path else str(key)

    if isinstance(a, dict) and isinstance(b, dict):
        for key in sorted(set(a) | set(b), key=str):
            if key not in a:
                diffs.append({"path": child(key), "mc-spine": "<absent>", "vanilla": b[key]})
            elif key not in b:
                diffs.append({"path": child(key), "mc-spine": a[key], "vanilla": "<absent>"})
            else:
                diffs += diff_values(a[key], b[key], child(key))
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            diffs.append({"path": path or "<root>", "mc-spine": f"len={len(a)}",
                          "vanilla": f"len={len(b)}"})
        for i, (x, y) in enumerate(zip(a, b)):
            diffs += diff_values(x, y, f"{path}[{i}]" if path else f"[{i}]")
    elif a != b:
        diffs.append({"path": path or "<root>", "mc-spine": a, "vanilla": b})
    return diffs


def compare_targets(spine, vanilla):
    """Per-scenario PASS/MISMATCH matrix between the two target reports."""
    matrix = []
    for name in REQUIRED_SCENARIOS:
        s = spine["scenarios"].get(name, {})
        v = vanilla["scenarios"].get(name, {})
        sobs = s.get("obs", {})
        vobs = v.get("obs", {})
        diffs = diff_values(sobs, vobs, name)
        side_fail = s.get("status") in ("FAIL", "ERROR") or v.get("status") in ("FAIL", "ERROR")
        status = "FAIL" if side_fail else ("MISMATCH" if diffs else "PASS")
        matrix.append({
            "scenario": name,
            "status": status,
            "mc-spine_status": s.get("status"),
            "vanilla_status": v.get("status"),
            "diff_count": len(diffs),
            "diffs": diffs[:60],
            "mc-spine_errors": s.get("errors", []),
            "vanilla_errors": v.get("errors", []),
        })
    return matrix


def summarize(report):
    counts = {}
    for sc in report["scenarios"].values():
        counts[sc["status"]] = counts.get(sc["status"], 0) + 1
    return counts


def build_report(binary, jar, spine, vanilla):
    report = {
        "tool": "survival_creative_oracle_matrix",
        "binary": str(binary),
        "vanilla_jar": str(jar) if jar else None,
        "targets": {},
    }
    if spine:
        report["targets"]["mc-spine"] = spine
    if vanilla:
        report["targets"]["vanilla"] = vanilla
    if spine and vanilla:
        report["matrix"] = compare_targets(spine, vanilla)
    else:
        target = spine or vanilla
        report["matrix"] = [
            {"scenario": name,
             "status": sc["status"],
             "diffs": [],
             "checks": sc.get("checks", []),
             "errors": sc.get("errors", [])}
            for name, sc in target["scenarios"].items()
        ]
    report["summary"] = {kind: summarize(t) for kind, t in report["targets"].items()}
    return report


# ---------------------------------------------------------------------------
# Offline self-test
# ---------------------------------------------------------------------------

class SelfTest(unittest.TestCase):
    def test_scenario_table_complete(self):
        names = [n for n, _ in SCENARIOS]
        self.assertEqual(names, [
            "survival_block_place_break_drop",
            "survival_support_cascade",
            "interactive_redstone_blocks",
            "sign_front_edit_and_persist",
            "creative_slot_and_components",
            "container_chest_furnace_persist",
        ])
        self.assertEqual(REQUIRED_SCENARIOS, names + [RESTART_SCENARIO])
        for name, fn in SCENARIOS:
            self.assertTrue(callable(fn), name)

    def test_comparator_detects_mismatch(self):
        a = {"blocks": {"1,0,1": 1}, "sign": {"messages": ["A"]}}
        b = {"blocks": {"1,0,1": 2}, "sign": {"messages": ["A"]}}
        diffs = diff_values(a, b)
        self.assertEqual(len(diffs), 1)
        self.assertEqual(diffs[0]["path"], "blocks.1,0,1")
        self.assertEqual(diff_values(a, a), [])

    def test_comparator_nested_and_missing(self):
        diffs = diff_values({"a": {"b": [1, 2]}}, {"a": {"b": [1, 3], "c": 1}})
        paths = {d["path"] for d in diffs}
        self.assertIn("a.b[1]", paths)
        self.assertIn("a.c", paths)

    def test_report_schema(self):
        spine = {"kind": "mc-spine", "scenarios": {}, "restart": None, "errors": []}
        for name in REQUIRED_SCENARIOS:
            spine["scenarios"][name] = {"status": "PASS", "checks": [], "errors": [], "obs": {}}
        vanilla = {"kind": "vanilla", "scenarios": {}, "restart": None, "errors": []}
        vanilla["scenarios"] = {k: dict(v) for k, v in spine["scenarios"].items()}
        report = build_report("bin", None, spine, vanilla)
        for key in ("tool", "binary", "targets", "matrix", "summary"):
            self.assertIn(key, report)
        self.assertEqual(len(report["matrix"]), len(REQUIRED_SCENARIOS))
        for row in report["matrix"]:
            self.assertIn(row["status"], ("PASS", "MISMATCH", "FAIL"))
            self.assertIn("diffs", row)
        json.dumps(report)

    def test_slot_payload_roundtrip(self):
        r = Reader(slot_payload(1, 5))
        self.assertEqual(tolerant_stack(r), (1, 5, {}))
        self.assertEqual(Reader(slot_payload()).varint(), 0)
        r = Reader(slot_payload(838, 1, [(9, enchant_payload([(32, 5)]))]))
        item, count, comps = tolerant_stack(r)
        self.assertEqual((item, count), (838, 1))
        self.assertIn(9, comps)
        r = Reader(slot_payload(1, 1, [(5, nbt_string('"Hero"'))]))
        item, count, comps = tolerant_stack(r)
        self.assertEqual(comps.get(5), '"Hero"')

    def test_block_properties_decode(self):
        self.assertEqual(block_properties("stone", 1), {})
        props = block_properties("oak_sign", 4303)
        self.assertIn("rotation", props)
        self.assertEqual(STATE_TO_NAME[1], "stone")
        door = block_properties("oak_door", 4590)
        self.assertIn("open", door)
        self.assertIn("powered", door)

    def test_sign_update_shape(self):
        # Protocol 767 UpdateSign lines are plain strings, not JSON.
        body = position(2, 240, 10) + b"\x01"
        for line in ("A", "B", "", ""):
            raw = line.encode()
            body += vint(len(raw)) + raw
        r = Reader(body[9:])
        self.assertEqual([r.take(r.varint()).decode() for _ in range(4)],
                         ["A", "B", "", ""])

    def test_enchant_payload_bytes(self):
        payload = enchant_payload([(32, 5)])
        r = Reader(payload)
        self.assertEqual(r.varint(), 1)
        self.assertEqual((r.varint(), r.varint()), (32, 5))
        self.assertEqual(r.number("B"), 1)

    def test_canon_stack_json(self):
        json.dumps(canon_stack((1, 5, {})))
        json.dumps(canon_stack((838, 1, {9: {32: 5}, 5: '"Hero"'})))

    def test_sign_lines_canonical(self):
        nbt = {"front_text": {"messages": ['"Oracle"', '""', '"Third"', '""'],
                              "color": "black", "has_glowing_text": 0},
               "is_waxed": 0}
        lines = sign_lines_from_nbt(nbt)
        self.assertEqual(lines["messages"], ["Oracle", "", "Third", ""])
        self.assertEqual(lines["color"], "black")
        self.assertIsNone(sign_lines_from_nbt(None))
        self.assertIsNone(sign_lines_from_nbt({}))

    def test_chunk_be_parse_light_tail(self):
        # A synthetic 0x27 body is overkill; verify the light-tail validator
        # rejects garbage and accepts a trivially-empty mask section is not
        # required here — the live run exercises the real path. Just ensure
        # the helper exists and is deterministic on empty input.
        self.assertFalse(parse_light_tail(Reader(b""), 24, 26, True))


def self_test():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if result.wasSuccessful() else 1


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def faulted_report(kind, exc):
    return {"kind": kind, "errors": [f"run faulted: {exc!r}"],
            "scenarios": {n: {"status": "ERROR", "checks": [],
                              "errors": [repr(exc)], "obs": {}}
                          for n in REQUIRED_SCENARIOS},
            "restart": None}


def main(argv):
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=str(ROOT / "target/debug/mc-spine"))
    parser.add_argument("--with-vanilla", action="store_true")
    parser.add_argument("--vanilla-jar", default=str(JAR))
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--json", action="store_true",
                        help="emit the full JSON report (default: summary + matrix)")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    binary = Path(args.binary)
    if not binary.is_file():
        print(f"missing binary: {binary}", file=sys.stderr)
        return 2
    jar = Path(args.vanilla_jar) if args.with_vanilla else None
    tmp = ROOT / "target/tmp"
    tmp.mkdir(parents=True, exist_ok=True)

    spine = vanilla = None
    try:
        with tempfile.TemporaryDirectory(prefix="matrix-spine-", dir=tmp) as folder:
            spine = run_target("mc-spine", binary, jar, Path(folder) / "world")
    except Exception as exc:
        spine = faulted_report("mc-spine", exc)
    if args.with_vanilla:
        if not jar.is_file():
            print(f"vanilla jar absent: {jar}", file=sys.stderr)
            return 2
        try:
            with tempfile.TemporaryDirectory(prefix="matrix-vanilla-", dir=tmp) as folder:
                vanilla = run_target("vanilla", binary, jar, Path(folder) / "world")
        except Exception as exc:
            vanilla = faulted_report("vanilla", exc)

    report = build_report(binary, jar, spine, vanilla)
    if args.json:
        print(json.dumps(report, indent=1, sort_keys=True, default=str))
    else:
        for row in report["matrix"]:
            print(f"{row['scenario']:38s} {row['status']:9s} "
                  f"spine={row.get('mc-spine_status', row['status'])} "
                  f"vanilla={row.get('vanilla_status', '-')} diffs={row.get('diff_count', 0)}")
        print(json.dumps({"summary": report["summary"]}, sort_keys=True))
        if any(r["status"] != "PASS" for r in report["matrix"]):
            print("--- first diffs ---")
            for row in report["matrix"]:
                if row["status"] == "PASS":
                    continue
                for d in row.get("diffs", [])[:8]:
                    print(f"  {row['scenario']} {d['path']}: "
                          f"mc-spine={d.get('mc-spine')} vanilla={d.get('vanilla')}")
                for e in row.get("mc-spine_errors", [])[:3]:
                    print(f"  {row['scenario']} spine-error: {e}")
                for e in row.get("vanilla_errors", [])[:3]:
                    print(f"  {row['scenario']} vanilla-error: {e}")
                for ch in row.get("checks", [])[:8]:
                    if not ch.get("ok"):
                        print(f"  {row['scenario']} check-fail: "
                              f"{ch.get('label')} {ch.get('detail', '')}")
    bad = any(sc["status"] != "PASS"
              for t in report["targets"].values() for sc in t["scenarios"].values())
    bad = bad or any(r["status"] != "PASS" for r in report["matrix"])
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
