#!/usr/bin/env python3
"""Tests for tools/vanilla_parity_audit.py (megu-parity-audit/1).

Mode A runs against the real repository. Mode B is exercised with synthetic
Vanilla-shaped reports/{packets,commands,registries}.json fixtures that
deliberately contain a missing packet, a packet id mismatch, a missing
command root and a missing entity id. Fail-closed behaviour (exit 2) is
checked for a missing --vanilla-jar and corrupt report JSON.
"""

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
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
    """One Mode A scan shared by every test class (it takes ~1 minute)."""
    global _MODE_A_CACHE
    if _MODE_A_CACHE is None:
        _MODE_A_CACHE = vpa.mode_a(REPO)
    return _MODE_A_CACHE


def _write_reports(dirpath, packets, commands, registries):
    dirpath = Path(dirpath)
    dirpath.mkdir(parents=True, exist_ok=True)
    (dirpath / "packets.json").write_text(json.dumps(packets))
    (dirpath / "commands.json").write_text(json.dumps(commands))
    (dirpath / "registries.json").write_text(json.dumps(registries))
    return dirpath


def _pick_proto_packet(phase="play", direction="serverbound", skip=0):
    """A real (name, id) pair from proto/src/data_v767.rs."""
    text = (REPO / "proto/src/data_v767.rs").read_text()
    packets = [p for p in vpa.parse_v767_packets(text)
               if p["phase"] == phase and p["direction"] == direction]
    return packets[skip]


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

    def _run_mode_b(self, packets, commands, registries):
        with tempfile.TemporaryDirectory() as td:
            rdir = _write_reports(Path(td) / "reports", packets, commands,
                                  registries)
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
