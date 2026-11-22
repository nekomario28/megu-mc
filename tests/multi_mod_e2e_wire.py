#!/usr/bin/env python3
"""Multi-mod archetype live E2E: a real Protocol 767 wire check across six
distinct mod archetypes (Issue #221, Lane G2).

Fixtures mirror `mod-adapter-core/tests/multi_mod_archetypes.rs` exactly:
  1. my-fast-render.jar     -- renamed performance mod (manifest id "sodium"),
                             auto-skipped before its deliberately corrupt
                             `.class` member is ever read
  2. hearthcraft-*.jar      -- NeoForge culinary mod: neoforge.mods.toml +
                             cooking/cutting recipes, item/block tags, loot
                             modifier -> NativeBundle
  3. aether-frontier-*.jar  -- Fabric dimension mod: dimension_type +
                             dimension + ores tag + embedded
                             mc-spine-bundle.json (portal + progression
                             barrier + channel) -> NativeBundle
  4. guard-events-*.jar     -- Fabric mixin mod: real JVM `.class` bytes with
                             @Mixin/@Inject on hurt/placeNewPlayer ->
                             Tier2LiftedShim (EntityDamagePre + PlayerJoin)
  5. custom_creatures.bundle.json -- standalone mc-spine-mod-bundle/1 Megu
                             mod (creature + barrier + region + channel)
  6. invasive-coremod-*.jar -- @Overwrite/@Redirect bytecode -> clean
                             Tier3FallbackRequired (checked in a second
                             isolated boot so it can never mask the others)

`--self-test` validates every synthetic fixture and the wire parsers
offline; live mode boots a real `--features mod-suite` binary twice,
connects protocol767.Client, verifies 0x19 payloads and boot logs, and
requires a graceful exit 0.  Standard library only; ephemeral ports;
TemporaryDirectory everywhere.  MC_PROBE_DEADLINE is honored throughout.
"""

import hashlib
import json
import math
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import time
import zipfile

from protocol767 import Client, ContractFailure, Reader, ROOT, require, string, vint
from wire_smoke import port, status


SYNC_CHANNEL = "mc-spine:mod_sync_v1"
EXHAUSTION_CHANNEL = "appleskin:exhaustion_sync"
SATURATION_CHANNEL = "appleskin:saturation_sync"
CUSTOM_PAYLOAD_ID = 0x19
BUNDLE_SCHEMA = "mc-spine-mod-bundle/1"
DOWNLOAD_URL = ("https://cdn.modrinth.com/data/hearthcraft/versions/1.4.2/"
                "hearthcraft-neoforge-1.4.2.jar")

PERF_JAR = "my-fast-render.jar"
CULINARY_JAR = "hearthcraft-neoforge-1.4.2.jar"
DIMENSION_JAR = "aether-frontier-2.0.jar"
MIXIN_JAR = "guard-events-1.0.jar"
COREMOD_JAR = "invasive-coremod-1.0.jar"
NATIVE_BUNDLE_FILE = "custom_creatures.bundle.json"


def _deadline():
    return float(os.environ.get("MC_PROBE_DEADLINE", "30"))


# ---------------------------------------------------------------------------
# Minimal JVMS class-file builder (@Mixin/@Inject/@Overwrite/@Redirect)
# ---------------------------------------------------------------------------

INJECT_DESC = "Lorg/spongepowered/asm/mixin/injection/Inject;"
AT_DESC = "Lorg/spongepowered/asm/mixin/injection/At;"
MIXIN_DESC = "Lorg/spongepowered/asm/mixin/Mixin;"
OVERWRITE_DESC = "Lorg/spongepowered/asm/mixin/Overwrite;"
REDIRECT_DESC = "Lorg/spongepowered/asm/mixin/injection/Redirect;"
RVA = "RuntimeInvisibleAnnotations"


def _ev_s(value):
    return ("s", value)


def _ev_z(flag):
    return ("Z", flag)


def _ev_at(value, extra=()):
    elems = [("value", _ev_s(value))] + list(extra)
    return ("@", (AT_DESC, elems))


