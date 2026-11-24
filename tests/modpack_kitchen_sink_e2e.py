#!/usr/bin/env python3
"""TEST-KITCHEN-SINK-35MOD-STRESS -- end-to-end kitchen-sink modpack suite.

Generates a 35+ archive mixed modpack in a temp dir -- 30 synthetic
NeoForge/Fabric ``.jar`` files (tech, culinary, magic, colossal creatures,
worldgen, QoL, superseded perf shims, config libraries, a Jar-in-Jar
container, plus hostile traversal/corrupt siblings and ``*.disabled``
sidecars) and 5 native ``.mmod`` archives built through
``tools/megu_mod_dev.py`` (``init`` -> ``validate`` -> deterministic
``pack``) -- then:

* validates every ``.mmod`` and proves deterministic repacking is
  byte-identical (sha256),
* installs each ``.mmod``'s ``megu-mod-bundle.json`` into ``mods/`` as
  ``<mod_id>.bundle.json`` (the server's native-bundle load path),
* runs ``megu-mod-check --adapt-mods-dir`` on the pack: the clean dir must
  adapt every jar with the four renamed perf jars auto-skipped, and the
  same dir plus the hostile siblings must fail closed (exit 1),
* optionally (``--server-bin`` or ``--live`` with a built binary) boots the
  real ``megu`` server with ``MEGU_MODS_DIR=<pack>`` on an ephemeral port,
  verifies a TCP status ping, per-JAR hostile rejection in ``[live-mods]``
  logs, and a clean SIGTERM shutdown.

Pure stdlib ``unittest``; no third-party pip deps, no committed binary
fixtures -- every archive is generated at runtime.

Usage:
    python3 tests/modpack_kitchen_sink_e2e.py [-v]
    python3 tests/modpack_kitchen_sink_e2e.py --server-bin target/release/megu
    python3 tests/modpack_kitchen_sink_e2e.py --live        # auto-detect megu
"""

from __future__ import annotations

import argparse
import binascii
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
import zlib

ROOT = Path(__file__).resolve().parents[1]
DEV_TOOL = ROOT / "tools" / "megu_mod_dev.py"
FIXED_DATE = (2024, 1, 1, 0, 0, 0)
MMOD_SCHEMA = "megu-mod-bundle/1"

# Populated from argv in main(); --server-bin wins over --live autodetect.
SERVER_BIN: Path | None = None
LIVE_REQUESTED = False


# ---------------------------------------------------------------------------
# synthetic JAR builders (deterministic ZIP output)
# ---------------------------------------------------------------------------

def zip_bytes(entries):
    """Deterministic STORE zip: sorted names, fixed timestamp, unix 0644."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as z:
        for name, payload in sorted(entries):
            info = zipfile.ZipInfo(name, FIXED_DATE)
            info.compress_type = zipfile.ZIP_STORED
            info.external_attr = 0o644 << 16
            data = payload.encode("utf-8") if isinstance(payload, str) else payload
            z.writestr(info, data)
    return buf.getvalue()


def fabric_manifest(mod_id, deps=None):
    doc = {
        "schemaVersion": 1,
        "id": mod_id,
        "version": "1.0.0",
        "name": mod_id,
        "environment": "*",
        "entrypoints": {},
        "license": "MIT",
    }
    if deps:
        doc["depends"] = deps
    return json.dumps(doc)


def neoforge_manifest(mod_id, deps=None):
    out = (
        'modLoader="javafml"\nloaderVersion="[2,)"\nlicense="MIT"\n'
        "[[mods]]\n"
        f'modId="{mod_id}"\nversion="1.0.0"\ndisplayName="{mod_id}"\n'
    )
    for dep in deps or []:
        out += (
            f'[[dependencies.{mod_id}]]\nmodId="{dep}"\n'
            'type="optional"\nside="BOTH"\n'
        )
    return out


def manifest_for(mod_id, loader, deps=None):
    if loader == "fabric":
        return ("fabric.mod.json", fabric_manifest(mod_id, deps))
    return ("META-INF/neoforge.mods.toml", neoforge_manifest(mod_id, deps))


def jar(mod_id, loader, entries, deps=None):
    return [(manifest_for(mod_id, loader, deps)), *entries]


def recipe_json(namespace, name, kind="smelting"):
    return {
        f"data/{namespace}/recipe/{name}.json": json.dumps({
            "type": f"{namespace}:{kind}",
            "ingredients": [{"item": f"{namespace}:raw_{name}"}],
            "result": {"id": f"{namespace}:{name}", "count": 1},
            "cookingtime": 160,
        })
    }


def tag_json(namespace, domain, name, values):
    return {
        f"data/{namespace}/tags/{domain}/{name}.json": json.dumps({
            "replace": False,
            "values": values,
        })
    }


def lang_json(namespace, code, entries):
    return {f"assets/{namespace}/lang/{code}.json": json.dumps(entries, ensure_ascii=False)}


def loot_modifier_json(namespace, name, kind, **extra):
    doc = {"type": f"{namespace}:{kind}", "conditions": []}
    doc.update(extra)
    return {f"data/{namespace}/loot_modifiers/{name}.json": json.dumps(doc)}


def embedded_bundle(mod_id, **over):
    doc = {
        "schema": MMOD_SCHEMA,
        "mod_id": mod_id,
        "mc_version_range": "[1.21,)",
        "portals": [],
        "progression_barriers": [],
        "biome_regions": [],
        "creatures": [],
        "custom_channels": [],
    }
    doc.update(over)
    return {"megu-bundle.json": json.dumps(doc)}


def minimal_class(internal_name):
    """A structurally valid Java 8 `.class`: `public <internal>` with one
    `public void <init>() { return; }` method -- real bytecode surface the
    adapter's mixin scan can parse (and fail closed on, never execute)."""
    import struct

    def utf8(s):
        raw = s.encode("utf-8")
        return b"\x01" + struct.pack(">H", len(raw)) + raw

    def clazz(idx):
        return b"\x07" + struct.pack(">H", idx)

    cp = (utf8(internal_name) + clazz(1)
          + utf8("java/lang/Object") + clazz(3)
          + utf8("<init>") + utf8("()V") + utf8("Code"))
    code_body = (struct.pack(">HHI", 0, 1, 1) + b"\xb1"
                 + struct.pack(">HH", 0, 0))
    code_attr = struct.pack(">HI", 7, len(code_body)) + code_body
    method = struct.pack(">HHHH", 0x0001, 5, 6, 1) + code_attr
    out = bytearray()
    out += b"\xca\xfe\xba\xbe" + struct.pack(">HH", 0, 52)
    out += struct.pack(">H", 8) + cp
    out += struct.pack(">HHH", 0x0021, 2, 4)
    out += struct.pack(">H", 0)          # interfaces
    out += struct.pack(">H", 0)          # fields
    out += struct.pack(">H", 1) + method
    out += struct.pack(">H", 0)          # class attributes
    return bytes(out)


