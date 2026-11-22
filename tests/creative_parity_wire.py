#!/usr/bin/env python3
"""Creative-mode & builder parity on the live protocol-767 wire.

Drives one loopback client through: middle-click pick-item (`0x20`) hotbar
selection and main-inventory swaps, sign dye/glow-ink/ink-sac/honeycomb
waxing with `0x07` block-entity NBT verification, waxed-sign edit rejection,
and the creative-slot (`0x32`) component preservation + >8 KiB bomb guard.
Assertions read only what the server sends back."""
import json
import math
import os
import struct
import sys
import time
from pathlib import Path

os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"

from protocol767 import (AIR_STATES, Client, ContractFailure, Reader, ROOT,
                         WIRE_BLOCKS, local_server, position, require,
                         spawn_window, unpack_position, vint)

# Item ids are the pinned 1.21.1 protocol registry ids.
STONE, DIRT, OAK_SIGN = 1, 28, 886
RED_DYE, GLOW_INK, INK_SAC, HONEYCOMB, BLUE_DYE = 958, 942, 941, 1221, 955

SLOT_COMPONENT_LIMIT = 8192


def vi(number):
    return vint(number)


def slot_string(text):
    raw = text.encode("utf-8")
    return vint(len(raw)) + raw


def nbt_text(text):
    """Unnamed NBT string value, the shape text components use on the wire."""
    raw = text.encode("utf-8")
    return bytes([8]) + struct.pack(">H", len(raw)) + raw


def slot_payload(item, count, added=(), removed=()):
    """`slot_read_stack`/`slot_wire` body: count, item, added, removed, payloads."""
    out = vint(count)
    if count:
        out += vint(item) + vint(len(added)) + vint(len(removed))
        for ty, payload in added:
            out += vint(ty) + payload
        for ty in removed:
            out += vint(ty)
    return out


def plain_stack(item, count=1):
    return slot_payload(item, count)


def parity_components():
    """One of every representative component family, encoded exactly like the
    protocol-767 fixtures in menus_and_items.rs (raw payload bytes preserved)."""
    enchantments = vi(1) + vi(1) + vi(3) + b"\x01"
    lore = vi(1) + nbt_text("lore line")
    dyed_color = bytes([0x11, 0x22, 0x33, 0x44, 1])
    potion = (b"\x01" + vi(2) + b"\x01" + bytes([0x00, 0x33, 0x55, 0x77]) + vi(1)
              + vi(3) + vi(1) + vi(60) + bytes([0, 1, 1, 0]) + b"\x01"
              + slot_string("custom"))
    firework_explosion = (vi(2) + vi(1) + bytes([1, 2, 3, 4]) + vi(1)
                          + bytes([5, 6, 7, 8]) + bytes([1, 0]))
    fireworks = vi(2) + vi(1) + firework_explosion
    banner = (vi(1) + vi(0) + slot_string("minecraft:stripe")
              + slot_string("stripe") + vi(5))
    container = vi(1) + slot_payload(6, 1, [(16, vi(4))], removed=[3])
    return [
        (5, nbt_text("Hello")),        # custom_name
        (4, b"\x01"),                  # unbreakable
        (7, lore),                     # lore
        (9, enchantments),             # enchantments
        (24, dyed_color),              # dyed_color
        (31, potion),                  # potion_contents
        (46, fireworks),               # fireworks
        (49, banner),                  # banner_patterns
        (52, container),               # container (nested stack)
    ]


def component_bytes(components):
    return sum(len(payload) for _, payload in components)


def creative_slot(client, slot, stack):
    """C2S `0x32`; returns the echoed slot-wire bytes from the window-0 `0x15`."""
    client.send(0x32, struct.pack(">h", slot) + stack)
    return changed_slot(client, 0, slot)


def changed_slot(client, window, slot):
    """First `0x15` whose (window, slot) matches; returns the raw stack-wire tail."""
    seen = []

    def matches(pid, body):
        if pid != 0x15:
            return False
        reader = Reader(body)
        wid = reader.number("B")
        reader.varint()  # state id
        key = reader.number(">h")
        if (wid, key) == (window, slot):
            seen.append(body[reader.pos:])
            return True
        return False

    client.until(matches, "set-slot-echo")
    return seen[0]


def pick_item(client, slot):
    """C2S `0x20` pick item: one VarInt `Inventory.items` index."""
    client.send(0x20, vint(slot))


def player_inv_slot(client, items_slot):
    """`0x15` on the -2 (player inventory) window used by pick-item resyncs."""
    return changed_slot(client, 0xFE, items_slot)


def use_on(client, pos, sequence, face=1, hit=(0.5, 1.0, 0.5)):
    client.send(0x38, vint(0) + position(*pos) + vint(face)
                + struct.pack(">fff?", *hit, False) + vint(sequence))