def _mixin_class_bytes(internal, targets, methods):
    """Hand-assembled class file: public class `internal` extending Object,
    each `methods` entry (name, annotation-desc, element pairs) becoming a
    ()V method with a single RuntimeInvisibleAnnotations annotation, plus a
    class-level @Mixin(targets=[...])."""
    items = []

    def utf8(text):
        data = text.encode("utf-8")
        items.append(b"\x01" + struct.pack(">H", len(data)) + data)
        return len(items)

    def clazz(name):
        index = utf8(name)
        items.append(b"\x07" + struct.pack(">H", index))
        return len(items)

    def integer(value):
        items.append(b"\x03" + struct.pack(">i", value))
        return len(items)

    def write_ev(tag, payload):
        if tag == "s":
            return b"s" + struct.pack(">H", utf8(payload))
        if tag == "Z":
            return b"Z" + struct.pack(">H", integer(1 if payload else 0))
        if tag == "@":
            desc, elems = payload
            return b"@" + write_ann(desc, elems)
        if tag == "[":
            return (b"[" + struct.pack(">H", len(payload))
                    + b"".join(write_ev(*item) for item in payload))
        raise ValueError(tag)

    def write_ann(desc, elems):
        out = struct.pack(">H", utf8(desc)) + struct.pack(">H", len(elems))
        for name, ev in elems:
            out += struct.pack(">H", utf8(name)) + write_ev(*ev)
        return out

    this_idx = clazz(internal)
    super_idx = clazz("java/lang/Object")
    rva_idx = utf8(RVA)
    desc_idx = utf8("()V")

    mixin_payload = struct.pack(">H", 1) + write_ann(
        MIXIN_DESC, [("targets", ("[", [_ev_s(t) for t in targets]))])

    method_parts = []
    for name, ann_desc, elems in methods:
        name_idx = utf8(name)
        payload = struct.pack(">H", 1) + write_ann(ann_desc, elems)
        method_parts.append((name_idx, payload))

    out = bytearray()
    out += b"\xca\xfe\xba\xbe" + struct.pack(">H", 0) + struct.pack(">H", 61)
    out += struct.pack(">H", len(items) + 1) + b"".join(items)
    out += (struct.pack(">H", 0x0021) + struct.pack(">H", this_idx)
            + struct.pack(">H", super_idx))
    out += struct.pack(">H", 0)          # interfaces
    out += struct.pack(">H", 0)          # fields
    out += struct.pack(">H", len(method_parts))
    for name_idx, payload in method_parts:
        out += (struct.pack(">H", 0x0001) + struct.pack(">H", name_idx)
                + struct.pack(">H", desc_idx) + struct.pack(">H", 1)
                + struct.pack(">H", rva_idx) + struct.pack(">I", len(payload))
                + payload)
    out += (struct.pack(">H", 1) + struct.pack(">H", rva_idx)
            + struct.pack(">I", len(mixin_payload)) + mixin_payload)
    return bytes(out)


def _plain_class_bytes(internal):
    """A valid empty class with no annotations — zero mixin surface."""
    items = []

    def utf8(text):
        data = text.encode("utf-8")
        items.append(b"\x01" + struct.pack(">H", len(data)) + data)
        return len(items)

    def clazz(name):
        index = utf8(name)
        items.append(b"\x07" + struct.pack(">H", index))
        return len(items)

    this_idx = clazz(internal)
    super_idx = clazz("java/lang/Object")
    desc_idx = utf8("()V")
    name_idx = utf8("init")
    out = bytearray()
    out += b"\xca\xfe\xba\xbe" + struct.pack(">H", 0) + struct.pack(">H", 61)
    out += struct.pack(">H", len(items) + 1) + b"".join(items)
    out += (struct.pack(">H", 0x0021) + struct.pack(">H", this_idx)
            + struct.pack(">H", super_idx))
    out += struct.pack(">H", 0)          # interfaces
    out += struct.pack(">H", 0)          # fields
    out += (struct.pack(">H", 1) + struct.pack(">H", 0x0001)
            + struct.pack(">H", name_idx) + struct.pack(">H", desc_idx)
            + struct.pack(">H", 0))
    out += struct.pack(">H", 0)          # no class attributes
    return bytes(out)


# ---------------------------------------------------------------------------
# The six archetype fixtures (mirror the Rust test exactly)
# ---------------------------------------------------------------------------

def _write_jar(path, members):
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, data in members:
            archive.writestr(name, data)


def _jar_bytes(members):
    import io
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, data in members:
            archive.writestr(name, data)
    return buf.getvalue()


def fabric_manifest(mod_id, version, extra=None):
    manifest = {
        "schemaVersion": 1,
        "id": mod_id,
        "version": version,
        "environment": "*",
    }
    manifest.update(extra or {})
    return json.dumps(manifest)


def perf_jar_members():
    """Archetype 1: renamed perf mod. The manifest-declared `sodium` id is
    the only superseded signal; the `.class` member is deliberately corrupt
    so a wrongly-ordered bytecode scan would fail closed instead of skip."""
    return [
        ("fabric.mod.json", fabric_manifest("sodium", "0.6.13")),
        ("com/fastsodium/Broken.class", b"\xde\xad\xbe\xef not a class"),
    ]