def png_16x16_rgba(seed=0xC0):
    """Hand-rolled 16x16 RGBA PNG (zlib + crc32, no deps)."""
    sig = b"\x89PNG\r\n\x1a\n"

    def chunk(tag, data):
        return (len(data).to_bytes(4, "big") + tag + data
                + binascii.crc32(tag + data).to_bytes(4, "big"))

    ihdr = (16).to_bytes(4, "big") + (16).to_bytes(4, "big") + b"\x08\x06\x00\x00\x00"
    rows = bytearray()
    for y in range(16):
        rows.append(0)
        for x in range(16):
            rows += bytes([(seed + x * 13) & 0xFF, (seed + y * 11) & 0xFF,
                           (seed + x * y) & 0xFF, 0xFF])
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(rows))) \
        + chunk(b"IEND", b"")


# ---------------------------------------------------------------------------
# the 30-jar kitchen-sink pack
# ---------------------------------------------------------------------------

def build_jars():
    """-> {filename: bytes}; 30 valid jars across every mod category."""
    out = {}

    # --- tech & automation (4) ---
    out["gearforged-neoforge-2.1.0.jar"] = zip_bytes(jar("gearforged", "neoforge", [
        ("com/gearforged/ForgeMod.class", minimal_class("com/gearforged/ForgeMod")),
        *recipe_json("gearforged", "steel_ingot", "crushing").items(),
        *tag_json("gearforged", "item", "c/ingots/steel",
                  ["gearforged:steel_ingot"]).items(),
        *tag_json("gearforged", "item", "c/gears", ["gearforged:steel_gear"]).items(),
        *lang_json("gearforged", "en_us",
                   {"item.gearforged.steel_gear": "Steel Gear"}).items(),
    ]))
    out["kineticworks-fabric-1.4.2.jar"] = zip_bytes(jar("kineticworks", "fabric", [
        *recipe_json("kineticworks", "kinetic_crusher", "crushing").items(),
        *tag_json("kineticworks", "item", "c/gears",
                  ["kineticworks:kinetic_gear"]).items(),
        *lang_json("kineticworks", "en_us",
                   {"block.kineticworks.crusher": "Kinetic Crusher"}).items(),
    ]))
    out["conduitry-neoforge-3.0.1.jar"] = zip_bytes(jar("conduitry", "neoforge", [
        *recipe_json("conduitry", "brass_plate").items(),
        *tag_json("conduitry", "item", "plates/brass",
                  ["conduitry:brass_plate"]).items(),
        *lang_json("conduitry", "en_us",
                   {"item.conduitry.brass_plate": "Brass Plate"}).items(),
    ]))
    # Jar-in-Jar container: jarjar metadata + a real nested jar member.
    nested = zip_bytes(jar("nestedlib", "fabric", [
        *lang_json("nestedlib", "en_us", {"x": "y"}).items(),
    ]))
    out["jijforge-neoforge-1.2.0.jar"] = zip_bytes(jar("jijforge", "neoforge", [
        ("META-INF/jarjar/metadata.json", json.dumps({
            "jars": [{"identifier": {"group": "com.jijforge",
                                     "artifact": "nestedlib"},
                      "version": {"artifactVersion": "1.0.0"},
                      "path": "META-INF/jarjar/nestedlib-1.0.0.jar",
                      "isObfuscated": False}],
        })),
        ("META-INF/jarjar/nestedlib-1.0.0.jar", nested),
        *lang_json("jijforge", "en_us",
                   {"mod.jijforge.name": "JiJ Forge"}).items(),
    ]))

    # --- cooking, farming & food (4) ---
    out["hearthdelight-neoforge-1.0.0.jar"] = zip_bytes(jar("hearthdelight", "neoforge", [
        *recipe_json("hearthdelight", "test_soup", "cooking").items(),
        *recipe_json("hearthdelight", "soup_slice", "cutting").items(),
        *loot_modifier_json("hearthdelight", "add_carrot", "add_item",
                            item="minecraft:carrot").items(),
        *tag_json("hearthdelight", "item", "c/foods",
                  ["hearthdelight:test_soup"]).items(),
        *lang_json("hearthdelight", "en_us",
                   {"item.hearthdelight.test_soup": "Hearth Soup"}).items(),
    ]))
    out["spiceharvest-fabric-0.9.3.jar"] = zip_bytes(jar("spiceharvest", "fabric", [
        *recipe_json("spiceharvest", "fermented_spice", "fermenting").items(),
        *tag_json("spiceharvest", "item", "c/crops",
                  ["spiceharvest:spice_leaf"]).items(),
        *tag_json("spiceharvest", "item", "c/foods",
                  ["spiceharvest:fermented_spice"]).items(),
        *lang_json("spiceharvest", "en_us",
                   {"item.spiceharvest.spice_leaf": "Spice Leaf"}).items(),
    ]))
    out["barrelworks-neoforge-1.1.0.jar"] = zip_bytes(jar("barrelworks", "neoforge", [
        *recipe_json("barrelworks", "aged_cider", "fermenting").items(),
        *loot_modifier_json("barrelworks", "add_table", "add_table",
                            table="barrelworks:inject/apple_drop").items(),
        *lang_json("barrelworks", "en_us",
                   {"block.barrelworks.barrel": "Fermenting Barrel"}).items(),
    ]))
    out["cropfields-fabric-2.0.0.jar"] = zip_bytes(jar("cropfields", "fabric", [
        *recipe_json("cropfields", "roasted_corn").items(),
        *tag_json("cropfields", "item", "c/crops/corn",
                  ["cropfields:corn_cob"]).items(),
        *lang_json("cropfields", "en_us",
                   {"item.cropfields.corn_cob": "Corn Cob"}).items(),
    ]))

    # --- magic, spells & rituals (3): embedded megu-bundle.json ---
    out["manaweave-neoforge-1.3.0.jar"] = zip_bytes(jar(
        "manaweave", "neoforge", [
            *embedded_bundle("manaweave",
                             custom_channels=["manaweave:mana_sync"]).items(),
            *lang_json("manaweave", "en_us",
                       {"item.manaweave.mana_crystal": "Mana Crystal"}).items(),
            *tag_json("manaweave", "item", "c/gems/mana",
                      ["manaweave:mana_crystal"]).items(),
        ], deps=["jupiter"]))
    out["spellbound-fabric-2.2.1.jar"] = zip_bytes(jar("spellbound", "fabric", [
        *embedded_bundle("spellbound").items(),
        ("data/spellbound/advancement/story/first_spell.json", json.dumps({
            "display": {"title": {"translate": "advancements.spellbound.first"}},
            "criteria": {"cast": {"trigger": "minecraft:impossible"}},
        })),
        *lang_json("spellbound", "en_us",
                   {"item.spellbound.spell_book": "Spell Book"}).items(),
    ]))
    out["ritualcraft-neoforge-1.0.7.jar"] = zip_bytes(jar(
        "ritualcraft", "neoforge", [
            *embedded_bundle("ritualcraft",
                             progression_barriers=[{
                                 "id": "ritualcraft:attunement",
                                 "advancement": "ritualcraft:attunement",
                                 "blocks": ["ritualcraft:altar"],
                             }]).items(),
            *lang_json("ritualcraft", "en_us",
                       {"block.ritualcraft.altar": "Ritual Altar"}).items(),
        ], deps=["uranus"]))

    # --- bosses, colossal creatures & multi-block forges (3) ---
    out["dragonreign-neoforge-1.5.0.jar"] = zip_bytes(jar(
        "dragonreign", "neoforge", [
            *embedded_bundle("dragonreign",
                             custom_channels=["dragonreign:rig_sync"]).items(),
            *lang_json("dragonreign", "en_us",
                       {"entity.dragonreign.sky_dragon": "Sky Dragon"}).items(),
        ], deps=["architectury", "cloth_config"]))
    out["serpentdeep-fabric-0.8.2.jar"] = zip_bytes(jar("serpentdeep", "fabric", [
        *embedded_bundle("serpentdeep").items(),
        *lang_json("serpentdeep", "en_us",
                   {"entity.serpentdeep.sea_serpent": "Sea Serpent"}).items(),
    ]))
    out["forgecolossus-neoforge-1.0.0.jar"] = zip_bytes(jar("forgecolossus", "neoforge", [
        *embedded_bundle("forgecolossus").items(),
        *recipe_json("forgecolossus", "colossal_anvil", "forging").items(),
        *tag_json("forgecolossus", "item", "c/ingots/colossal",
                  ["forgecolossus:colossal_ingot"]).items(),
        *lang_json("forgecolossus", "en_us",
                   {"block.forgecolossus.anvil": "Colossal Anvil"}).items(),
    ]))

    # --- worldgen, biomes, ores & dimensions (4) ---
    out["zephyria-fabric-1.2.0.jar"] = zip_bytes(jar("zephyria", "fabric", [
        *embedded_bundle("zephyria",
                         biome_regions=[{"id": "zephyria:sky_meadow",
                                         "biomes": ["zephyria:sky_meadow"],
                                         "climate": "temperate"}]).items(),
        ("data/zephyria/dimension/sky_realm.json", json.dumps({
            "type": "zephyria:sky_realm",
            "generator": {"type": "minecraft:noise"},
        })),
        *lang_json("zephyria", "en_us",
                   {"biome.zephyria.sky_meadow": "Sky Meadow"}).items(),
    ]))
    out["oremancy-neoforge-2.0.1.jar"] = zip_bytes(jar("oremancy", "neoforge", [
        *recipe_json("oremancy", "deep_ingot", "blasting").items(),
        *tag_json("oremancy", "block", "c/ores/deep",
                  ["oremancy:deep_ore"]).items(),
        *lang_json("oremancy", "en_us",
                   {"block.oremancy.deep_ore": "Deep Ore"}).items(),
    ]))
    out["biomeweaver-fabric-1.1.5.jar"] = zip_bytes(jar("biomeweaver", "fabric", [
        *embedded_bundle("biomeweaver").items(),
        ("data/biomeweaver/worldgen/biome/glow_forest.json", json.dumps({
            "temperature": 0.7,
            "downfall": 0.8,
        })),
        *lang_json("biomeweaver", "en_us",
                   {"biome.biomeweaver.glow_forest": "Glow Forest"}).items(),
    ]))
    out["voidreach-neoforge-0.9.9.jar"] = zip_bytes(jar("voidreach", "neoforge", [
        *embedded_bundle("voidreach").items(),
        *tag_json("voidreach", "block", "c/ores/void",
                  ["voidreach:void_ore"]).items(),
        *lang_json("voidreach", "en_us",
                   {"block.voidreach.glow_lantern": "Void Lantern"}).items(),
    ]))

    # --- QoL / HUD / tooltip / audio / resource (4) ---
    out["hudplus-neoforge-1.0.0.jar"] = zip_bytes(jar("hudplus", "neoforge", [
        ("assets/hudplus/sounds.json", json.dumps({
            "hudplus.click": {"sounds": [{"name": "hudplus:ui/click"}]},
        })),
        *lang_json("hudplus", "en_us",
                   {"key.hudplus.toggle": "Toggle HUD"}).items(),
        *lang_json("hudplus", "ja_jp",
                   {"key.hudplus.toggle": "HUD切替"}).items(),
    ]))
    out["tooltipper-fabric-2.0.0.jar"] = zip_bytes(jar("tooltipper", "fabric", [
        *lang_json("tooltipper", "en_us",
                   {"tooltip.tooltipper.hint": "Hold Shift"}).items(),
        *lang_json("tooltipper", "ja_jp",
                   {"tooltip.tooltipper.hint": "Shiftを押す"}).items(),
    ]))
    out["soundscapes-neoforge-1.1.0.jar"] = zip_bytes(jar("soundscapes", "neoforge", [
        ("assets/soundscapes/sounds.json", json.dumps({
            "soundscapes.wind": {"sounds": [{"name": "soundscapes:ambient/wind"}]},
        })),
        *lang_json("soundscapes", "en_us",
                   {"sound.soundscapes.wind": "Wind"}).items(),
    ]))
    out["resourcery-fabric-1.0.0.jar"] = zip_bytes(jar("resourcery", "fabric", [
        ("assets/resourcery/textures/block/gem_block.png", png_16x16_rgba()),
        *tag_json("resourcery", "item", "planks",
                  ["resourcery:gem_planks"]).items(),
        *lang_json("resourcery", "en_us",
                   {"block.resourcery.gem_block": "Gem Block"}).items(),
    ]))

    # --- superseded perf mods (4): renamed jars, manifest-declared ids ---
    out["fastmath-lithium-fabric-0.13.0.jar"] = zip_bytes([
        ("fabric.mod.json", fabric_manifest("lithium")),
    ])
    out["quickdraw-sodium-neoforge-0.6.13.jar"] = zip_bytes([
        ("META-INF/neoforge.mods.toml", neoforge_manifest("sodium")),
    ])
    out["memtrim-ferritecore-fabric-7.0.0.jar"] = zip_bytes([
        ("fabric.mod.json", fabric_manifest("ferritecore")),
    ])
    out["bootboost-modernfix-neoforge-5.19.0.jar"] = zip_bytes([
        ("META-INF/neoforge.mods.toml", neoforge_manifest("modernfix")),
    ])

    # --- config/utility library shims (4): manifest + bytecode surface ---
    out["jupiter-fabric-5.11.0.jar"] = zip_bytes(jar("jupiter", "fabric", [
        ("net/jupiter/ConfigApi.class", minimal_class("net/jupiter/ConfigApi")),
    ]))
    out["uranus-neoforge-1.8.0.jar"] = zip_bytes(jar("uranus", "neoforge", [
        ("dev/uranus/Core.class", minimal_class("dev/uranus/Core")),
    ]))
    out["architectury-fabric-13.0.8.jar"] = zip_bytes(jar("architectury", "fabric", [
        ("dev/architectury/Platform.class", minimal_class("dev/architectury/Platform")),
    ]))
    out["clothconfig-neoforge-15.0.140.jar"] = zip_bytes(jar("cloth_config", "neoforge", [
        ("me/shedaniel/cloth/ClothConfig.class", minimal_class("me/shedaniel/cloth/ClothConfig")),
    ]))

    return out


