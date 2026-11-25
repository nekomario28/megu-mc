#!/usr/bin/env python3
"""Tests for tools/vanilla_live_diff.py (megu-live-diff/1).

Two layers, both fully offline and fast (<10s total):

  * Codec layer: VarInt/VarLong, compression framing roundtrips, position
    packing, anonymous-NBT component rendering, and every S2C field parser
    the scenarios rely on.
  * Differential layer: an in-process mock protocol-767 server pair. Two
    identical "vanilla-faithful" mocks must produce 18/18 MATCH (exit 0
    with --strict); a deliberately broken mock (wrong block state id, wrong
    difficulty, wrong health, missing pong) must produce the exact expected
    DIFF fields (exit 1 with --strict). Flaw flags pin the negative
    controls for the six bedrock scenarios: a missing custom-name 0x58, a
    persistent mob lost on far teleport, a hostile surviving peaceful, and
    a wrong block-update state id each force DIFF.
"""

import importlib.util
import json
import socket
import struct
import sys
import threading
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TOOL = REPO / "tools" / "vanilla_live_diff.py"

spec = importlib.util.spec_from_file_location("vanilla_live_diff", TOOL)
vld = importlib.util.module_from_spec(spec)
sys.modules["vanilla_live_diff"] = vld
spec.loader.exec_module(vld)


# ---------------------------------------------------------------------------
# Mock protocol-767 server (drives the tool's client end-to-end on loopback)
# ---------------------------------------------------------------------------

POS_4_80_4 = vld.pack_position(4, 80, 4)


def _frame(pid, body=b"", threshold=-1):
    return vld.frame_packet(vld.vint(pid) + body, threshold)


def _pos_body(x, y, z):
    return struct.pack(">Q", vld.pack_position(x, y, z))


def _login_play_body():
    """Minimal 0x2b login the client treats as arrived (fields are not parsed)."""
    return (struct.pack(">i", 1) + b"\x00" + vld.vint(1)
            + vld.mc_string("minecraft:overworld"))


def _position_body(x=8.0, y=80.0, z=8.0, teleport_id=41):
    return (struct.pack(">ddd", x, y, z) + struct.pack(">ff", 0.0, 0.0)
            + b"\x00" + vld.vint(teleport_id) + b"\x00")


def _slot_body(item, count):
    if item is None:
        return vld.vint(0)
    return vld.vint(count) + vld.vint(item) + vld.vint(0) + vld.vint(0)


def _add_entity_body(eid, etype, x=8.0, y=80.0, z=8.0):
    return (vld.vint(eid) + bytes(16) + vld.vint(etype)
            + struct.pack(">ddd", x, y, z) + b"\x00\x00\x00"
            + vld.vint(0) + struct.pack(">hhh", 0, 0, 0))


def _remove_body(eids):
    return vld.vint(len(eids)) + b"".join(vld.vint(e) for e in eids)


def _name_meta_body(eid, name, with_visible=False):
    """0x58 for a named mob: index 2 OptionalComponent (bare TAG_String, the
    compact form vanilla picks for a literal component) + index 9 health
    float. `with_visible` appends index 3 CustomNameVisible (live Megu
    writes it; vanilla leaves the default off the wire)."""
    body = (vld.vint(eid) + b"\x02\x06\x01" + b"\x08"
            + struct.pack(">H", len(name)) + name.encode()
            + b"\x09\x03" + struct.pack(">f", 20.0))
    if with_visible:
        body += b"\x03\x08\x01"
    return body + b"\xff"


def _nbt_text_component(text):
    return (b"\x0a" + b"\x08" + struct.pack(">H", 4) + b"text"
            + struct.pack(">H", len(text)) + text.encode() + b"\x00")


def _nbt_translate_component(key, args):
    """Anonymous NBT `{with:[args...], translate:key}` — the wire form vanilla
    emits for /gamerule feedback and container titles."""
    body = bytearray(b"\x0a")
    body += b"\x09" + struct.pack(">H", 4) + b"with"
    body += b"\x08" + struct.pack(">i", len(args))
    for arg in args:
        body += struct.pack(">H", len(arg)) + arg.encode()
    body += (b"\x08" + struct.pack(">H", 9) + b"translate"
             + struct.pack(">H", len(key)) + key.encode())
    body += b"\x00"
    return bytes(body)