def culinary_jar_members():
    """Archetype 2: NeoForge culinary mod — neoforge.mods.toml manifest,
    cooking + cutting recipes, item/block tags, add_item loot modifier."""
    mods_toml = """modLoader = "javafml"
loaderVersion = "[1,)"
license = "MIT"

[[mods]]
modId = "hearthcraft"
version = "1.4.2"

[[dependencies.hearthcraft]]
modId = "minecraft"
type = "required"
versionRange = "[1.21,1.22)"
side = "BOTH"

[[dependencies.hearthcraft]]
modId = "neoforge"
type = "required"
versionRange = "[21.1,)"
side = "BOTH"
"""
    cooking = {
        "type": "hearthcraft:cooking",
        "ingredients": [{"item": "minecraft:beef"}, {"item": "hearthcraft:onion"},
                        {"tag": "hearthcraft:vegetables"}],
        "result": {"id": "hearthcraft:pot_roast", "count": 1},
        "container": {"item": "minecraft:bowl"},
        "cookingtime": 240,
        "experience": 0.6,
    }
    cutting = {
        "type": "hearthcraft:cutting",
        "ingredients": [{"item": "hearthcraft:pot_roast"}],
        "tool": {"item": "hearthcraft:iron_knife"},
        "result": [{"item": "hearthcraft:roast_slice", "count": 3}],
    }
    item_tags = {"replace": False,
                 "values": ["hearthcraft:onion", "minecraft:carrot",
                            "minecraft:potato"]}
    block_tags = {"replace": False,
                  "values": ["minecraft:fire", "minecraft:campfire",
                             "hearthcraft:stove"]}
    loot = {
        "type": "hearthcraft:add_item",
        "item": "hearthcraft:wild_seeds",
        "chance": 0.4,
        "conditions": [{"condition": "minecraft:loot_table_id",
                        "loot_table_id": "minecraft:blocks/short_grass"}],
    }
    return [
        ("META-INF/neoforge.mods.toml", mods_toml),
        ("com/hearthcraft/HearthcraftMod.class",
         _plain_class_bytes("com/hearthcraft/HearthcraftMod")),
        ("data/hearthcraft/recipe/pot_roast.json", json.dumps(cooking)),
        ("data/hearthcraft/recipe/roast_slices.json", json.dumps(cutting)),
        ("data/hearthcraft/tags/items/vegetables.json", json.dumps(item_tags)),
        ("data/hearthcraft/tags/blocks/heat_sources.json", json.dumps(block_tags)),
        ("data/hearthcraft/loot_modifiers/seeds_from_grass.json", json.dumps(loot)),
    ]


def aether_bundle_document():
    """The mc-spine-mod-bundle/1 document embedded in archetype 3's JAR."""
    return {
        "schema": BUNDLE_SCHEMA,
        "mod_id": "aether_demo",
        "mc_version_range": "[1.21,)",
        "portals": [{
            "name": "aether_demo:skyreach_portal",
            "target_dim": "aether_demo:skyreach",
            "teleport_grace_ticks": 30,
            "trigger": {
                "kind": "vertical_frame",
                "frame_block": "aether_demo:sky_stone",
                "igniter_item": "aether_demo:sky_shard",
                "min_width": 2,
                "min_height": 3,
                "max_width": 9,
                "max_height": 9,
                "portal_block": "aether_demo:skyreach_portal",
            },
        }],
        "progression_barriers": [{
            "id": "aether_demo:storm_lock",
            "dimension": "aether_demo:skyreach",
            "target_biomes": ["aether_demo:storm_peaks"],
            "required_advancements": ["aether_demo:enter_skyreach"],
            "block_break_protected": True,
            "block_place_protected": True,
            "enforce_effects": [{"effect": "minecraft:slowness",
                                 "duration_ticks": 100, "amplifier": 1}],
            "periodic_damage": {"hearts": 0.5, "interval_ticks": 40},
        }],
        "biome_regions": [],
        "creatures": [],
        "custom_channels": ["aether_demo:skyreach_sync"],
    }


def dimension_jar_members():
    """Archetype 3: Fabric dimension mod — dimension_type + dimension +
    ores tag discovered as pure data-driven, embedded bundle installed."""
    dimension_type = {
        "min_y": 0,
        "height": 256,
        "has_skylight": True,
        "has_ceiling": False,
        "natural": True,
        "ambient_light": 0.7,
        "effects": "minecraft:overworld",
        "coordinate_scale": 1.0,
    }
    dimension = {
        "type": "aether_demo:skyreach",
        "generator": {
            "type": "minecraft:noise",
            "settings": "aether_demo:skyreach_noise",
            "biome_source": {"type": "minecraft:fixed",
                             "biome": "aether_demo:storm_peaks"},
        },
    }
    ores = {"replace": False,
            "values": ["aether_demo:zephyr_ore", "aether_demo:cloudium_ore"]}
    return [
        ("fabric.mod.json", fabric_manifest("aether_demo", "2.0.0")),
        ("com/aetherdemo/AetherDemo.class",
         _plain_class_bytes("com/aetherdemo/AetherDemo")),
        ("data/aether_demo/dimension_type/skyreach.json",
         json.dumps(dimension_type)),
        ("data/aether_demo/dimension/skyreach.json", json.dumps(dimension)),
        ("data/aether_demo/tags/blocks/ores.json", json.dumps(ores)),
        ("mc-spine-bundle.json", json.dumps(aether_bundle_document())),
    ]


def mixin_jar_members():
    """Archetype 4: liftable Mixin Fabric mod — two registered mixin classes
    whose @Inject sites bind EntityDamagePre (cancellable HEAD on
    LivingEntity.hurt) and PlayerJoin (TAIL on PlayerList.placeNewPlayer)."""
    config = json.dumps({
        "required": True,
        "package": "com.guard.mixin",
        "compatibilityLevel": "JAVA_17",
        "mixins": ["LivingEntityGuardMixin", "PlayerListGuardMixin"],
    })
    damage = _mixin_class_bytes(
        "com/guard/mixin/LivingEntityGuardMixin",
        ["net.minecraft.world.entity.LivingEntity"],
        [("guard$onHurt", INJECT_DESC, [
            ("method", _ev_s("hurt")),
            ("at", _ev_at("HEAD")),
            ("cancellable", _ev_z(True)),
        ])],
    )
    join = _mixin_class_bytes(
        "com/guard/mixin/PlayerListGuardMixin",
        ["net.minecraft.server.players.PlayerList"],
        [("guard$onJoin", INJECT_DESC, [
            ("method", _ev_s("placeNewPlayer")),
            ("at", _ev_at("TAIL")),
        ])],
    )
    return [
        ("fabric.mod.json",
         fabric_manifest("guard_events", "1.0.0", {"mixins": ["guard.mixins.json"]})),
        ("guard.mixins.json", config),
        ("com/guard/mixin/LivingEntityGuardMixin.class", damage),
        ("com/guard/mixin/PlayerListGuardMixin.class", join),
        ("com/guard/GuardMod.class", _plain_class_bytes("com/guard/GuardMod")),
    ]


