#!/usr/bin/env python3
"""Tests for tools/vanilla_parity_audit.py (megu-parity-audit/1).

Mode A runs against the real repository. Mode B is exercised with synthetic
Vanilla-shaped reports/{packets,commands,registries[,blocks]}.json fixtures
that deliberately contain a missing packet, a packet id mismatch, a missing
command root, a missing entity id and block-state drift. Fail-closed behaviour
(exit 2) is checked for a missing --vanilla-jar and corrupt report JSON, and
the strict regression gate (exit 1) for coverage floors and Mode B drift.
"""

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TOOL = REPO / "tools" / "vanilla_parity_audit.py"

spec = importlib.util.spec_from_file_location("vanilla_parity_audit", TOOL)
vpa = importlib.util.module_from_spec(spec)
sys.modules["vanilla_parity_audit"] = vpa
spec.loader.exec_module(vpa)

_MODE_A_CACHE = None


def mode_a_cached():
    """One Mode A scan shared by every test class (must stay < 5s)."""
    global _MODE_A_CACHE
    if _MODE_A_CACHE is None:
        _t0 = time.monotonic()
        _MODE_A_CACHE = vpa.mode_a(REPO)
        _MODE_A_CACHE["_elapsed"] = time.monotonic() - _t0
    return _MODE_A_CACHE


def _write_reports(dirpath, packets, commands, registries, blocks=None):
    dirpath = Path(dirpath)
    dirpath.mkdir(parents=True, exist_ok=True)
    (dirpath / "packets.json").write_text(json.dumps(packets))
    (dirpath / "commands.json").write_text(json.dumps(commands))
    (dirpath / "registries.json").write_text(json.dumps(registries))
    if blocks is not None:
        (dirpath / "blocks.json").write_text(json.dumps(blocks))
    return dirpath


def _blocks_fixture_from_asset():
    """A blocks.json that exactly mirrors assets/block_states_1211.json:
    per-block state counts match, ids contiguous per block."""
    asset = json.loads((REPO / "assets/block_states_1211.json").read_text())
    out = {}
    for name, _first, count in asset["blocks"]:
        out[name] = {"states": [{"id": i} for i in range(count)]}
    return out


def _pick_proto_packet(phase="play", direction="serverbound", skip=0):
    """A real (name, id) pair from proto/src/data_v767.rs."""
    text = (REPO / "proto/src/data_v767.rs").read_text()
    packets = [p for p in vpa.parse_v767_packets(text)
               if p["phase"] == phase and p["direction"] == direction]
    return packets[skip]