def _system_chat(text):
    return _nbt_text_component(text) + b"\x00"


def _translate_chat(key, args):
    return _nbt_translate_component(key, args) + b"\x00"


class _Session:
    """Per-connection scripted entity/menu state for the mock servers."""

    def __init__(self, broken=False, flaws=()):
        self.broken = broken
        self.flaws = set(flaws)
        self.next_eid = 100
        self.alive = {}       # eid -> {"type","persistent","name"}
        self.window = None    # open chest window id


def _summon(text, session):
    """add_entity + optional name metadata for `summon minecraft:<t> .. {nbt}`."""
    if ":zombie" in text:
        etype = 124
    elif ":sheep" in text:
        etype = 87
    else:
        return []
    name = None
    if 'CustomName:"' in text:
        name = text.split('CustomName:"', 1)[1].split('"', 1)[0]
    persistent = "PersistenceRequired:1b" in text or name is not None
    eid = session.next_eid
    session.next_eid += 1
    session.alive[eid] = {"type": etype, "persistent": persistent,
                          "name": name}
    # `~N` summons offset from the bot's 8,80,8 position
    rel = text.split(" ", 2)[2].split(" ", 1)[0]
    x = 8.0 + (float(rel[1:]) if rel.startswith("~") and len(rel) > 1 else 0.0)
    out = [(0x01, _add_entity_body(eid, etype, x=x))]
    if name is not None and not (name == "jeb_" and "no_jeb" in session.flaws):
        out.append((0x58, _name_meta_body(eid, name)))
    return out


def command_responses(text, session):
    """S2C (pid, body) answers a vanilla-faithful server sends for `text`.

    `session.broken` models the original defect classes; `session.flaws`
    pin the bedrock negative controls."""
    broken = session.broken
    flaws = session.flaws
    if text.startswith("time set"):
        return [(0x64, struct.pack(">q", 0) + struct.pack(">q", 6000))]
    if text == "weather rain":
        # Live 1.21.1: game_event 1 (START_RAINING); the rain-level event-7
        # ramp is noise the scenario filters out.
        return [(0x22, b"\x01" + struct.pack(">f", 0.0))]
    if text == "weather clear":
        return [(0x22, b"\x02" + struct.pack(">f", 0.0))]
    if text == "difficulty normal":
        return [(0x0B, b"\x01\x00")]
    if text == "difficulty peaceful":
        out = [(0x0B, b"\x00\x00")]
        if "peaceful_survives" not in flaws:
            # Vanilla discards every non-persistent hostile (Monster
            # category) on the switch; passives stay tracked.
            gone = [eid for eid, mob in session.alive.items()
                    if mob["type"] == 124 and not mob["persistent"]]
            for eid in gone:
                del session.alive[eid]
            if gone:
                out.append((0x42, _remove_body(gone)))
        return out
    if text == "difficulty hard":
        return [(0x0B, bytes([2 if broken else 3]) + b"\x00")]
    if text == "gamemode creative":
        return [(0x22, b"\x03" + struct.pack(">f", 1.0)),
                (0x38, b"\x0d" + struct.pack(">ff", 0.05, 0.1))]
    if text == "gamemode survival":
        return [(0x22, b"\x03" + struct.pack(">f", 0.0)),
                (0x38, b"\x00" + struct.pack(">ff", 0.05, 0.1))]
    if text.startswith("gamerule "):
        _, rule, *rest = text.split()
        key = "commands.gamerule.query" if not rest else "commands.gamerule.set"
        value = rest[0] if rest else "true"
        return [(0x6C, _translate_chat(key, [rule, value]))]
    if text.startswith("setworldspawn"):
        return []
    if text.startswith("tp "):
        # Live 1.21.1 centres integer x/z coordinates on block centres.
        x, y, z = (float(t) for t in text.split()[-3:])
        out = [(0x40, _position_body(
            x + 0.5 if x.is_integer() else x, y,
            z + 0.5 if z.is_integer() else z, teleport_id=42))]
        if abs(x) > 128 or abs(z) > 128:
            # Tracking loss removes everything; non-persistent hostiles in
            # a still-ticking chunk are then despawned for real.
            gone = list(session.alive)
            for eid in gone:
                mob = session.alive[eid]
                if not mob["persistent"] or "despawn_persistent" in flaws:
                    del session.alive[eid]
            if gone:
                out.append((0x42, _remove_body(gone)))
        else:
            # Back in range: still-alive entities are re-sent (with their
            # name metadata), despawned ones never return.
            for eid, mob in session.alive.items():
                out.append((0x01, _add_entity_body(eid, mob["type"])))
                if mob["name"] is not None:
                    out.append((0x58, _name_meta_body(eid, mob["name"])))
        return out
    if text == "setblock 4 80 4 minecraft:stone":
        return [(0x09, _pos_body(4, 80, 4)
                 + vld.vint(99 if broken else 1))]
    if text == "setblock 4 80 4 minecraft:oak_planks":
        return [(0x09, _pos_body(4, 80, 4) + vld.vint(15))]
    if text == "setblock 6 80 4 minecraft:chest":
        return [(0x09, _pos_body(6, 80, 4) + vld.vint(2955))]
    if text == "setblock 8 80 8 minecraft:oak_stairs[facing=east,half=top]":
        return [(0x09, _pos_body(8, 80, 8)
                 + vld.vint(2885 if "bad_state" in flaws else 2935))]
    if text == "setblock 9 80 8 minecraft:redstone_lamp[lit=true]":
        return [(0x09, _pos_body(9, 80, 8) + vld.vint(7417))]
    if text == "setblock 10 80 8 minecraft:furnace[facing=south,lit=true]":
        return [(0x09, _pos_body(10, 80, 8) + vld.vint(4296))]
    if text.startswith("give "):
        return [(0x15, b"\x00" + vld.vint(1) + struct.pack(">h", 36)
                 + _slot_body(838, 1))]
    if text.startswith("effect give"):
        # Live 1.21.1 wires minecraft:speed as mob_effect id 0.
        return [(0x76, vld.vint(1) + vld.vint(0) + b"\x01" + vld.vint(600)
                 + b"\x0e" + b"\x00")]
    if text.startswith("damage "):
        health = 18.0 if broken else 16.0
        return [(0x5D, struct.pack(">f", health) + vld.vint(20)
                 + struct.pack(">f", 5.0))]
    # Live 1.21.1 sends no scoreboard packets to a client with no display
    # slot, so the scoreboard commands intentionally fall through to [].
    if text.startswith("summon "):
        return _summon(text, session)
    return []