def coremod_jar_members():
    """Archetype 6: invasive coremod — @Overwrite on ServerLevel.tick plus a
    @Redirect on the same target; both categorically unliftable -> Tier 3."""
    config = json.dumps({
        "package": "com.coremod.mixin",
        "mixins": ["ServerLevelCoreMixin"],
    })
    core = _mixin_class_bytes(
        "com/coremod/mixin/ServerLevelCoreMixin",
        ["net.minecraft.server.level.ServerLevel"],
        [
            ("tick", OVERWRITE_DESC, []),
            ("coremod$redirectTick", REDIRECT_DESC, [
                ("method", _ev_s("tick")),
                ("at", _ev_at("INVOKE", [(
                    "target",
                    _ev_s("net/minecraft/server/level/ServerLevel.tickBlocks"),
                )])),
            ]),
        ],
    )
    return [
        ("fabric.mod.json",
         fabric_manifest("invasive_coremod", "0.9.9",
                         {"mixins": ["coremod.mixins.json"]})),
        ("coremod.mixins.json", config),
        ("com/coremod/mixin/ServerLevelCoreMixin.class", core),
    ]


def native_bundle_document():
    """Archetype 5: standalone mc-spine-mod-bundle/1 Megu mod — custom
    creature (with tame/heal items), progression barrier, biome region and
    custom payload channel."""
    return {
        "schema": BUNDLE_SCHEMA,
        "mod_id": "custom_creatures",
        "mc_version_range": "[1.21,)",
        "portals": [],
        "progression_barriers": [{
            "id": "custom_creatures:wilds_lock",
            "dimension": "minecraft:overworld",
            "target_biomes": ["custom_creatures:moss_fields"],
            "required_advancements": ["custom_creatures:spot_mossback"],
            "block_break_protected": True,
            "block_place_protected": False,
            "enforce_effects": [{"effect": "minecraft:mining_fatigue",
                                 "duration_ticks": 80, "amplifier": 2}],
            "periodic_damage": None,
        }],
        "biome_regions": [{
            "mod_id": "custom_creatures",
            "dimension": "minecraft:overworld",
            "weight": 900,
            "biome_overrides": {"minecraft:plains": "custom_creatures:moss_fields"},
        }],
        "creatures": [{
            "entity_type": "custom_creatures:mossback",
            "max_health": 26.0,
            "tame_items": ["custom_creatures:moss_biscuit",
                           "#custom_creatures:mossback_treats"],
            "tame_chance": 0.45,
            "breed_items": ["custom_creatures:moss_biscuit"],
            "heal_items": [{"item": "custom_creatures:moss_stew", "heal": 4.0}],
            "supports_wander_mode": True,
            "can_shoulder_perch": False,
            "rideable_when_tamed": False,
            "movement": {"kind": "amphibious",
                         "land_speed": 0.12, "swim_speed": 0.5},
        }],
        "custom_channels": ["custom_creatures:sync_v1"],
    }


def make_fixture(mods, with_coremod):
    """Write the archetype set into `mods`; returns the culinary jar bytes
    used to arm the mod-sync manifest."""
    mods.mkdir()
    (mods / PERF_JAR).write_bytes(_jar_bytes(perf_jar_members()))
    culinary_bytes = _jar_bytes(culinary_jar_members())
    (mods / CULINARY_JAR).write_bytes(culinary_bytes)
    (mods / DIMENSION_JAR).write_bytes(_jar_bytes(dimension_jar_members()))
    (mods / MIXIN_JAR).write_bytes(_jar_bytes(mixin_jar_members()))
    if with_coremod:
        (mods / COREMOD_JAR).write_bytes(_jar_bytes(coremod_jar_members()))
    (mods / NATIVE_BUNDLE_FILE).write_text(
        json.dumps(native_bundle_document()), encoding="utf-8")
    return culinary_bytes


def make_coremod_fixture(mods):
    """Isolated second-boot fixture: only the invasive coremod is present."""
    mods.mkdir()
    (mods / COREMOD_JAR).write_bytes(_jar_bytes(coremod_jar_members()))


def make_mod_sync_manifest(root, jar_bytes):
    entry = {
        "mod_id": "hearthcraft",
        "file_name": CULINARY_JAR,
        "download_url": DOWNLOAD_URL,
        "sha256_hex": hashlib.sha256(jar_bytes).hexdigest(),
        "size_bytes": len(jar_bytes),
    }
    manifest = root / "mod-sync.json"
    manifest.write_text(json.dumps({"entries": [entry]}), encoding="utf-8")
    return manifest, entry