def hostile_jars():
    """Two jars that must never poison the pack."""
    out = {}
    # path-traversal probe member alongside otherwise valid data
    out["evilprobe-traversal-9.9.9.jar"] = zip_bytes([
        ("fabric.mod.json", fabric_manifest("evilprobe")),
        ("../evil.class", minimal_class("evil/Evil")),
        ("assets/evilprobe/lang/en_us.json",
         json.dumps({"x": "y"})),
    ])
    # truncated/corrupt zip payload
    out["truncated-corrupt-0.0.1.jar"] = zip_bytes([
        ("fabric.mod.json", fabric_manifest("corruptmod")),
    ])[:47]
    return out


MMOD_PROJECTS = [
    ("ruby_starter", "starter"),
    ("gem_press", "machine"),
    ("ruby_boar", "creature"),
    ("ruby_shader", "shader"),
    ("quartz_craft", "starter"),
]


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

def find_mod_check():
    here = Path(__file__).resolve().parents[1]
    for rel in ("mod-adapter-core/target/debug/megu-mod-check",
                "mod-adapter-core/target/release/megu-mod-check",
                "target/debug/megu-mod-check",
                "target/release/megu-mod-check"):
        cand = here / rel
        if cand.is_file() and os.access(cand, os.X_OK):
            return cand
    which = shutil.which("megu-mod-check")
    return Path(which) if which else None