def _open_screen_body(window=1, menu=2, title_key="container.chest"):
    return (vld.vint(window) + vld.vint(menu)
            + _nbt_translate_component(title_key, []))


def _container_content_body(window=1, slots=63):
    return (b"\x01"[:0] + bytes([window]) + vld.vint(1) + vld.vint(slots)
            + bytes(slots) + b"\x00")


class MockServer:
    """A scripted protocol-767 server: handshake/status/login/config/play."""

    def __init__(self, broken=False, flaws=()):
        self.broken = broken
        self.flaws = tuple(flaws)
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._accept_loop, daemon=True)
        self._thread.start()

    def endpoint(self):
        return ("127.0.0.1", self.port)

    def close(self):
        self._stop.set()
        self._thread.join(timeout=2)
        self.listener.close()

    def _accept_loop(self):
        while not self._stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,),
                             daemon=True).start()

    # -- connection phases ----------------------------------------------------

    def _serve(self, conn):
        try:
            conn.settimeout(15)
            reader = _SockReader(conn)
            pid, body = reader.packet()
            assert pid == 0x00, "expected handshake"
            hs = vld.Reader(body)
            hs.varint()
            hs.string()
            hs.number(">H")
            next_state = hs.varint()
            if next_state == 1:
                self._status(conn, reader)
            else:
                self._play(conn, reader)
        except (OSError, AssertionError, vld.WireError):
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    def _status(self, conn, reader):
        pid, _body = reader.packet()  # status_request
        info = {"version": {"name": "1.21.1", "protocol": 767},
                "players": {"max": 20, "online": 0},
                "description": {"text": "mock"}}
        conn.sendall(_frame(0x00, vld.mc_string(json.dumps(info))))
        pid, body = reader.packet()  # ping_request (i64)
        conn.sendall(_frame(0x01, body))

    def _play(self, conn, reader):
        pid, _body = reader.packet()  # login_start
        uuid = vld.offline_uuid(vld.BOT_NAME)
        conn.sendall(_frame(0x02, uuid + vld.mc_string(vld.BOT_NAME)
                            + vld.vint(0)))
        pid, _body = reader.packet()  # login_acknowledged
        assert pid == 0x03
        pid, _body = reader.packet()  # client_information
        assert pid == 0x00
        # select_known_packs then wait for the client's answer before finish
        conn.sendall(_frame(0x0E, vld.vint(0)))
        pid, _body = reader.packet()
        assert pid == 0x07
        conn.sendall(_frame(0x03))
        pid, _body = reader.packet()  # finish_configuration
        assert pid == 0x03
        # play: login + initial position + one keepalive to exercise the
        # client's automatic 0x18 answer.
        conn.sendall(_frame(0x2B, _login_play_body()))
        conn.sendall(_frame(0x40, _position_body()))
        conn.sendall(_frame(0x26, struct.pack(">q", 0xFEED)))
        session = _Session(self.broken, self.flaws)
        while True:
            pid, body = reader.packet()
            if pid == 0x21 and not self.broken:      # ping_request -> pong
                conn.sendall(_frame(0x36, body))
            elif pid in (0x04, 0x05):                # chat_command
                command = _command_text(body)
                for out_pid, out_body in command_responses(command, session):
                    conn.sendall(_frame(out_pid, out_body))
            elif pid == 0x38:                        # use_item_on -> open chest
                session.window = 1
                conn.sendall(_frame(0x33, _open_screen_body()))
                conn.sendall(_frame(0x13, _container_content_body()))
            elif pid == 0x0F:                        # container_close -> echo
                if session.window is not None:
                    conn.sendall(_frame(0x12, bytes([session.window])))
                    session.window = None
            # teleport confirms, keepalives, pongs and batch acks need no reply