# ---------------------------------------------------------------------------
# Wire parsers (same contract as tests/mod_suite_live_wire.py)
# ---------------------------------------------------------------------------

def custom_payload(packet_id, body):
    require(packet_id == CUSTOM_PAYLOAD_ID, f"custom-payload-id: 0x{packet_id:02x}")
    reader = Reader(body)
    channel = reader.string()
    require(channel, "custom-payload-empty-channel")
    return channel, reader.take(len(body) - reader.pos)


def decode_msy1(data):
    reader = Reader(data)
    require(reader.take(4) == b"MSY1", "mod-sync-magic")
    count = reader.number(">H")
    require(0 < count <= 512, f"mod-sync-entry-count: {count}")
    entries = []
    for _ in range(count):
        mod_id = reader.take(reader.number("B")).decode("utf-8")
        file_name = reader.take(reader.number("B")).decode("utf-8")
        download_url = reader.take(reader.number(">H")).decode("utf-8")
        size_bytes = reader.number(">Q")
        sha256_hex = reader.take(32).hex()
        require(mod_id and file_name.endswith(".jar") and size_bytes > 0,
                "mod-sync-invalid-entry")
        require(download_url.startswith("https://"), "mod-sync-invalid-url")
        entries.append({
            "mod_id": mod_id,
            "file_name": file_name,
            "download_url": download_url,
            "sha256_hex": sha256_hex,
            "size_bytes": size_bytes,
        })
    require(reader.pos == len(data), "mod-sync-trailing-bytes")
    return entries


def decode_appleskin(data):
    require(len(data) == 4, f"appleskin-f32-length: {len(data)}")
    value = struct.unpack(">f", data)[0]
    require(math.isfinite(value) and value >= 0, f"appleskin-f32-value: {value}")
    return value


class ModClient(Client):
    """Capture early play payloads, including those consumed inside login()."""

    def __init__(self, game):
        super().__init__(game, "Alice")
        self.payloads = {}

    def read(self, timeout):
        packet_id, body = super().read(timeout)
        if self.phase == "play" and packet_id == CUSTOM_PAYLOAD_ID:
            channel, payload = custom_payload(packet_id, body)
            if channel == SYNC_CHANNEL:
                self.payloads.setdefault(channel, decode_msy1(payload))
            elif channel in (EXHAUSTION_CHANNEL, SATURATION_CHANNEL):
                self.payloads.setdefault(channel, decode_appleskin(payload))
        return packet_id, body


# ---------------------------------------------------------------------------
# --self-test: offline validation of every fixture + wire parser
# ---------------------------------------------------------------------------

def _require_class(class_bytes, markers, code):
    require(class_bytes[:4] == b"\xca\xfe\xba\xbe", code + "-magic")
    for marker in markers:
        require(marker.encode() in class_bytes, f"{code}-{marker}")


