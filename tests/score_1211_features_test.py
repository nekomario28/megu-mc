#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Unit tests for tools/score_1211_features.py.

Builds a synthetic mini repository + mini CSV in a temp dir and proves:
  * comments and `#[cfg(test)]` code cannot produce evidence,
  * test-only module files are excluded from the production corpus,
  * COVERED vs PARTIAL vs MISSING vs NEEDS_VERIFY vs MANUAL_MECH are honest,
  * the CLI --json-out schema is stable.
"""

from __future__ import annotations

import csv
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TOOL_PATH = REPO_ROOT / "tools" / "score_1211_features.py"

_spec = importlib.util.spec_from_file_location("score_1211_features", TOOL_PATH)
scorer = importlib.util.module_from_spec(_spec)
sys.modules["score_1211_features"] = scorer
_spec.loader.exec_module(scorer)


# ---------------------------------------------------------------------------
# Synthetic repo fixtures
# ---------------------------------------------------------------------------

DATA_V767 = '''
pub(crate) enum PacketPhase { Handshake, Status, Login, Configuration, Play }
pub(crate) enum PacketDir { Serverbound, Clientbound }
pub(crate) static V767_PACKETS: [(PacketPhase, PacketDir, &str, u32); 3] = [
    (PacketPhase::Play, PacketDir::Clientbound, "spawn_entity", 0x01),
    (PacketPhase::Play, PacketDir::Serverbound, "move_player_pos", 0x1a),
    (PacketPhase::Play, PacketDir::Clientbound, "registered_but_unhandled", 0x03),
];
pub(crate) static V767_BLOCKS: [&str; 2] = ["minecraft:stone", "minecraft:dirt"];
pub(crate) static V767_ITEMS: [&str; 2] = ["minecraft:stone", "minecraft:dirt"];
pub(crate) static V767_ENTITY_TYPES: [&str; 1] = ["minecraft:zombie"];
pub(crate) static V767_MENUS: [&str; 1] = ["minecraft:anvil"];
pub(crate) static V767_COMPONENTS: [&str; 1] = ["minecraft:custom_name"];
'''

CODEC_RS = '''
pub(crate) fn decode(phase: PacketPhase, dir: PacketDir, id: u32) {
    match (phase, dir, id) {
        (_, _, 0x01) => handle_spawn_entity(),
        _ => {}
    }
}
fn handle_spawn_entity() {}
fn packet_name(id: u32) -> &'static str {
    match id {
        0x01 => "spawn_entity",
        _ => "unknown",
    }
}
'''

MENUS_RS = '''
const ITEM_NAMES: &[&str] = &["stone", "dirt", "chest"];
const EFFECT_FREE: &str = "unused";

fn craft3_result(g: &HashMap<i16, Stack>) -> Option<Stack> {
    // "phantom_item" only lives in this comment and must not count
    if g.is_empty() {
        return name_to_item("chest").map(|r| (r, 1, Vec::new()));
    }
    let _ = "commented_out_result";
    None
}

fn name_to_item(n: &str) -> Option<i64> { Some(0) }

fn live_block_entity_id(name: &str) -> Option<&'static str> {
    match name {
        "minecraft:beacon" => Some("beacon"),
        _ => None,
    }
}

const BANNER_PATTERN_REGISTRY: &[&str] = &["base", "border"];

#[cfg(test)]
fn test_only_helper() -> &'static str {
    "phantom_item"
}
'''

COMMANDS_RS = '''
const COMMAND_GAMERULE_DEFAULTS: &[(&str, bool)] = &[
    ("keepInventory", false),
    ("doPatrolSpawning", true),
];

fn command_usage(name: &str) -> &'static str {
    match name.trim_start_matches('/') {
        "give" => "/give <targets> <item>",
        "tp" | "teleport" => "/tp ...",
        _ => "?",
    }
}

fn dispatch(cmd: &str) {
    match cmd {
        "give" => run_give(),
        "tp" | "teleport" => run_tp(),
        _ => {}
    }
}

fn run_data_command(ctx: &CmdCtx, args: &[&str]) -> String {
    match args.first().copied() {
        Some("get") => "got".to_owned(),
        Some("modify") => "modified".to_owned(),
        _ => String::new(),
    }
}
'''

MAIN_RS = '''
fn gamerule_atomic_value(name: &str) -> Option<i32> {
    Some(match name {
        "keepInventory" => 1,
        "doDaylightCycle" => 0,
        _ => return None,
    })
}
'''

ENTITY_RS = '''
fn specs() {
    entity_spec_1211!(0, "zombie", 0.6, 1.95, 1.74, Hostile, 2e+01, true, true, true, 32, 2);
}
'''

EFFECTS_RS = '''
const EFFECT_REGISTRY: &[(&str, u32)] = &[("strength", 1), ("speed", 2)];
'''

BLOCK_REG_RS = '''
pub(crate) fn tag_members(tag: &str) -> &[u32] { &[] }
'''

GHOST_TEST_RS = '''
// reached only via #[cfg(test)] mod ghost_tests; must stay out of prod corpus
fn ghost() -> &'static str { "phantom_root" }
'''

MINI_CSV_ROWS = [
    # id, category, subcategory, element, behavior
    ("BLK-0001", "c", "block", "minecraft:stone (石)", "hard"),
    ("BLK-0002", "c", "block", "minecraft:unobtainium", "hard"),
    ("ITEM-0001", "c", "item", "minecraft:dirt", "stack=64"),
    ("ITEM-0002", "c", "item", "minecraft:phantom_item", "stack=1"),
    ("ENT-0001", "c", "entity_type", "minecraft:zombie", "ai"),
    ("ENT-0002", "c", "entity_type", "minecraft:ghost", "ai"),
    ("GR-0001", "c", "gamerule", "keepInventory", "default=false"),
    ("GR-0002", "c", "gamerule", "doPatrolSpawning", "default=true"),
    ("GR-0003", "c", "gamerule", "noSuchRule", "default=true"),
    ("CMD-0001", "c", "/give", "/give <targets> <item>", "syntax"),
    ("CMD-0002", "c", "/jfr", "/jfr start", "syntax"),
    ("CMD-0003", "c", "/give", "/give <targets> <item> bogusmode", "syntax"),
    ("PKT-0001", "c", "play/S→C", "0x01 spawn_entity", "handled"),
    ("PKT-0002", "c", "play/S→C", "0x02 mystery_packet", "handled"),
    ("PKT-0003", "c", "play/S→C", "0x03 registered_but_unhandled", "handled"),
    ("TAG-0001", "c", "tag/block", "#stone", "tag defined"),
    ("TAG-0002", "c", "tag/block", "#ghost_tag", "tag defined"),
    ("DATA-0001", "c", "banner_pattern", "minecraft:base", "{}"),
    ("DATA-0002", "c", "banner_pattern", "minecraft:void_pattern", "{}"),
    ("REG-0001", "c", "menu", "minecraft:anvil", "present"),
    ("REG-0002", "c", "mob_effect", "minecraft:strength", "present"),
    ("REG-0003", "c", "instrument", "minecraft:admire_goat_horn", "present"),
    ("BIOME-0001", "c", "biome", "minecraft:plains", "temp=0.8"),
    ("RCP-0001", "c", "crafting_shaped", "minecraft:chest", "result=minecraft:chest"),
    ("RCP-0002", "c", "crafting_shaped", "minecraft:unobtainium_recipe", "result=minecraft:unobtainium_recipe"),
    ("MECH-0001", "敵対 Hostile mobs", "敵対 Hostile mobs", "zombie", "burns in sunlight; strength effect"),
    ("MECH-0002", "運用", "server.properties", "max-players", "config"),
    ("MECH-0003", "敵対 Hostile mobs", "敵対 Hostile mobs", "xyzzy_unknown_thing", "gibberish mechanic"),
]

CSV_HEADER = ["id", "category", "subcategory", "element", "behavior", "source_url"]


def build_repo(root: Path) -> None:
    def w(rel: str, text: str) -> None:
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")

    w("proto/src/data_v767.rs", DATA_V767)
    w("proto/src/codec.rs", CODEC_RS)
    w("src/menus_and_items.rs", MENUS_RS)
    w("src/commands_core.rs", COMMANDS_RS)
    w("src/main.rs", MAIN_RS + "\n#[cfg(test)]\nmod ghost_tests;\n")
    w("src/entity_registry.rs", ENTITY_RS)
    w("src/effects.rs", EFFECTS_RS)
    w("src/block_registry.rs", BLOCK_REG_RS)
    w("src/ghost_tests.rs", GHOST_TEST_RS)
    w(
        "assets/blocks_1211.json",
        json.dumps([{"name": "stone"}, {"name": "dirt"}]),
    )
    w(
        "assets/block_states_1211.json",
        json.dumps({"minecraft": {"stone": {}, "dirt": {}}}),
    )
    w(
        "datapacks/vanilla/data/minecraft/tags/block/stone.json",
        json.dumps({"replace": False, "values": ["minecraft:stone"]}),
    )
    w(
        "datapacks/vanilla/data/minecraft/worldgen/biome/plains.json",
        json.dumps({"temperature": 0.8}),
    )
    w(
        "datapacks/vanilla/data/minecraft/worldgen/biome/VANILLA.txt",
        "",
    )


def build_csv(root: Path) -> Path:
    p = root / "features.csv"
    with p.open("w", newline="", encoding="utf-8") as fh:
        wr = csv.writer(fh)
        wr.writerow(CSV_HEADER)
        for row in MINI_CSV_ROWS:
            wr.writerow(list(row) + ["src"])
    return p


def build_datapack_biome_marker(root: Path) -> None:
    # VANILLA_BIOMES lives in datapack.rs in the real repo; emulate it here.
    dp = root / "src" / "datapack.rs"
    dp.write_text(
        'const VANILLA_BIOMES: &[&str] = &["plains", "desert"];\n',
        encoding="utf-8",
    )


class TestLexer(unittest.TestCase):
    def test_comments_do_not_leak_into_literals(self):
        src = 'let a = "real"; // "fake_line"\n/* "fake_block" */ let b = 1;\n'
        clean, skel = scorer.lex_rust(src)
        self.assertIn('"real"', clean)
        self.assertNotIn("fake_line", clean)
        self.assertNotIn("fake_block", clean)
        rf = scorer.RustFile("x.rs", clean, skel)
        self.assertEqual(rf.literals, {"real"})

    def test_cfg_test_items_are_stripped(self):
        src = (
            "fn prod() { let s = \"prod_value\"; }\n"
            "#[cfg(test)]\nmod tests {\n    fn t() { let s = \"test_only_value\"; }\n}\n"
            "#[cfg(not(test))]\nfn keep() { let s = \"kept_value\"; }\n"
        )
        clean, skel = scorer.lex_rust(src)
        clean2, skel2, removed = scorer.strip_cfg_test(clean, skel)
        self.assertIn("prod_value", clean2)
        self.assertIn("kept_value", clean2)
        self.assertNotIn("test_only_value", clean2)
        self.assertTrue(removed)

    def test_test_only_module_file_is_excluded(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            build_repo(root)
            build_datapack_biome_marker(root)
            files, test_files, skipped = scorer.scan_rust_tree(root)
            self.assertIn("src/ghost_tests.rs", test_files)
            self.assertNotIn("src/ghost_tests.rs", files)
            ghost_lits = test_files["src/ghost_tests.rs"].literals
            self.assertIn("phantom_root", ghost_lits)
            for rf in files.values():
                self.assertNotIn("phantom_root", rf.literals)


class TestScoring(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        root = Path(cls._tmp.name)
        cls.repo = root / "repo"
        cls.repo.mkdir()
        build_repo(cls.repo)
        build_datapack_biome_marker(cls.repo)
        cls.csv_path = build_csv(root)
        cls.ev = scorer.Evidence(cls.repo)
        with cls.csv_path.open(newline="", encoding="utf-8") as fh:
            cls.rows = list(csv.DictReader(fh))
        cls.results = {r["id"]: r for r in scorer.score_rows(cls.rows, cls.ev)}

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def status(self, rid: str) -> str:
        return self.results[rid]["status"]

    def test_covered_classification(self):
        # data-backed rows are covered
        self.assertEqual(self.status("BLK-0001"), scorer.COVERED)
        self.assertEqual(self.status("ITEM-0001"), scorer.COVERED)
        self.assertEqual(self.status("ENT-0001"), scorer.COVERED)
        self.assertEqual(self.status("GR-0001"), scorer.COVERED)  # atomic gamerule
        self.assertEqual(self.status("CMD-0001"), scorer.COVERED)
        self.assertEqual(self.status("PKT-0001"), scorer.COVERED)
        self.assertEqual(self.status("TAG-0001"), scorer.COVERED)
        self.assertEqual(self.status("DATA-0001"), scorer.COVERED)
        self.assertEqual(self.status("REG-0001"), scorer.COVERED)
        self.assertEqual(self.status("REG-0002"), scorer.COVERED)
        self.assertEqual(self.status("BIOME-0001"), scorer.COVERED)
        self.assertEqual(self.status("RCP-0001"), scorer.COVERED)

    def test_partial_classification(self):
        self.assertEqual(self.status("GR-0002"), scorer.PARTIAL)  # default only
        self.assertEqual(self.status("CMD-0003"), scorer.PARTIAL)  # bad token
        self.assertEqual(self.status("PKT-0003"), scorer.PARTIAL)  # table only

    def test_missing_classification(self):
        self.assertEqual(self.status("BLK-0002"), scorer.MISSING)
        self.assertEqual(self.status("ITEM-0002"), scorer.MISSING)
        self.assertEqual(self.status("ENT-0002"), scorer.MISSING)
        self.assertEqual(self.status("GR-0003"), scorer.MISSING)
        self.assertEqual(self.status("CMD-0002"), scorer.MISSING)  # /jfr absent
        self.assertEqual(self.status("PKT-0002"), scorer.MISSING)
        self.assertEqual(self.status("TAG-0002"), scorer.MISSING)
        self.assertEqual(self.status("DATA-0002"), scorer.MISSING)
        self.assertEqual(self.status("REG-0003"), scorer.MISSING)
        self.assertEqual(self.status("RCP-0002"), scorer.MISSING)
        # comment-only literal must not leak: phantom_item is MISSING even
        # though the string appears inside a Rust comment in menus_and_items.rs
        self.assertEqual(self.status("ITEM-0002"), scorer.MISSING)

    def test_mech_statuses(self):
        self.assertEqual(self.status("MECH-0001"), scorer.NEEDS_VERIFY)
        self.assertEqual(self.status("MECH-0002"), scorer.MANUAL_MECH)
        self.assertEqual(self.status("MECH-0003"), scorer.MISSING)

    def test_every_row_scored(self):
        self.assertEqual(len(self.results), len(MINI_CSV_ROWS))
        for r in self.results.values():
            self.assertIn(r["status"], scorer.ALL_STATUSES)
            self.assertIsInstance(r["evidence"], list)

    def test_json_out_schema(self):
        out = Path(self._tmp.name) / "scoreboard.json"
        rc = scorer.main(
            [
                "--csv", str(self.csv_path),
                "--repo", str(self.repo),
                "--top-gaps", "5",
                "--json-out", str(out),
                "--quiet",
            ]
        )
        self.assertEqual(rc, 0)
        data = json.loads(out.read_text(encoding="utf-8"))
        for key in (
            "tool", "csv", "repo", "rows_total", "skipped_files",
            "summary", "top_gaps", "rows",
        ):
            self.assertIn(key, data)
        self.assertEqual(data["rows_total"], len(MINI_CSV_ROWS))
        self.assertEqual(len(data["rows"]), len(MINI_CSV_ROWS))
        for p in scorer.PREFIXES:
            self.assertIn(p, data["summary"])
            for field in (
                "covered", "partial", "missing", "needs_verify",
                "manual_mech", "total", "pct",
            ):
                self.assertIn(field, data["summary"][p])
        for r in data["rows"]:
            self.assertIn(r["status"], scorer.ALL_STATUSES)
            self.assertTrue(r["evidence"])
        self.assertTrue(data["top_gaps"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