def _command_text(body):
    reader = vld.Reader(body)
    return reader.string()


class _SockReader:
    """Blocking packet reader for the mock (uncompressed frames)."""

    def __init__(self, conn):
        self.conn = conn

    def _exact(self, n):
        out = bytearray()
        while len(out) < n:
            chunk = self.conn.recv(n - len(out))
            if not chunk:
                raise vld.WireError("EOF")
            out.extend(chunk)
        return bytes(out)

    def packet(self):
        length = 0
        for i in range(5):
            byte = self._exact(1)[0]
            length |= (byte & 127) << (7 * i)
            if not byte & 128:
                break
        frame = vld.Reader(self._exact(length))
        return frame.varint(), frame.take(frame.remaining())


# ---------------------------------------------------------------------------
# Codec tests
# ---------------------------------------------------------------------------


class CodecTest(unittest.TestCase):
    def test_varint_roundtrip(self):
        cases = [0, 1, 127, 128, 255, 300, 2147483647,
                 -1 & 0xFFFFFFFF, 0x80000000]
        for value in cases:
            encoded = vld.vint(value)
            self.assertEqual(vld.Reader(encoded).varint(), value & 0xFFFFFFFF)
        self.assertEqual(vld.vint(0), b"\x00")
        self.assertEqual(vld.vint(300), b"\xac\x02")
        self.assertEqual(len(vld.vint(-1)), 5)

    def test_varlong_roundtrip(self):
        for value in (0, 1, 300, 1 << 40, -1 & 0xFFFFFFFFFFFFFFFF):
            self.assertEqual(vld.Reader(vld.varlong(value)).varlong(), value)
        self.assertEqual(len(vld.varlong(-1)), 10)

    def test_compression_framing(self):
        raw = vld.vint(0x27) + bytes(4096)  # compressible, above threshold
        for threshold in (-1, 256):
            framed = vld.frame_packet(raw, threshold)
            reader = vld.Reader(framed)
            length = reader.varint()
            body = reader.take(length)
            self.assertEqual(vld.unframe_packet(body, threshold), raw)
        # small packets stay uncompressed even with a threshold
        small = vld.vint(0x01) + b"\x02"
        framed = vld.frame_packet(small, 256)
        reader = vld.Reader(framed)
        body = reader.take(reader.varint())
        self.assertEqual(vld.unframe_packet(body, 256), small)
        # a corrupt compressed stream fails closed
        inner = vld.vint(64) + b"\x00\x01\x02"
        bad = vld.vint(len(inner)) + inner
        reader = vld.Reader(bad)
        with self.assertRaises(vld.WireError):
            vld.unframe_packet(reader.take(reader.varint()), 256)

    def test_position_packing(self):
        for x, y, z in ((4, 80, 4), (-8, -64, -20), (33554432 - 1, 2047, -1)):
            packed = vld.pack_position(x, y, z)
            self.assertEqual(vld.unpack_position(packed), (x, y, z))
            buf = struct.pack(">Q", packed)
            self.assertEqual(vld.parse_block_update(buf + vld.vint(7))["state"],
                             7)

    def test_s2c_parsers(self):
        pos = vld.pack_position(4, 80, 4)
        upd = vld.parse_block_update(struct.pack(">Q", pos) + vld.vint(15))
        self.assertEqual((upd["x"], upd["y"], upd["z"], upd["state"]),
                         (4, 80, 4, 15))
        self.assertEqual(vld.parse_change_difficulty(b"\x03\x00")["difficulty"],
                         3)
        event = vld.parse_game_event(b"\x03" + struct.pack(">f", 1.0))
        self.assertEqual((event["event"], event["value"]), (3, 1.0))
        abilities = vld.parse_player_abilities(
            b"\x0d" + struct.pack(">ff", 0.05, 0.1))
        self.assertTrue(abilities["allow_flying"])
        self.assertTrue(abilities["invulnerable"])
        self.assertFalse(abilities["flying"])
        body = (struct.pack(">ddd", 12.0, 100.0, -8.0)
                + struct.pack(">ff", 90.0, 0.0) + b"\x00" + vld.vint(7)
                + b"\x01")
        pos_parsed = vld.parse_player_position(body)
        self.assertEqual((pos_parsed["x"], pos_parsed["teleport_id"]),
                         (12.0, 7))
        health = vld.parse_set_health(struct.pack(">f", 16.0) + vld.vint(20)
                                      + struct.pack(">f", 5.0))
        self.assertEqual(health["health"], 16.0)
        times = vld.parse_set_time(struct.pack(">q", 12345)
                                   + struct.pack(">q", 6000))
        self.assertEqual(times["day_time"] % 24000, 6000)
        effect = vld.parse_update_mob_effect(
            vld.vint(9) + vld.vint(1) + b"\x01" + vld.vint(600) + b"\x06"
            + b"\x00")
        self.assertEqual((effect["effect_id"], effect["amplifier"]), (1, 1))
        score = vld.parse_set_score(vld.mc_string("DiffBot")
                                    + vld.mc_string("diffobj") + vld.vint(42))
        self.assertEqual(score["value"], 42)
        entity = vld.parse_add_entity(
            vld.vint(2) + bytes(16) + vld.vint(124)
            + struct.pack(">ddd", 6.0, 80.0, 6.0) + b"\x00\x00\x00"
            + vld.vint(0) + struct.pack(">hhh", 0, 0, 0))
        self.assertEqual(entity["entity_type"], 124)
        slot = vld.parse_container_set_slot(
            b"\x00" + vld.vint(1) + struct.pack(">h", 36) + _slot_body(838, 1))
        self.assertEqual((slot["item"], slot["count"]), (838, 1))
        content = vld.parse_container_set_content(
            b"\x00" + vld.vint(1) + vld.vint(2) + _slot_body(None, 0)
            + _slot_body(838, 1) + _slot_body(None, 0))
        self.assertEqual(content["slots"][1]["item"], 838)
        self.assertEqual(vld.parse_pong_response(
            struct.pack(">q", 0x1122334455667788)), 0x1122334455667788)

    def test_s2c_bedrock_parsers(self):
        self.assertEqual(vld.parse_remove_entities(
            vld.vint(2) + vld.vint(5) + vld.vint(9)), [5, 9])
        # system_chat: literal text component and a translate component
        literal = _system_chat("Gamerule keepInventory is now set to: true")
        self.assertEqual(vld.parse_system_chat(literal)["text"],
                         "Gamerule keepInventory is now set to: true")
        trans = _translate_chat("commands.gamerule.query",
                                ["keepInventory", "true"])
        self.assertEqual(vld.parse_system_chat(trans)["text"],
                         "Gamerule keepInventory is currently set to: true")
        trans_set = _translate_chat("commands.gamerule.set",
                                    ["doDaylightCycle", "false"])
        self.assertEqual(vld.parse_system_chat(trans_set)["text"],
                         "Gamerule doDaylightCycle is now set to: false")
        # open_screen: window/menu + translate title resolves to text
        screen = vld.parse_open_screen(
            vld.vint(1) + vld.vint(2)
            + _nbt_translate_component("container.chest", []))
        self.assertEqual(screen, {"window": 1, "menu": 2, "title": "Chest"})
        # set_entity_data: index 2 optional component + index 3 bool +
        # an index 9 float that must be skipped by serializer size
        parsed = vld.parse_set_entity_data(_name_meta_body(77, "jeb_"))
        self.assertEqual(parsed["entity_id"], 77)
        self.assertEqual(parsed["meta"][2], "jeb_")
        self.assertNotIn(3, parsed["meta"])  # vanilla omits the default
        parsed = vld.parse_set_entity_data(
            _name_meta_body(78, "Dinnerbone", with_visible=True))
        self.assertEqual(parsed["meta"][2], "Dinnerbone")
        self.assertIs(parsed["meta"][3], True)