def find_megu():
    if SERVER_BIN:
        return SERVER_BIN
    for rel in ("target/release/megu", "target/debug/megu"):
        cand = ROOT / rel
        if cand.is_file() and os.access(cand, os.X_OK):
            return cand
    which = shutil.which("megu")
    return Path(which) if which else None


def run_tool(*args, check=True):
    proc = subprocess.run(
        [sys.executable, str(DEV_TOOL), *args],
        capture_output=True, text=True, timeout=60)
    if check:
        assert proc.returncode == 0, \
            f"megu_mod_dev {' '.join(map(str, args))} -> {proc.returncode}\n" \
            f"{proc.stdout}\n{proc.stderr}"
    return proc


def run_mod_check(binary, *args):
    return subprocess.run([str(binary), *map(str, args)],
                          capture_output=True, text=True, timeout=120)


def build_pack(tmp):
    """Materialize the whole kitchen sink under ``tmp``.

    Returns dict(mods=Path, hostile=Path, projects={id: Path},
                 mmods={id: Path}, bundle_ids=[str]).
    """
    mods = tmp / "mods"
    mods.mkdir()
    for name, payload in build_jars().items():
        (mods / name).write_bytes(payload)
    # disabled sidecars: valid bytes, never scanned as jars
    (mods / "ghostmod-1.0.jar.disabled").write_bytes(
        zip_bytes(jar("ghostmod", "fabric",
                      [*lang_json("ghostmod", "en_us", {"x": "y"}).items()])))
    (mods / "deadmod-1.0.mmod.disabled").write_bytes(b"PK\x03\x04disabled")

    # hostile sibling dir = clean pack + the two hostile jars
    hostile = tmp / "mods_hostile"
    shutil.copytree(mods, hostile)
    for name, payload in hostile_jars().items():
        (hostile / name).write_bytes(payload)

    projects = {}
    mmods = {}
    bundle_ids = []
    if DEV_TOOL.is_file():
        for mod_id, template in MMOD_PROJECTS:
            proj = tmp / "projects" / mod_id
            run_tool("init", mod_id, "--out", proj,
                     "--template", template, "--force")
            run_tool("validate", proj, "--json")
            mmod = mods / f"{mod_id}.mmod"
            run_tool("pack", proj, "--out", mmod,
                     "--deterministic", "--json")
            projects[mod_id] = proj
            mmods[mod_id] = mmod
            # install the native bundle the way a server operator would:
            # extract megu-mod-bundle.json -> mods/<mod_id>.bundle.json
            with zipfile.ZipFile(mmod) as z:
                bundle = json.loads(z.read("megu-mod-bundle.json"))
            assert bundle["schema"] == MMOD_SCHEMA
            bid = bundle.get("mod_id") or mod_id
            (mods / f"{bid}.bundle.json").write_text(
                json.dumps(bundle, indent=2))
            bundle_ids.append(bid)
        for mmod in mmods.values():
            shutil.copy2(mmod, hostile / mmod.name)
        for bid in bundle_ids:
            shutil.copy2(mods / f"{bid}.bundle.json",
                         hostile / f"{bid}.bundle.json")

    return {"mods": mods, "hostile": hostile, "projects": projects,
            "mmods": mmods, "bundle_ids": bundle_ids}