def sign_update(client, pos, front, lines):
    """C2S `0x35`: pos, front flag byte, four varint-length UTF-8 lines."""
    body = position(*pos) + bytes([1 if front else 0])
    for line in lines:
        raw = line.encode("utf-8")
        body += vint(len(raw)) + raw
    client.send(0x35, body)


def sign_block_entity(body):
    """`0x07` body -> (pos, anonymous NBT compound)."""
    reader = Reader(body)
    pos = unpack_position(reader.take(8))
    require(reader.varint() == 7, "sign-block-entity-type")
    tag = reader.nbt()
    require(reader.pos == len(body), "complete-sign-block-entity")
    return pos, tag


def require_sign(tag, color, glow, waxed):
    require(tag.get("is_waxed") == waxed, "sign-is-waxed")
    front, back = tag["front_text"], tag["back_text"]
    require(front["color"] == color, "sign-front-color")
    require(front["has_glowing_text"] == glow, "sign-front-glow")
    require(back["color"] == "black" and back["has_glowing_text"] == 0,
            "sign-back-untouched")


def drain(client, seconds=0.6):
    """Collect (pid, body) pairs the server emits unprompted for `seconds`."""
    out = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            pid, body = client.read(min(0.1, max(0.0, deadline - time.monotonic())))
        except (TimeoutError, OSError):
            continue
        out.append((pid, body))
    return out


def find_sign_spot(client, sx, sy, sz):
    """A flat column: solid floor, air for the sign, solid+air for the stand
    two blocks south (so the rotation-0 sign's front faces the player)."""
    solid = {state for block in WIRE_BLOCKS
             if block["boundingBox"] == "block" and 0 <= block["hardness"] < 50
             for state in range(block["minStateId"], block["maxStateId"] + 1)}
    top = min(math.floor(sy) - 1, 319)
    for dx in range(1, 8):
        for dz in range(-3, 6):
            x = math.floor(sx) + dx
            z = math.floor(sz) + dz
            for y in range(top, -65, -1):
                if client.block(x, y, z) not in solid:
                    continue
                if y > math.floor(sy) + 1:
                    break
                if (client.block(x, y + 1, z) in AIR_STATES
                        and client.block(x, y, z + 2) in solid
                        and client.block(x, y + 1, z + 2) in AIR_STATES):
                    return (x, y, z)
                break
    raise ContractFailure("no-sign-spot")