def self_test():
    with tempfile.TemporaryDirectory(prefix="mc-spine-multimod-selftest-") as tmp:
        root = Path(tmp)
        mods = root / "mods"
        culinary_bytes = make_fixture(mods, with_coremod=True)
        manifest, entry = make_mod_sync_manifest(root, culinary_bytes)

        require(json.loads(manifest.read_text())["entries"] == [entry],
                "selftest-json-manifest")
        require(len(entry["sha256_hex"]) == 64, "selftest-sha256-length")

        # Archetype 1: renamed perf mod — manifest hides the superseded id
        # behind a neutral file name; the corrupt .class member is real.
        require(zipfile.is_zipfile(mods / PERF_JAR), "selftest-perf-is-jar")
        with zipfile.ZipFile(mods / PERF_JAR) as archive:
            require(archive.testzip() is None, "selftest-perf-jar-corrupt")
            require(json.loads(archive.read("fabric.mod.json"))["id"] == "sodium",
                    "selftest-perf-manifest-id")
            require(archive.read("com/fastsodium/Broken.class")
                    == b"\xde\xad\xbe\xef not a class",
                    "selftest-perf-corrupt-class")

        # Archetype 2: NeoForge culinary jar — manifest + data members.
        with zipfile.ZipFile(mods / CULINARY_JAR) as archive:
            require(archive.testzip() is None, "selftest-culinary-corrupt")
            names = set(archive.namelist())
            require("META-INF/neoforge.mods.toml" in names,
                    "selftest-culinary-manifest")
            toml = archive.read("META-INF/neoforge.mods.toml").decode()
            require('modId = "hearthcraft"' in toml, "selftest-culinary-toml-id")
            require(json.loads(archive.read(
                "data/hearthcraft/recipe/pot_roast.json"))["type"]
                    == "hearthcraft:cooking", "selftest-culinary-cooking")
            require(json.loads(archive.read(
                "data/hearthcraft/recipe/roast_slices.json"))["type"]
                    == "hearthcraft:cutting", "selftest-culinary-cutting")
            require(json.loads(archive.read(
                "data/hearthcraft/loot_modifiers/seeds_from_grass.json"))["type"]
                    == "hearthcraft:add_item", "selftest-culinary-loot")
            _require_class(archive.read("com/hearthcraft/HearthcraftMod.class"),
                           ["com/hearthcraft/HearthcraftMod", "()V"],
                           "selftest-culinary-class")

        # Archetype 3: dimension data + embedded bundle document.
        with zipfile.ZipFile(mods / DIMENSION_JAR) as archive:
            require(archive.testzip() is None, "selftest-dimension-corrupt")
            dim_type = json.loads(archive.read(
                "data/aether_demo/dimension_type/skyreach.json"))
            require(dim_type["min_y"] == 0 and dim_type["height"] == 256,
                    "selftest-dimension-type")
            dim = json.loads(archive.read("data/aether_demo/dimension/skyreach.json"))
            require(dim["type"] == "aether_demo:skyreach"
                    and dim["generator"]["type"] == "minecraft:noise",
                    "selftest-dimension-entry")
            embedded = json.loads(archive.read("mc-spine-bundle.json"))
            require(embedded == aether_bundle_document()
                    and embedded["schema"] == BUNDLE_SCHEMA,
                    "selftest-embedded-bundle")
            require(embedded["portals"][0]["name"] == "aether_demo:skyreach_portal"
                    and embedded["portals"][0]["trigger"]["kind"] == "vertical_frame",
                    "selftest-embedded-portal")
            require(embedded["progression_barriers"][0]["id"] == "aether_demo:storm_lock",
                    "selftest-embedded-barrier")

        # Archetype 4: real class bytes carrying liftable @Inject sites.
        with zipfile.ZipFile(mods / MIXIN_JAR) as archive:
            require(archive.testzip() is None, "selftest-mixin-corrupt")
            cfg = json.loads(archive.read("guard.mixins.json"))
            require(cfg["mixins"] == ["LivingEntityGuardMixin",
                                      "PlayerListGuardMixin"],
                    "selftest-mixin-config")
            require(json.loads(archive.read("fabric.mod.json"))["mixins"]
                    == ["guard.mixins.json"], "selftest-mixin-manifest")
            _require_class(
                archive.read("com/guard/mixin/LivingEntityGuardMixin.class"),
                [MIXIN_DESC, "net.minecraft.world.entity.LivingEntity",
                 INJECT_DESC, AT_DESC, "hurt", "HEAD", "cancellable",
                 "guard$onHurt", RVA],
                "selftest-mixin-damage-class")
            _require_class(
                archive.read("com/guard/mixin/PlayerListGuardMixin.class"),
                [MIXIN_DESC, "net.minecraft.server.players.PlayerList",
                 INJECT_DESC, "placeNewPlayer", "TAIL"],
                "selftest-mixin-join-class")

        # Archetype 5: standalone Megu bundle document.
        bundle = json.loads((mods / NATIVE_BUNDLE_FILE).read_text())
        require(bundle == native_bundle_document()
                and bundle["schema"] == BUNDLE_SCHEMA,
                "selftest-native-bundle-document")
        require(bundle["creatures"][0]["entity_type"] == "custom_creatures:mossback"
                and bundle["custom_channels"] == ["custom_creatures:sync_v1"]
                and bundle["progression_barriers"][0]["id"]
                    == "custom_creatures:wilds_lock"
                and bundle["biome_regions"][0]["weight"] == 900,
                "selftest-native-bundle-sections")

        # Archetype 6: invasive coremod — real @Overwrite/@Redirect bytecode.
        with zipfile.ZipFile(mods / COREMOD_JAR) as archive:
            require(archive.testzip() is None, "selftest-coremod-corrupt")
            _require_class(
                archive.read("com/coremod/mixin/ServerLevelCoreMixin.class"),
                [MIXIN_DESC, "net.minecraft.server.level.ServerLevel",
                 OVERWRITE_DESC, REDIRECT_DESC, "INVOKE",
                 "net/minecraft/server/level/ServerLevel.tickBlocks",
                 "coremod$redirectTick"],
                "selftest-coremod-class")

        # Independent encoding of the MSY1 wire contract, followed by the
        # same 0x19 channel parser used by the live client.
        raw_id = entry["mod_id"].encode()
        raw_name = entry["file_name"].encode()
        raw_url = entry["download_url"].encode()
        wire = (b"MSY1" + struct.pack(">H", 1)
                + bytes([len(raw_id)]) + raw_id
                + bytes([len(raw_name)]) + raw_name
                + struct.pack(">H", len(raw_url)) + raw_url
                + struct.pack(">Q", entry["size_bytes"])
                + bytes.fromhex(entry["sha256_hex"]))
        channel, body = custom_payload(CUSTOM_PAYLOAD_ID, string(SYNC_CHANNEL) + wire)
        require(channel == SYNC_CHANNEL and decode_msy1(body) == [entry],
                "selftest-msy1-roundtrip")
        for bad in (wire[:3], b"BAD1" + wire[4:], wire[:-1], wire + b"extra"):
            try:
                decode_msy1(bad)
            except (AssertionError, struct.error, UnicodeError):
                pass
            else:
                raise ContractFailure("selftest-invalid-msy1-accepted")
        for channel in (EXHAUSTION_CHANNEL, SATURATION_CHANNEL):
            packet_channel, payload = custom_payload(
                CUSTOM_PAYLOAD_ID, string(channel) + struct.pack(">f", 5.25))
            require(packet_channel == channel and decode_appleskin(payload) == 5.25,
                    "selftest-appleskin-f32")
        for bad in (b"", b"\0\0", struct.pack(">f", float("nan"))):
            try:
                decode_appleskin(bad)
            except AssertionError:
                pass
            else:
                raise ContractFailure("selftest-invalid-appleskin-accepted")
    print("PASS offline 6-archetype synthetic JAR/bundle/class fixtures "
          "and 0x19 parsers", flush=True)