class S2CResolverTest(unittest.TestCase):
    """Synthetic send paths test static ID tracing without file-global guesses."""

    @staticmethod
    def _audit(*sources):
        texts = {}
        for index, source in enumerate(sources):
            live, tests = vpa.mask_test_regions(vpa.live_text(source), vpa.code_skeleton(source))
            texts[f"src/probe_{index}.rs"] = (
                live, tests, vpa.code_skeleton(live))
        return vpa.audit_s2c(REPO, texts)["s2c_play_packets"]

    def test_test_only_fields_do_not_hide_following_live_send(self):
        result = self._audit("""
            struct State {
                #[cfg(test)]
                legacy: HashMap<String, Vec<u8>>,
                active: i32,
            }
            fn actual_send(socket: &mut Socket) {
                let state = State {
                    #[cfg(test)]
                    legacy: HashMap::new(),
                    active: 4,
                };
                send_packet(socket, 0x25, vec![]);
            }
            #[cfg(test)]
            fn test_only(socket: &mut Socket) { send_packet(socket, 0x16, vec![]); }
        """)
        self.assertIn(0x25, {p["id"] for p in result["emitted"]})
        self.assertNotIn(0x16, {p["id"] for p in result["emitted"]})

    def test_method_packet_requires_consumed_return_value(self):
        result = self._audit("""
            type WorldMechanismsPacket = (i64, Vec<u8>);
            struct Sender;
            impl Sender {
                fn initialize_packet(&self) -> WorldMechanismsPacket { (0x25, vec![]) }
                fn unused_packet(&self) -> WorldMechanismsPacket { (0x16, vec![]) }
            }
            fn actual_send(socket: &mut Socket, sender: Sender) {
                let (id, payload) = sender.initialize_packet();
                send_packet(socket, id, payload);
            }
        """)
        self.assertIn(0x25, {p["id"] for p in result["emitted"]})
        self.assertNotIn(0x16, {p["id"] for p in result["emitted"]})

    def test_command_result_packet_requires_broadcast_consumer(self):
        source = """
            struct Update;
            impl Update {
                fn into_packet(self) -> WorldMechanismsPacket { (0x4e, vec![]) }
            }
            struct WorldBorderCommand;
            impl WorldBorderCommand {
                fn execute(&self) -> BorderResult {
                    BorderResult { packet: update.map(Update::into_packet) }
                }
            }
            fn unrelated() {
                let result = WorldBorderCommand::parse(args)
                    .and_then(|cmd| cmd.execute(border));
            }
        """
        without_send = self._audit(source)
        self.assertNotIn(0x4e, {p["id"] for p in without_send["emitted"]})

        sent = self._audit(source + """
            fn handler() {
                let result = WorldBorderCommand::parse(args)
                    .and_then(|cmd| cmd.execute(border));
                if let Some((id, body)) = result.packet {
                    broadcast(-1, |_| (id, body.clone()));
                }
            }
        """)
        self.assertIn(0x4e, {p["id"] for p in sent["emitted"]})

    def test_qualified_constant_and_match_arms_reach_send(self):
        result = self._audit("""
            const CLIENTBOUND_COMMANDS: i32 = 0x11;
            fn actual_send(socket: &mut Socket, mode: &str) {
                send_packet(socket, i64::from(brigadier_wire::CLIENTBOUND_COMMANDS), vec![]);
                let pkt_id: i64 = match mode {
                    "title" => 0x65, "subtitle" => 0x63, "actionbar" => 0x4c, _ => -1
                };
                if pkt_id >= 0 { send_packet(socket, pkt_id, vec![]); }
            }
        """)
        ids = {p["id"] for p in result["emitted"]}
        self.assertTrue({0x11, 0x65, 0x63, 0x4c}.issubset(ids))
        self.assertEqual(result["unresolved_send_sites"], 0)

    def test_return_tuple_only_counts_when_bound_to_send(self):
        result = self._audit("""
            type WorldMechanismsPacket = (i64, Vec<u8>);
            fn encode_scoreboard_score() -> WorldMechanismsPacket { (0x61, vec![]) }
            fn actual_send(socket: &mut Socket) {
                let (id, body) = encode_scoreboard_score();
                send_packet(socket, id, body);
            }
            fn dead_encoder() -> WorldMechanismsPacket { (0x02, vec![]) }
        """)
        ids = {p["id"] for p in result["emitted"]}
        self.assertIn(0x61, ids)
        self.assertNotIn(0x02, ids)
        self.assertEqual(result["unresolved_send_sites"], 0)

    def test_dead_constants_and_other_function_bindings_are_not_emitted(self):
        result = self._audit("""
            const S2C_UNUSED_PACKET: i64 = 0x16;
            const CB_UNUSED_PACKET: i64 = 0x6b;
            fn unused(socket: &mut Socket) {
                let id = S2C_UNUSED_PACKET;
            }
            fn live_send(socket: &mut Socket) {
                send_packet(socket, 0x35, vec![]);
                send_packet(socket, id, vec![]);
            }
        """)
        ids = {p["id"] for p in result["emitted"]}
        self.assertEqual(ids, {0x35})
        self.assertEqual(result["unresolved_send_sites"], 1)