# ---------------------------------------------------------------------------
# Dual-mock differential engine tests
# ---------------------------------------------------------------------------


class DifferentialTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.good_a = MockServer()
        cls.good_b = MockServer()
        cls.broken = MockServer(broken=True)

    @classmethod
    def tearDownClass(cls):
        for server in (cls.good_a, cls.good_b, cls.broken):
            server.close()

    def _run(self, megu, vanilla):
        return vld.run_differential(
            megu.endpoint(), vanilla.endpoint() if vanilla else None,
            timeout=60, per_wait=0.5, command_delay=0.005, settle=0.02,
            tail=0.02)

    def test_all_18_scenarios_registered(self):
        self.assertEqual(vld.SCHEMA, "megu-live-diff/1")
        names = [name for name, _, _ in vld.SCENARIOS]
        self.assertEqual(names, [
            "status_ping", "login_config_play", "ping_request_pong",
            "time_command", "weather_command", "difficulty_command",
            "gamemode_abilities", "teleport_sync", "setblock_state_id",
            "give_item_slot", "effect_and_health", "scoreboard_and_entity",
            "peaceful_hostile_vs_passive", "easter_egg_custom_name_wire",
            "persistent_mob_survives_far_teleport",
            "gamerule_query_and_toggle", "container_chest_open_close",
            "block_state_properties_parity"])
        for name, func, expected in vld.SCENARIOS:
            self.assertTrue(callable(func), name)
            self.assertIsInstance(expected, dict, name)

    def test_identical_servers_match_all(self):
        report = self._run(self.good_a, self.good_b)
        statuses = {e["scenario"]: e["status"] for e in report["results"]}
        self.assertEqual(statuses, {name: "MATCH" for name, _, _ in vld.SCENARIOS})
        self.assertEqual(report["matched"], 18)
        self.assertEqual(report["total"], 18)

    def test_good_mock_satisfies_reference_expectations(self):
        report = self._run(self.good_a, None)
        self.assertEqual(report["matched"], 18, repr(report["results"]))

    def test_broken_server_produces_exact_diffs(self):
        report = self._run(self.broken, self.good_b)
        by_name = {e["scenario"]: e for e in report["results"]}
        self.assertEqual(by_name["ping_request_pong"]["status"], "DIFF")
        self.assertEqual(by_name["ping_request_pong"]["fields"]["pong"],
                         {"megu": None, "vanilla": 0x1122334455667788})
        self.assertEqual(by_name["difficulty_command"]["fields"]["difficulty"],
                         {"megu": 2, "vanilla": 3})
        self.assertEqual(by_name["setblock_state_id"]["fields"]["stone_state"],
                         {"megu": 99, "vanilla": 1})
        self.assertEqual(by_name["effect_and_health"]["fields"]["health"],
                         {"megu": 18.0, "vanilla": 16.0})
        matched = {n for n, e in by_name.items() if e["status"] == "MATCH"}
        self.assertEqual(matched, {n for n, _, _ in vld.SCENARIOS}
                         - {"ping_request_pong", "difficulty_command",
                            "setblock_state_id", "effect_and_health"})
        self.assertEqual(report["matched"], 14)

    def _run_flaw(self, flaws):
        server = MockServer(flaws=flaws)
        self.addCleanup(server.close)
        return self._run(server, self.good_b)

    def test_missing_custom_name_meta_diffs(self):
        """A missing 0x58 jeb_/Dinnerbone custom name must DIFF."""
        report = self._run_flaw({"no_jeb"})
        entry = {e["scenario"]: e
                 for e in report["results"]}["easter_egg_custom_name_wire"]
        self.assertEqual(entry["status"], "DIFF")
        self.assertEqual(entry["fields"]["names"],
                         {"megu": ["Dinnerbone"],
                          "vanilla": ["Dinnerbone", "jeb_"]})

    def test_persistent_mob_lost_on_far_teleport_diffs(self):
        """A persistent mob disappearing on far teleport must DIFF."""
        report = self._run_flaw({"despawn_persistent"})
        entry = {e["scenario"]: e for e in report["results"]}[
            "persistent_mob_survives_far_teleport"]
        self.assertEqual(entry["status"], "DIFF")
        self.assertEqual(entry["fields"]["persistent_resent"],
                         {"megu": False, "vanilla": True})

    def test_hostile_surviving_peaceful_diffs(self):
        """A hostile mob surviving /difficulty peaceful must DIFF."""
        report = self._run_flaw({"peaceful_survives"})
        entry = {e["scenario"]: e for e in report["results"]}[
            "peaceful_hostile_vs_passive"]
        self.assertEqual(entry["status"], "DIFF")
        self.assertEqual(entry["fields"]["zombie_removed"],
                         {"megu": False, "vanilla": True})

    def test_wrong_block_update_state_diffs(self):
        """A wrong 0x09 BlockUpdate state id must DIFF."""
        report = self._run_flaw({"bad_state"})
        entry = {e["scenario"]: e for e in report["results"]}[
            "block_state_properties_parity"]
        self.assertEqual(entry["status"], "DIFF")
        self.assertEqual(entry["fields"]["stairs_state"],
                         {"megu": 2885, "vanilla": 2935})

    def test_strict_exit_codes(self):
        import contextlib, io
        good_ep = f"127.0.0.1:{self.good_b.port}"
        broken_ep = f"127.0.0.1:{self.broken.port}"
        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            ok = vld.main(["--megu-endpoint", good_ep,
                           "--vanilla-endpoint", good_ep, "--strict",
                           "--format", "json", "--timeout", "60",
                           "--per-wait", "0.5", "--command-delay", "0.005",
                           "--settle", "0.02", "--tail", "0.02"])
        self.assertEqual(ok, 0)
        with contextlib.redirect_stderr(buf):
            bad = vld.main(["--megu-endpoint", broken_ep,
                            "--vanilla-endpoint", good_ep, "--strict",
                            "--format", "json", "--timeout", "60",
                            "--per-wait", "0.5", "--command-delay", "0.005",
                            "--settle", "0.02", "--tail", "0.02"])
        self.assertEqual(bad, 1)

    def test_strict_exit_one_on_bedrock_flaw(self):
        """A bedrock defect class also fails --strict end to end."""
        import contextlib, io
        flawed = MockServer(flaws={"bad_state"})
        self.addCleanup(flawed.close)
        good_ep = f"127.0.0.1:{self.good_b.port}"
        flaw_ep = f"127.0.0.1:{flawed.port}"
        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            rc = vld.main(["--megu-endpoint", flaw_ep,
                           "--vanilla-endpoint", good_ep, "--strict",
                           "--format", "json", "--timeout", "60",
                           "--per-wait", "0.5", "--command-delay", "0.005",
                           "--settle", "0.02", "--tail", "0.02"])
        self.assertEqual(rc, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