# ---------------------------------------------------------------------------
# Live mode: two real server boots over the wire
# ---------------------------------------------------------------------------

def log_tail(path):
    return path.read_text(errors="replace")[-6500:] if path.exists() else "(no log)"


def _server_env(root, mods, manifest):
    game = port()
    env = os.environ.copy()
    for key in ("MC_SPINE_RCON_PORT", "MC_SPINE_RCON_PASS",
                "MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS", "MC_SPINE_PROGRESSION",
                "MC_SPINE_MOD_SYNC_MIRRORS"):
        env.pop(key, None)
    env.update(
        MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
        MC_SPINE_MODS_DIR=str(mods),
        MC_SPINE_WG="old", MC_SPINE_ENABLE_QUERY="0",
        MC_SPINE_ANVIL_WORLD_DIR="", MC_SPINE_RESOURCE_PACK_URL="",
        MC_SPINE_ANTICHEAT="off",
    )
    if manifest is not None:
        env["MC_SPINE_MOD_SYNC"] = str(manifest)
    else:
        env.pop("MC_SPINE_MOD_SYNC", None)
    return game, env


def _boot(binary, root, mods, manifest):
    """Launch one server with the fixture links and env; returns
    (proc, log_path, game_port) once the status endpoint answers."""
    for name in ("config_packets.bin", "assets"):
        source = ROOT / name
        require(source.exists(), f"server-fixture-missing: {source}")
        target = root / name
        if not target.exists():
            target.symlink_to(source, target_is_directory=source.is_dir())
    game, env = _server_env(root, mods, manifest)
    log_path = root / "server.log"
    output = log_path.open("w")
    proc = subprocess.Popen([str(binary)], cwd=root, env=env,
                            stdin=subprocess.DEVNULL, stdout=output,
                            stderr=subprocess.STDOUT)
    try:
        deadline = time.monotonic() + _deadline()
        while time.monotonic() < deadline:
            require(proc.poll() is None,
                    f"server-exited-during-boot: {proc.returncode}")
            try:
                status(game)
                break
            except (OSError, AssertionError):
                time.sleep(0.1)
        else:
            raise ContractFailure("server-listener-start-timeout")
    except BaseException:
        output.close()
        proc.kill()
        proc.wait(timeout=5)
        raise
    return proc, output, log_path, game


def _shutdown(proc, output):
    if proc.poll() is None:
        proc.terminate()
    try:
        proc.wait(timeout=_deadline())
    except subprocess.TimeoutExpired as exc:
        proc.kill()
        proc.wait(timeout=5)
        output.close()
        raise ContractFailure("server-graceful-shutdown-timeout") from exc
    output.close()


def _assert_clean_logs(log_path):
    logs = log_path.read_text(errors="replace")
    require(not re.search(r"(?i)panicked at|thread .* panicked|stack backtrace", logs),
            "server-panic-in-logs")
    require("culinary scan skipped" not in logs and "mixin scan failed closed" not in logs,
            "live-mod-jar-scan-failed")
    return logs


def run_live(binary):
    require(binary.is_file(), f"compiled-binary-missing: {binary}")
    require(os.access(binary, os.X_OK), f"compiled-binary-not-executable: {binary}")
    _run_live_five(binary)
    _run_live_coremod(binary)