def exercise(game):
    with Client(game, "CreativeParity") as client:
        client.login()
        if client.pos is None:
            client.until_id(0x40)
        sx, sy, sz = client.pos
        cx, cz = math.floor(sx) // 16, math.floor(sz) // 16
        client.until_chunks(spawn_window(), "spawn-window")
        require((cx, cz) in client.chunks, "spawn-chunk")

        # 0x32 creative slots are ignored in the survival default.
        client.chat("/gamemode creative")
        before = len(client.messages)

        def gamemode_confirmed(pid, _body):
            if pid == 0x22:
                return len(_body) == 5 and _body[0] == 3
            return pid == 0x6c and any("gamemode" in m for m in client.messages[before:])

        client.until(gamemode_confirmed, "creative-gamemode-confirm")

        # ---------- creative slot: component preservation + bomb guard ----
        components = parity_components()
        comp_stack = slot_payload(STONE, 1, components)
        require(component_bytes(components) <= SLOT_COMPONENT_LIMIT,
                "fixture-within-limit")
        echo = creative_slot(client, 9, comp_stack)
        require(echo == comp_stack, "creative-component-echo-bytes")

        # >8 KiB of added-component payload: parses, but must be refused before
        # storage — proven by the missing echo AND by a later pick moving an
        # empty stack out of that slot.
        bomb = slot_payload(STONE, 1, [(5, nbt_text("a" * 9000))])
        require(len(nbt_text("a" * 9000)) > SLOT_COMPONENT_LIMIT, "bomb-over-limit")
        client.send(0x32, struct.pack(">h", 11) + bomb)
        seen = drain(client, 0.5)
        for pid, body in seen:
            if pid != 0x15:
                continue
            reader = Reader(body)
            if reader.number("B") == 0 and reader.varint() == 0 \
                    and reader.number(">h") == 11:
                raise ContractFailure("bomb-component-echoed")

        # ---------- pick item: main -> hotbar swap with components --------
        pick_item(client, 9)
        hot = player_inv_slot(client, 0)
        require(hot == comp_stack, "pick-moves-component-stack")
        require(player_inv_slot(client, 9) == vint(0), "pick-clears-main-slot")
        require(client.until_id(0x53, "pick-carried") == bytes([0]),
                "pick-carried-slot0")

        # A second main slot lands in the next empty hotbar slot, not the held one.
        creative_slot(client, 10, plain_stack(DIRT, 3))
        pick_item(client, 10)
        require(player_inv_slot(client, 1) == plain_stack(DIRT, 3),
                "pick-second-hotbar-slot")
        require(player_inv_slot(client, 10) == vint(0), "pick-second-source-empty")
        require(client.until_id(0x53, "pick-carried-1") == bytes([1]),
                "pick-carried-slot1")

        # Hotbar index selects the slot: only `0x53`, no slot resyncs.
        pick_item(client, 0)
        require(client.until_id(0x53, "pick-hotbar-select") == bytes([0]),
                "pick-hotbar-carried")

        # The rejected bomb slot picks as empty: had it been stored, the
        # hotbar resync would carry the oversized component payload.
        pick_item(client, 11)
        require(player_inv_slot(client, 2) == vint(0),
                "bomb-never-stored")
        require(player_inv_slot(client, 11) == vint(0), "bomb-source-empty")
        require(client.until_id(0x53, "pick-carried-2") == bytes([2]),
                "pick-carried-slot2")

        # Out-of-range pick slots are dropped fail-closed: no resync at all.
        pick_item(client, 36)
        seen = drain(client, 0.4)
        require(all(pid not in (0x15, 0x53) for pid, _ in seen),
                "invalid-pick-silent")

        # Back to hotbar slot 0 for the sign applicators.
        client.send(0x2f, struct.pack(">h", 0))

        # ---------- sign: place, dye, glow, ink, wax, reject -------------
        (gx, gy, gz) = find_sign_spot(client, sx, sy, sz)
        sign_pos = (gx, gy + 1, gz)
        stand = (gx + 0.5, gy + 1.0, gz + 2.5)
        client.move(*stand)
        client.send(0x1c, struct.pack(">ff", 180.0, 0.0) + bytes([0]))

        creative_slot(client, 36, plain_stack(OAK_SIGN))
        use_on(client, (gx, gy, gz), 1)
        body = client.until_id(0x34, "sign-editor-open")
        reader = Reader(body)
        require(unpack_position(reader.take(8)) == sign_pos, "sign-editor-pos")
        client.until(lambda *_: client.block(*sign_pos) != 0, "sign-placed")

        # Text on the front side is what dye/ink applicators require.
        sign_update(client, sign_pos, True, ["Hello"])
        pos, tag = sign_block_entity(client.until_id(0x07, "sign-text-0x07"))
        require(pos == sign_pos, "sign-text-pos")
        require_sign(tag, "black", 0, 0)
        require(json.loads(tag["front_text"]["messages"][0]) == "Hello",
                "sign-text-line")

        seq = 10

        def apply_sign_item(item):
            nonlocal seq
            creative_slot(client, 36, plain_stack(item))
            use_on(client, sign_pos, seq, face=3, hit=(0.5, 0.5, 0.5))
            seq += 1
            pos, tag = sign_block_entity(client.until_id(0x07, "sign-item-0x07"))
            require(pos == sign_pos, "sign-item-pos")
            ack = client.until_id(0x05, "sign-item-ack")
            require(ack == vint(seq - 1), "sign-item-seq")
            return tag

        tag = apply_sign_item(RED_DYE)
        require_sign(tag, "red", 0, 0)
        tag = apply_sign_item(GLOW_INK)
        require_sign(tag, "red", 1, 0)
        tag = apply_sign_item(INK_SAC)
        require_sign(tag, "red", 0, 0)
        tag = apply_sign_item(GLOW_INK)
        require_sign(tag, "red", 1, 0)
        tag = apply_sign_item(HONEYCOMB)
        require_sign(tag, "red", 1, 1)

        # Waxed: any right-click is the interact-fail no-op — sound + ack,
        # never a block-entity update and never the editor.
        for item in (BLUE_DYE, STONE):
            creative_slot(client, 36, plain_stack(item))
            use_on(client, sign_pos, seq, face=3, hit=(0.5, 0.5, 0.5))
            seq += 1
            client.until_id(0x68, "waxed-sign-sound")
            client.until_id(0x05, "waxed-sign-ack")
            seen = drain(client, 0.5)
            require(all(pid not in (0x07, 0x34) for pid, _ in seen),
                    "waxed-sign-no-update-or-editor")

        # A later `0x35` edit is dropped before it can touch stored text.
        sign_update(client, sign_pos, True, ["Goodbye"])
        seen = drain(client, 0.6)
        require(all(pid != 0x07 for pid, _ in seen), "waxed-edit-ignored")

    print(json.dumps({
        "protocol": 767,
        "pick_item_swap": True,
        "pick_components_preserved": True,
        "creative_bomb_rejected": True,
        "sign_dye_glow_ink_wax": True,
        "waxed_edit_ignored": True,
    }, sort_keys=True))
    print("creative parity wire: PASS")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 \
        else ROOT / "target/debug/megu"
    try:
        with local_server(binary) as game:
            exercise(game)
    except ContractFailure as error:
        print(f"creative parity wire: FAIL {error.code}", file=sys.stderr)
        raise