class ModeATest(unittest.TestCase):
    """Mode A against the real repository (offline, no external files)."""

    @classmethod
    def setUpClass(cls):
        # One authoritative Mode A scan reused by the JSON-schema test below.
        cls.report = mode_a_cached()

    def test_mode_a_json_schema_and_coverage(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = vpa.main(["--repo", str(REPO), "--format", "json"])
        self.assertEqual(rc, 0)
        report = json.loads(buf.getvalue())
        self.assertEqual(report["schema"], "megu-parity-audit/1")
        self.assertEqual(report["mode"], "offline")
        off = report["offline"]
        packets = off["packets"]
        self.assertGreater(packets["total"], 0)
        self.assertGreater(packets["handled_count"], 0)
        self.assertGreater(packets["handled_pct"], 0.0)
        commands = off["commands"]
        self.assertGreater(commands["registered_count"], 0)
        self.assertGreater(commands["covered_count"], 0)
        self.assertIn("gamerules", off)
        self.assertIn("mechanisms", off)
        self.assertIn("entity_table_consistency", off)

    def test_mode_a_s2c_coverage(self):
        s2c = self.report["offline"]["s2c_play_packets"]
        self.assertGreater(s2c["total"], 100)
        self.assertGreater(s2c["emitted_count"], 0)
        self.assertLessEqual(s2c["emitted_count"], s2c["total"])
        self.assertEqual(
            s2c["emitted_count"] + len(s2c["unemitted"]), s2c["total"])
        for pkt in s2c["emitted"] + s2c["unemitted"]:
            self.assertIn("name", pkt)
            self.assertIsInstance(pkt["id"], int)
        for pkt in s2c["emitted"]:
            self.assertTrue(pkt["via"], f"{pkt['name']} has no via signal")
        # known-live packets must be reported emitted
        emitted_names = {p["name"] for p in s2c["emitted"]}
        self.assertIn("disconnect", emitted_names)
        self.assertIn("entity_event", emitted_names)

    def test_mode_a_behavior_matrix(self):
        bm = self.report["offline"]["behavior_matrix"]
        totals = bm["totals"]
        self.assertGreater(totals["subsystems"], 20)
        self.assertEqual(
            totals["covered"] + totals["gaps"], totals["subsystems"])
        for sub in bm["subsystems"]:
            self.assertIn(sub["status"], ("COVERED", "GAP"))
            self.assertIn("impl", sub)
            self.assertIn("test", sub)
            if sub["status"] == "COVERED":
                self.assertTrue(sub["impl"] and sub["test"], sub)
        groups = {s["group"] for s in bm["subsystems"]}
        for g in ("combat", "redstone", "end", "nether", "tame",
                  "commands", "save"):
            self.assertIn(g, groups)

    def test_mode_a_runtime_budget(self):
        # The optimized scan must stay far below the old ~60s per-character
        # masker; 5s is the hard budget from the lane brief.
        self.assertLess(self.report["_elapsed"], 5.0)

    def test_mode_a_direct_report_shape(self):
        off = self.report["offline"]
        self.assertEqual(self.report["schema"], "megu-parity-audit/1")
        self.assertGreater(off["packets"]["handled_count"], 0)
        self.assertGreater(off["commands"]["covered_count"], 0)
        # every unhandled entry must carry name + hex id
        for pkt in off["packets"]["unhandled"]:
            self.assertIn("name", pkt)
            self.assertIsInstance(pkt["id"], int)


class ModeBFixtureTest(unittest.TestCase):
    """Synthetic Vanilla reports proving each gap class is detected."""

    @classmethod
    def setUpClass(cls):
        cls.mode_a = mode_a_cached()

    def _run_mode_b(self, packets, commands, registries, blocks=None):
        with tempfile.TemporaryDirectory() as td:
            rdir = _write_reports(Path(td) / "reports", packets, commands,
                                  registries, blocks=blocks)
            data, _source = vpa.load_reports_dir(rdir)
        return vpa.audit_vanilla(REPO, self.mode_a, data, "synthetic-test")

    def _packets_fixture(self):
        good = _pick_proto_packet(skip=0)
        other = _pick_proto_packet(skip=1)
        return {
            "play": {
                "serverbound": {
                    # correct: name + id match proto
                    f"minecraft:{good['name']}": {"protocol_id": good["id"]},
                    # deliberate id mismatch
                    f"minecraft:{other['name']}": {"protocol_id": other["id"] + 1},
                    # deliberate missing-in-proto packet
                    "minecraft:zz_parity_probe_missing": {"protocol_id": 250},
                },
                "clientbound": {},
            },
            "configuration": {"serverbound": {}, "clientbound": {}},
            "login": {"serverbound": {}, "clientbound": {}},
            "status": {"serverbound": {}, "clientbound": {}},
            "handshake": {"serverbound": {}, "clientbound": {}},
        }

    def test_missing_packet_and_id_mismatch_detected(self):
        vanilla_cmds = {"children": {"give": {"type": "literal"}}}
        registries = {"minecraft:entity_type": {"entries": {}}}
        out = self._run_mode_b(self._packets_fixture(), vanilla_cmds, registries)
        vp = out["vanilla"]["packets"]
        missing_names = {m["name"] for m in vp["missing_in_proto"]}
        self.assertIn("minecraft:zz_parity_probe_missing", missing_names)
        self.assertGreaterEqual(vp["missing_in_proto_count"], 1)
        mismatched = {(m["name"], m["vanilla_id"], m["proto_id"])
                      for m in vp["id_mismatch_in_proto"]}
        other = _pick_proto_packet(skip=1)
        self.assertIn((other["name"], other["id"] + 1, other["id"]), mismatched)
        self.assertGreaterEqual(vp["id_mismatch_in_proto_count"], 1)

    def _minimal_packets(self):
        good = _pick_proto_packet(skip=0)
        return {"play": {"serverbound": {
            f"minecraft:{good['name']}": {"protocol_id": good["id"]},
        }, "clientbound": {}}}

    def test_missing_command_detected(self):
        packets = self._minimal_packets()
        commands = {"children": {
            "give": {"type": "literal"},
            "zz_parity_probe_cmd": {"type": "literal"},
        }}
        registries = {"minecraft:entity_type": {"entries": {}}}
        out = self._run_mode_b(packets, commands, registries)
        vc = out["vanilla"]["commands"]
        self.assertIn("zz_parity_probe_cmd", vc["missing_vanilla_commands"])
        self.assertGreaterEqual(vc["missing_vanilla_commands_count"], 1)
        # a genuinely registered vanilla root must NOT be reported missing
        self.assertNotIn("give", vc["missing_vanilla_commands"])

    def test_missing_entity_detected(self):
        packets = self._minimal_packets()
        commands = {"children": {"give": {"type": "literal"}}}
        # Two vanilla entries: one real entity + one ghost id. Our ~130-entry
        # table makes the ghost "missing" and the rest "extra".
        # Pick a real id/name pair from entity_registry.rs.
        ent = vpa.parse_entity_specs(
            (REPO / "src/entity_registry.rs").read_text())
        real_id = sorted(ent)[0]
        registries = {"minecraft:entity_type": {"entries": {
            ent[real_id]: {"protocol_id": real_id},
            "minecraft:zz_parity_probe_ghost": {"protocol_id": 9000},
        }}}
        out = self._run_mode_b(packets, commands, registries)
        reg = out["vanilla"]["registries"]["minecraft:entity_type"]
        missing_names = {m["name"] for m in reg["missing_ids"]}
        # the ghost id present in vanilla but absent from our tables
        self.assertIn("minecraft:zz_parity_probe_ghost", missing_names)
        # the included real entity must not be reported missing
        self.assertNotIn(ent[real_id], missing_names)
        # every other real entity id we implement is "extra" vs this
        # two-entry vanilla fixture -> proves id-level comparison runs
        self.assertGreater(len(reg["extra_ids"]), 100)

    def test_blocks_json_match(self):
        packets = self._minimal_packets()
        commands = {"children": {"give": {"type": "literal"}}}
        registries = {"minecraft:entity_type": {"entries": {}}}
        out = self._run_mode_b(packets, commands, registries,
                               blocks=_blocks_fixture_from_asset())
        b = out["vanilla"]["blocks"]
        self.assertTrue(b["match"], b)
        self.assertEqual(b["vanilla"]["block_count"], 1060)
        self.assertEqual(b["vanilla"]["state_total"], 26_684)
        self.assertEqual(b["ours"]["state_ceiling"], 26_684)
        self.assertEqual(b["per_block_state_mismatch_count"], 0)

    def test_blocks_json_mismatch_detected(self):
        packets = self._minimal_packets()
        commands = {"children": {"give": {"type": "literal"}}}
        registries = {"minecraft:entity_type": {"entries": {}}}
        # real blocks with WRONG state counts + one ghost block; per-block
        # mismatches are only reported for names present in our asset table
        blocks = {
            "minecraft:stone": {"states": [{"id": 0}, {"id": 1}]},
            "minecraft:air": {"states": [{"id": i} for i in range(5)]},
            "minecraft:zz_parity_probe_block": {"states": [{"id": 0}]},
        }
        out = self._run_mode_b(packets, commands, registries, blocks=blocks)
        b = out["vanilla"]["blocks"]
        self.assertFalse(b["match"])
        self.assertEqual(b["vanilla"]["block_count"], 3)
        self.assertEqual(b["vanilla"]["state_total"], 8)
        names = {m["block"] for m in b["per_block_state_mismatches"]}
        self.assertIn("minecraft:stone", names)
        self.assertIn("minecraft:air", names)
        # non-contiguous state ids must fail closed at parse time
        with tempfile.TemporaryDirectory() as td:
            rdir = _write_reports(Path(td) / "reports", packets, commands,
                                  registries,
                                  blocks={"minecraft:stone": {"states": [
                                      {"id": 0}, {"id": 2}]}})
            with self.assertRaises(vpa.AuditError):
                vpa.load_reports_dir(rdir)

    def test_blocks_json_absent_still_works(self):
        packets = self._minimal_packets()
        commands = {"children": {"give": {"type": "literal"}}}
        registries = {"minecraft:entity_type": {"entries": {}}}
        out = self._run_mode_b(packets, commands, registries)
        self.assertNotIn("blocks", out["vanilla"])


class StrictGateTest(unittest.TestCase):
    """--min-*/--strict flags must exit 1 on any coverage/drift regression."""

    @classmethod
    def setUpClass(cls):
        cls.mode_a = mode_a_cached()

    def _reports_dir(self, td, packets=None, commands=None, registries=None,
                     blocks=None):
        packets = packets if packets is not None else {
            "play": {"serverbound": {}, "clientbound": {}}}
        commands = commands if commands is not None else {
            "children": {"give": {"type": "literal"}}}
        registries = registries if registries is not None else {
            "minecraft:entity_type": {"entries": {}}}
        return _write_reports(Path(td) / "reports", packets, commands,
                              registries, blocks=blocks)

    def _main(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = vpa.main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_min_c2s_handled_below_floor_fails(self):
        # 999 can never be satisfied -> proves the gate catches regressions
        rc, _out, err = self._main(
            ["--repo", str(REPO), "--format", "json",
             "--min-c2s-handled", "999"])
        self.assertEqual(rc, 1)
        self.assertIn("min-c2s-handled", err)
        self.assertIn("999", err)

    def test_min_c2s_handled_at_floor_passes(self):
        rc, _out, _err = self._main(
            ["--repo", str(REPO), "--format", "json",
             "--min-c2s-handled", "0"])
        self.assertEqual(rc, 0)

    def test_min_gamerules_wired_below_floor_fails(self):
        rc, _out, err = self._main(
            ["--repo", str(REPO), "--format", "json",
             "--min-gamerules-wired", "999"])
        self.assertEqual(rc, 1)
        self.assertIn("min-gamerules-wired", err)

    def test_strict_mode_b_drift_fails(self):
        with tempfile.TemporaryDirectory() as td:
            rdir = self._reports_dir(td, packets={
                "play": {"serverbound": {
                    "minecraft:zz_missing_probe": {"protocol_id": 250},
                }, "clientbound": {}}})
            rc, _out, err = self._main(
                ["--repo", str(REPO), "--reports-dir", str(rdir),
                 "--format", "json", "--strict"])
        self.assertEqual(rc, 1)
        self.assertIn("gate violation", err)
        self.assertIn("missing_in_proto", err)

    def test_require_zero_proto_drift_fails_on_blocks(self):
        good = _pick_proto_packet(skip=0)
        with tempfile.TemporaryDirectory() as td:
            rdir = self._reports_dir(
                td,
                packets={"play": {"serverbound": {
                    f"minecraft:{good['name']}": {"protocol_id": good["id"]},
                }, "clientbound": {}}},
                blocks={"minecraft:stone": {"states": [{"id": 0}]}})
            rc, _out, err = self._main(
                ["--repo", str(REPO), "--reports-dir", str(rdir),
                 "--format", "json", "--require-zero-proto-drift"])
        self.assertEqual(rc, 1)
        self.assertIn("block-state drift", err)

    def test_gate_does_not_mask_mode_b_exit_2(self):
        # corrupt reports still exit 2 even with gate flags active
        with tempfile.TemporaryDirectory() as td:
            rdir = Path(td) / "reports"
            rdir.mkdir()
            (rdir / "packets.json").write_text("{bad")
            (rdir / "commands.json").write_text(json.dumps(
                {"children": {"give": {"type": "literal"}}}))
            (rdir / "registries.json").write_text(json.dumps(
                {"minecraft:entity_type": {"entries": {}}}))
            rc, _out, err = self._main(
                ["--repo", str(REPO), "--reports-dir", str(rdir),
                 "--format", "json", "--strict"])
        self.assertEqual(rc, 2)


class FailClosedTest(unittest.TestCase):
    """Bad Mode B inputs must exit 2 without producing a report."""

    def test_missing_vanilla_jar(self):
        with tempfile.TemporaryDirectory() as td:
            missing = Path(td) / "no-such-server.jar"
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = vpa.main(["--repo", str(REPO),
                               "--vanilla-jar", str(missing),
                               "--format", "json"])
        self.assertEqual(rc, 2)
        self.assertIn("vanilla_parity_audit", err.getvalue())

    def test_corrupt_packets_json(self):
        with tempfile.TemporaryDirectory() as td:
            rdir = Path(td) / "reports"
            rdir.mkdir()
            (rdir / "packets.json").write_text("{not json")
            (rdir / "commands.json").write_text(json.dumps(
                {"children": {"give": {"type": "literal"}}}))
            (rdir / "registries.json").write_text(json.dumps(
                {"minecraft:entity_type": {"entries": {}}}))
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = vpa.main(["--repo", str(REPO),
                               "--reports-dir", str(rdir),
                               "--format", "json"])
        self.assertEqual(rc, 2)
        self.assertIn("corrupt", err.getvalue())

    def test_missing_reports_dir(self):
        with tempfile.TemporaryDirectory() as td:
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = vpa.main(["--repo", str(REPO),
                               "--reports-dir", str(Path(td) / "nope"),
                               "--format", "json"])
        self.assertEqual(rc, 2)

    def test_missing_report_file(self):
        with tempfile.TemporaryDirectory() as td:
            rdir = Path(td) / "reports"
            rdir.mkdir()
            (rdir / "packets.json").write_text(json.dumps(
                {"play": {"serverbound": {"x": {"protocol_id": 0}}}}))
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = vpa.main(["--repo", str(REPO),
                               "--reports-dir", str(rdir),
                               "--format", "json"])
        self.assertEqual(rc, 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