def _run_live_five(binary):
    """Boot 1: archetypes 1-5 share one mods/ directory and one live client."""
    with tempfile.TemporaryDirectory(prefix="mc-spine-multimod-live-") as tmp:
        root = Path(tmp)
        mods = root / "mods"
        culinary_bytes = make_fixture(mods, with_coremod=False)
        manifest, entry = make_mod_sync_manifest(root, culinary_bytes)
        proc, output, log_path, game = _boot(binary, root, mods, manifest)
        try:
            with ModClient(game) as alice:
                alice.login()
                expected_channels = {
                    SYNC_CHANNEL, EXHAUSTION_CHANNEL, SATURATION_CHANNEL,
                }
                if not expected_channels <= alice.payloads.keys():
                    alice.until(
                        lambda *_: expected_channels <= alice.payloads.keys(),
                        "live-mod-payloads-missing-" + repr(
                            expected_channels - alice.payloads.keys()),
                    )
                require(alice.payloads[SYNC_CHANNEL] == [entry],
                        f"live-mod-sync-entry: {alice.payloads[SYNC_CHANNEL]}")
                print("PASS Protocol 767 Alice login -> configuration -> play "
                      "(5-archetype mods dir)", flush=True)
                print("PASS 0x19 mc-spine:mod_sync_v1 MSY1 manifest (1 SHA-256 entry)",
                      flush=True)
                print("PASS 0x19 AppleSkin exhaustion/saturation big-endian f32",
                      flush=True)
        finally:
            _shutdown(proc, output)

        logs = _assert_clean_logs(log_path)
        require(proc.returncode == 0,
                f"server-graceful-shutdown-exit: {proc.returncode}")

        # --- Tier classification line: 4 jars across 3 tiers. ---
        require("[live-mods] 4 jar(s): 1 auto-skipped, 2 native-bundle, "
                "1 tier2-shim, 0 tier3" in logs,
                "live-mod-boot-classification")
        # --- Archetype 2 culinary aggregate (aether's ores tag adds 2 tag
        #     values on top of hearthcraft's 6). ---
        require("[live-mods] culinary: 1 cooking, 1 cutting, 1 loot "
                "modifiers, 0 food specs, 8 tag values" in logs,
                "live-mod-boot-culinary")
        # --- Dual-loader plan: renamed sodium caught on its manifest id. ---
        require("[live-mods] dual-loader: 3 unified, 0 client-only, "
                "1 superseded, 0 rejected, 0 duplicate ids, "
                "0 missing deps, 0 verified damage hooks" in logs,
                "live-mod-dual-loader-plan")
        require(f"rejected {PERF_JAR}" not in logs,
                "live-mod-renamed-jar-rejected")
        # --- Archetype 4: lifted hooks reported at boot. ---
        require("[live-mods] guard-events-1.0.jar (guard_events) tier2 shim: "
                "2 classes, 2 site(s)" in logs,
                "live-mod-tier2-line")
        require("all 2 mixin site(s) lifted onto standard event hooks "
                "(EntityDamagePre, PlayerJoin)" in logs,
                "live-mod-tier2-hooks")
        # --- Bundles registered into the unified runtime: standalone first,
        #     then embedded/synthesized in jar order. ---
        require(re.search(
                r"\[live-mods\] native bundle custom_creatures "
                r"\([^)]*custom_creatures\.bundle\.json\): 0 portals, "
                r"1 progression barriers, 1 regions, 1 creatures, "
                r"1 channels", logs) is not None,
                "live-mod-native-bundle-boot")
        require("[live-mods] native bundle aether_demo "
                "(aether-frontier-2.0.jar): 1 portals, "
                "1 progression barriers, 0 regions, 0 creatures, "
                "1 channels" in logs, "live-mod-embedded-aether")
        require("[live-mods] native bundle hearthcraft "
                "(hearthcraft-neoforge-1.4.2.jar): 0 portals, "
                "0 progression barriers, 0 regions, 0 creatures, "
                "0 channels" in logs, "live-mod-synthesized-hearthcraft")
        require("duplicate mod id" not in logs, "live-mod-duplicate-id")
        require("rejected" not in logs.split("dual-loader:")[0]
                or "0 rejected" in logs, "live-mod-unexpected-rejection")
        require("[live-mods] mod sync manifest armed: 1 entry" in logs,
                "live-mod-boot-sync")
        print("PASS live boot: renamed sodium auto-skipped before its corrupt "
              "class was read; culinary NeoForge + aether dimension jars "
              "native (1 cooking, 1 cutting, 1 loot modifier)", flush=True)
        print("PASS live boot: guard-events Tier 2 (EntityDamagePre + "
              "PlayerJoin lifted); custom_creatures + aether bundles "
              "registered", flush=True)
        print("PASS graceful shutdown (exit 0, no panics, disposable files "
              "cleaned)", flush=True)


def _run_live_coremod(binary):
    """Boot 2: the invasive coremod alone — Tier 3 must be reported cleanly
    and must not crash boot, client login, or graceful shutdown."""
    with tempfile.TemporaryDirectory(prefix="mc-spine-coremod-live-") as tmp:
        root = Path(tmp)
        mods = root / "mods"
        make_coremod_fixture(mods)
        proc, output, log_path, game = _boot(binary, root, mods, None)
        try:
            with ModClient(game) as alice:
                alice.login()
                print("PASS Protocol 767 Alice login -> configuration -> play "
                      "(isolated coremod mods dir)", flush=True)
        finally:
            _shutdown(proc, output)

        logs = _assert_clean_logs(log_path)
        require(proc.returncode == 0,
                f"coremod-server-graceful-shutdown-exit: {proc.returncode}")
        require("[live-mods] 1 jar(s): 0 auto-skipped, 0 native-bundle, "
                "0 tier2-shim, 1 tier3" in logs,
                "coremod-boot-classification")
        require("[live-mods] invasive-coremod-1.0.jar (invasive_coremod) "
                "tier3: 1 classes, 2 site(s)" in logs,
                "coremod-tier3-line")
        # The exact human-readable offending class/method names surface in
        # the boot diagnostics — operators can act on this directly.
        require("com/coremod/mixin/ServerLevelCoreMixin" in logs
                and "net.minecraft.server.level.ServerLevel#tick" in logs
                and "@Overwrite" in logs and "@Redirect" in logs
                and "unliftable" in logs,
                "coremod-tier3-blockers")
        print("PASS live boot: invasive @Overwrite/@Redirect coremod reported "
              "Tier 3 with exact class/method blockers; boot, login and "
              "shutdown all clean (exit 0)", flush=True)


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        self_test()
    elif len(sys.argv) == 2:
        run_live(Path(sys.argv[1]).resolve())
    else:
        raise SystemExit(f"usage: {sys.argv[0]} --self-test | /path/to/mc-spine-mod-suite")


if __name__ == "__main__":
    main()