# ---------------------------------------------------------------------------
# test suite
# ---------------------------------------------------------------------------

class KitchenSinkE2E(unittest.TestCase):
    """Single shared temp dir; tests are order-independent read-only."""

    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory(prefix="megu-kitchen-sink-")
        cls.tmp = Path(cls._tmp.name)
        cls.pack = build_pack(cls.tmp)
        cls.mods = cls.pack["mods"]
        cls.hostile = cls.pack["hostile"]
        cls.mod_check = find_mod_check()

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    # -- pack generation ---------------------------------------------------

    def test_pack_has_35plus_diverse_archives(self):
        jars = sorted(self.mods.glob("*.jar"))
        mmods = sorted(self.mods.glob("*.mmod"))
        bundles = sorted(self.mods.glob("*.bundle.json"))
        disabled = (sorted(self.mods.glob("*.jar.disabled"))
                    + sorted(self.mods.glob("*.mmod.disabled")))
        self.assertGreaterEqual(len(jars), 30, [j.name for j in jars])
        self.assertGreaterEqual(len(mmods), 5, [m.name for m in mmods])
        self.assertEqual(len(bundles), 5)
        self.assertEqual(len(disabled), 2)
        total = len(jars) + len(mmods) + len(bundles) + len(disabled)
        self.assertGreaterEqual(total, 35, f"pack has only {total} archives")
        # one of every loader/category probe
        names = {j.name for j in jars}
        for needle in ("gearforged", "hearthdelight", "manaweave",
                       "dragonreign", "zephyria", "hudplus",
                       "fastmath-lithium", "jupiter", "jijforge"):
            self.assertTrue(any(needle in n for n in names), needle)

    def test_generated_jars_are_valid_and_unique(self):
        ids = set()
        for path in sorted(self.mods.glob("*.jar")):
            with zipfile.ZipFile(path) as z:
                members = set(z.namelist())
            self.assertNotIn("../evil.class", members)
            has_fabric = "fabric.mod.json" in members
            has_neoforge = "META-INF/neoforge.mods.toml" in members
            self.assertTrue(has_fabric or has_neoforge, path.name)
            if has_fabric:
                doc = json.loads(zipfile.ZipFile(path)
                                 .read("fabric.mod.json"))
                ids.add(doc["id"])
        # every fabric jar carries a distinct declared mod id
        fabric_count = sum(
            "fabric.mod.json" in zipfile.ZipFile(p).namelist()
            for p in self.mods.glob("*.jar"))
        self.assertEqual(len(ids), fabric_count)

    def test_hostile_dir_extends_clean_pack(self):
        clean_jars = {p.name for p in self.mods.glob("*.jar")}
        hostile_jar_names = {p.name for p in self.hostile.glob("*.jar")}
        self.assertEqual(hostile_jar_names - clean_jars,
                         {"evilprobe-traversal-9.9.9.jar",
                          "truncated-corrupt-0.0.1.jar"})
        self.assertGreaterEqual(len(hostile_jar_names), 32)
        # traversal probe is really inside the zip
        with zipfile.ZipFile(self.hostile / "evilprobe-traversal-9.9.9.jar") as z:
            self.assertIn("../evil.class", z.namelist())
        # truncated jar is not a valid zip
        with self.assertRaises(zipfile.BadZipFile):
            zipfile.ZipFile(self.hostile / "truncated-corrupt-0.0.1.jar")

    # -- .mmod lifecycle ----------------------------------------------------

    @unittest.skipUnless(DEV_TOOL.is_file(),
                         "tools/megu_mod_dev.py not present")
    def test_mmod_projects_validate_clean(self):
        for mod_id, proj in sorted(self.pack["projects"].items()):
            proc = run_tool("validate", proj, "--json")
            doc = json.loads(proc.stdout)
            self.assertTrue(doc["ok"], (mod_id, doc))
            self.assertEqual(doc["mod_id"], mod_id)
            self.assertFalse([d for d in doc["diagnostics"]
                              if d["severity"] == "error"], doc["diagnostics"])

    @unittest.skipUnless(DEV_TOOL.is_file(),
                         "tools/megu_mod_dev.py not present")
    def test_mmod_deterministic_pack_is_byte_identical(self):
        for mod_id, proj in sorted(self.pack["projects"].items()):
            out_a = self.tmp / f"{mod_id}-a.mmod"
            out_b = self.tmp / f"{mod_id}-b.mmod"
            proc_a = run_tool("pack", proj, "--out", out_a,
                              "--deterministic", "--json")
            proc_b = run_tool("pack", proj, "--out", out_b,
                              "--deterministic", "--json")
            sum_a = json.loads(proc_a.stdout)
            sum_b = json.loads(proc_b.stdout)
            self.assertTrue(sum_a["ok"] and sum_b["ok"])
            self.assertEqual(sum_a["sha256"], sum_b["sha256"], mod_id)
            self.assertEqual(out_a.read_bytes(), out_b.read_bytes(), mod_id)
            with zipfile.ZipFile(out_a) as z:
                names = z.namelist()
                self.assertEqual(names, sorted(names), mod_id)
                for required in ("megu-mod.toml", "megu-mod-bundle.json",
                                 "pack.mcmeta"):
                    self.assertIn(required, names, mod_id)
                bundle = json.loads(z.read("megu-mod-bundle.json"))
                self.assertEqual(bundle["schema"], MMOD_SCHEMA)
                self.assertEqual(bundle["mod_id"], mod_id)

    @unittest.skipUnless(DEV_TOOL.is_file(),
                         "tools/megu_mod_dev.py not present")
    def test_installed_bundle_docs_are_valid_and_unique(self):
        seen = set()
        for path in sorted(self.mods.glob("*.bundle.json")):
            doc = json.loads(path.read_text())
            self.assertEqual(doc["schema"], MMOD_SCHEMA, path.name)
            self.assertEqual(path.name, f"{doc['mod_id']}.bundle.json")
            self.assertNotIn(doc["mod_id"], seen)
            seen.add(doc["mod_id"])
        self.assertEqual(len(seen), 5)

    # -- megu-mod-check adaptation -----------------------------------------

    def test_mod_check_adapts_clean_pack(self):
        if self.mod_check is None:
            self.skipTest("megu-mod-check binary not built")
        out_dir = self.tmp / "adapt_out"
        out_dir.mkdir()
        proc = run_mod_check(self.mod_check,
                             "--adapt-mods-dir", self.mods,
                             "--out", out_dir)
        # exit 2: at least one Tier3 fallback in the pack (shims + JiJ +
        # gearforged's .class entrypoint), never a hard failure
        self.assertEqual(proc.returncode, 2,
                         f"{proc.returncode}\n{proc.stdout}\n{proc.stderr}")
        report = json.loads((out_dir / "adaptation-report.json").read_text())
        self.assertEqual(report["schema"], "megu-mods-dir-adaptation/1")
        self.assertEqual(report["jars_found"], 30, report["jars"])
        totals = report["totals"]
        self.assertEqual(totals["auto_skipped"], 4,
                         f"superseded perf jars not skipped: {totals}")
        self.assertGreaterEqual(totals["native_bundle"], 18, totals)
        self.assertGreaterEqual(totals["tier3_fallback_required"], 5, totals)
        self.assertEqual(
            sum(totals.values()), report["jars_found"], totals)
        self.assertEqual(report["verdict"], "requires_fallback")
        # non-jar entries (.mmod, .bundle.json, disabled) counted, not jars
        self.assertGreaterEqual(report["non_jar_entries"], 10,
                                report["non_jar_entries"])
        skipped_ids = {j.get("mod_id") or j.get("inferred_mod_id")
                       for j in report["jars"]
                       if j["tier"] == "auto_skipped"}
        self.assertTrue({"lithium", "sodium", "ferritecore", "modernfix"}
                        <= skipped_ids, skipped_ids)

    def test_mod_check_adaptation_is_deterministic(self):
        if self.mod_check is None:
            self.skipTest("megu-mod-check binary not built")
        reports = []
        bundle_sets = []
        for i in (0, 1):
            out_dir = self.tmp / f"adapt_det_{i}"
            out_dir.mkdir()
            proc = run_mod_check(self.mod_check,
                                 "--adapt-mods-dir", self.mods,
                                 "--out", out_dir)
            self.assertEqual(proc.returncode, 2, proc.stderr)
            reports.append(
                (out_dir / "adaptation-report.json").read_bytes())
            bundle_sets.append(sorted(
                p.name for p in out_dir.glob("*.bundle.json")))
        self.assertEqual(reports[0], reports[1])
        self.assertEqual(bundle_sets[0], bundle_sets[1])
        self.assertGreater(len(bundle_sets[0]), 0)

    def test_mod_check_fail_closed_on_hostile_siblings(self):
        if self.mod_check is None:
            self.skipTest("megu-mod-check binary not built")
        out_dir = self.tmp / "adapt_hostile"
        out_dir.mkdir()
        proc = run_mod_check(self.mod_check,
                             "--adapt-mods-dir", self.hostile,
                             "--out", out_dir)
        # a corrupt sibling fails the whole dir closed (exit 1) rather than
        # silently adapting the other 30+
        self.assertEqual(proc.returncode, 1,
                         f"{proc.returncode}\n{proc.stdout}\n{proc.stderr}")
        self.assertTrue("truncated-corrupt" in proc.stdout + proc.stderr
                        or "evilprobe" in proc.stdout + proc.stderr,
                        proc.stdout[-2000:] + proc.stderr[-2000:])

    def test_scan_mods_dir_lists_pack(self):
        if self.mod_check is None:
            self.skipTest("megu-mod-check binary not built")
        proc = run_mod_check(self.mod_check, "--scan-mods-dir", self.mods)
        self.assertIn(proc.returncode, (0, 2),
                      f"{proc.returncode}\n{proc.stdout}\n{proc.stderr}")
        blob = proc.stdout + proc.stderr
        for needle in ("gearforged", "hearthdelight", "dragonreign"):
            self.assertIn(needle, blob, needle)

    # -- optional live server boot -----------------------------------------

    def _live_binary(self):
        if not (LIVE_REQUESTED or SERVER_BIN):
            self.skipTest("pass --live or --server-bin for the live boot leg")
        binary = find_megu()
        if binary is None or not binary.is_file():
            self.skipTest(f"megu binary not found: {binary}")
        return binary

    def test_live_server_boot_with_full_modpack(self):
        binary = self._live_binary()
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        from wire_smoke import port, status  # noqa: E402

        with tempfile.TemporaryDirectory(prefix="megu-ks-live-") as td:
            run_root = Path(td)
            for name in ("n", "n.bin", "config_packets.bin", "assets",
                         "datapacks"):
                src = ROOT / name
                if src.exists():
                    dst = run_root / name
                    if src.is_dir():
                        dst.symlink_to(src, target_is_directory=True)
                    else:
                        dst.symlink_to(src)

            game = port()
            env = os.environ.copy()
            for key in ("MEGU_RCON_PORT", "MEGU_RCON_PASS",
                        "MEGU_ALLOW_PLAYER_ADMIN_COMMANDS",
                        "MEGU_PROGRESSION", "MEGU_MOD_SYNC_MIRRORS"):
                env.pop(key, None)
            env.update(
                MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
                MEGU_MODS_DIR=str(self.hostile),
                MEGU_WG="old", MEGU_ENABLE_QUERY="0",
                MEGU_ANVIL_WORLD_DIR="", MEGU_RESOURCE_PACK_URL="",
                MEGU_ANTICHEAT="off",
            )
            log_path = run_root / "server.log"
            with log_path.open("w") as log:
                proc = subprocess.Popen(
                    [str(binary)], cwd=run_root, env=env,
                    stdin=subprocess.DEVNULL, stdout=log,
                    stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + 30
                    while time.monotonic() < deadline:
                        self.assertIsNone(
                            proc.poll(),
                            f"server exited during boot: {proc.returncode}\n"
                            + log_path.read_text(errors="replace")[-3000:])
                        try:
                            status(game)
                            break
                        except (OSError, AssertionError):
                            time.sleep(0.1)
                    else:
                        self.fail("server status listener never answered")
                finally:
                    if proc.poll() is None:
                        proc.terminate()
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=5)
                        self.fail("server ignored SIGTERM for >2s")

            logs = log_path.read_text(errors="replace")
            self.assertEqual(proc.returncode, 0,
                             f"nonzero exit {proc.returncode}\n"
                             + logs[-3000:])
            self.assertNotRegex(
                logs, r"(?i)panicked at|thread .* panicked",
                "panic in server log")
            # live bridge saw the hostile dir, recovered per-JAR
            self.assertIn("[live-mods]", logs, logs[-3000:])
            self.assertIn("auto-skipped", logs, logs[-3000:])
            self.assertRegex(logs, r"rejected[^\n]*truncated-corrupt",
                             "corrupt jar not loudly rejected")


# ---------------------------------------------------------------------------
# entrypoint
# ---------------------------------------------------------------------------

def main():
    global SERVER_BIN, LIVE_REQUESTED
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server-bin", type=Path, default=None,
                        help="path to a compiled megu server binary")
    parser.add_argument("--live", action="store_true",
                        help="auto-detect target/{release,debug}/megu and "
                             "run the live boot test")
    parser.add_argument("-v", "--verbose", action="store_true")
    args, rest = parser.parse_known_args()
    SERVER_BIN = args.server_bin
    LIVE_REQUESTED = args.live
    argv = [sys.argv[0]]
    if args.verbose:
        argv.append("-v")
    argv += rest
    unittest.main(argv=argv)


if __name__ == "__main__":
    main()
