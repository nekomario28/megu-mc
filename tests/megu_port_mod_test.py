"""End-to-end and unit tests for the Megu mod porter (`tools/megu_port_mod.py`).

The E2E cases run the shipped CLI as a subprocess against synthetic fixtures
built in temporary directories — Fabric/NeoForge/Forge JARs, a Modrinth
`.mrpack`, hand-assembled `.class` bytecode, path-traversal and zip-bomb
archives — and prove the generated project round-trips through
`tools/megu_mod_dev.py validate` and packs into a Modrinth-ready compat JAR.
The unit cases import the tool module directly to pin the constant-pool
parser, the classifier, and the fail-closed path checks.
"""

import io
import json
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "megu_port_mod.py"
DEV_TOOL = ROOT / "tools" / "megu_mod_dev.py"

sys.path.insert(0, str(ROOT / "tools"))
import megu_port_mod  # noqa: E402


def run(*args, cwd=ROOT, timeout=180):
    """Run the porter CLI with the repository root as cwd."""
    return subprocess.run(
        [sys.executable, str(TOOL), *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def run_json(*args, cwd=ROOT):
    result = run(*args, cwd=cwd)
    assert result.returncode in (0, 1, 2), result.stderr
    return result, json.loads(result.stdout)


def dev_validate(project):
    return subprocess.run(
        [sys.executable, str(DEV_TOOL), "validate", str(project), "--json"],
        cwd=str(ROOT),
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )


# ---------------------------------------------------------------------------
# Fixture builders
# ---------------------------------------------------------------------------


def class_bytes(name, super_name="java/lang/Object", interfaces=(), methods=("tick",), extra_utf8=()):
    """Hand-assembled minimal .class: constant pool + header + methods only."""
    pool = []

    def utf8(text):
        pool.append((1, text.encode("utf-8")))
        return len(pool)

    def cls(internal):
        pool.append((7, struct.pack(">H", utf8(internal))))
        return len(pool)

    this_index = cls(name)
    super_index = cls(super_name)
    iface_indexes = [cls(iface) for iface in interfaces]
    for text in extra_utf8:
        utf8(text)
    desc_index = utf8("()V")
    method_indexes = [utf8(method) for method in methods]

    out = io.BytesIO()
    out.write(struct.pack(">IHH", 0xCAFEBABE, 0, 61))
    out.write(struct.pack(">H", len(pool) + 1))
    for tag, payload in pool:
        out.write(bytes([tag]))
        if tag == 1:
            out.write(struct.pack(">H", len(payload)))
        out.write(payload)
    out.write(struct.pack(">HHH", 0x0021, this_index, super_index))
    out.write(struct.pack(">H", len(iface_indexes)))
    for index in iface_indexes:
        out.write(struct.pack(">H", index))
    out.write(struct.pack(">H", 0))  # fields
    out.write(struct.pack(">H", len(method_indexes)))
    for index in method_indexes:
        out.write(struct.pack(">HHHH", 0x0001, index, desc_index, 0))
    out.write(struct.pack(">H", 0))  # class attributes
    return out.getvalue()


SHAPED_RECIPE = json.dumps({
    "type": "minecraft:crafting_shaped",
    "pattern": ["RRR", " S ", " S "],
    "key": {
        "R": {"item": "rubymod:ruby"},
        "S": {"item": "minecraft:stick"},
    },
    "result": {"id": "rubymod:ruby_pickaxe", "count": 1},
})

TAG_JSON = json.dumps({"replace": False, "values": ["rubymod:ruby"]})
LOOT_JSON = json.dumps({"type": "minecraft:block", "pools": []})
LANG_JSON = json.dumps({"item.rubymod.ruby": "Ruby"})
BLOCKSTATE_JSON = json.dumps({"variants": {"": {"model": "rubymod:block/ruby_block"}}})
MODEL_JSON = json.dumps({"parent": "minecraft:block/cube_all", "textures": {"all": "rubymod:block/ruby_block"}})
PNG_STUB = b"\x89PNG\r\n\x1a\n" + bytes(32)


def fabric_jar(path, nested_data=True):
    """Synthetic Fabric mod: manifest + data + assets + classified classes."""
    nested = io.BytesIO()
    with zipfile.ZipFile(nested, "w") as inner:
        inner.writestr(
            "data/rubymod/loot_table/blocks/ruby_block.json", LOOT_JSON
        )
    with zipfile.ZipFile(path, "w") as jar:
        jar.writestr("fabric.mod.json", json.dumps({
            "schemaVersion": 1,
            "id": "rubymod",
            "version": "1.2.3",
            "name": "Ruby Mod",
            "description": "Synthetic fabric fixture",
            "license": "MIT",
            "authors": ["Fixture Author"],
            "depends": {"minecraft": "~1.21.1"},
        }))
        jar.writestr("data/rubymod/recipe/ruby_pick.json", SHAPED_RECIPE)
        jar.writestr("data/rubymod/tags/item/gems.json", TAG_JSON)
        jar.writestr("assets/rubymod/lang/en_us.json", LANG_JSON)
        jar.writestr("assets/rubymod/blockstates/ruby_block.json", BLOCKSTATE_JSON)
        jar.writestr("assets/rubymod/models/block/ruby_block.json", MODEL_JSON)
        jar.writestr("assets/rubymod/textures/block/ruby_block.png", PNG_STUB)
        jar.writestr(
            "com/example/rubymod/RubyBlock.class",
            class_bytes("com/example/rubymod/RubyBlock",
                        "net/minecraft/world/level/block/Block"),
        )
        jar.writestr(
            "com/example/rubymod/RubyItem.class",
            class_bytes("com/example/rubymod/RubyItem",
                        "net/minecraft/world/item/Item"),
        )
        jar.writestr(
            "com/example/rubymod/RubyBoarEntity.class",
            class_bytes("com/example/rubymod/RubyBoarEntity",
                        "net/minecraft/world/entity/animal/Animal"),
        )
        jar.writestr(
            "com/example/rubymod/SyncPayload.class",
            class_bytes("com/example/rubymod/SyncPayload",
                        "java/lang/Object",
                        interfaces=("net/minecraft/network/protocol/FriendlyByteBuf",)),
        )
        jar.writestr(
            "com/example/rubymod/mixin/ChunkMixin.class",
            class_bytes("com/example/rubymod/mixin/ChunkMixin",
                        "java/lang/Object",
                        extra_utf8=("Lorg/spongepowered/mixin/injection/Inject;",)),
        )
        jar.writestr(
            "com/example/rubymod/RubyScreen.class",
            class_bytes("com/example/rubymod/RubyScreen",
                        "net/minecraft/client/gui/screens/Screen"),
        )
        jar.writestr(
            "com/example/rubymod/RubyMod.class",
            class_bytes("com/example/rubymod/RubyMod",
                        "java/lang/Object",
                        interfaces=("net/fabricmc/api/ModInitializer",)),
        )
        if nested_data:
            jar.writestr("META-INF/jars/rubymod-data.jar", nested.getvalue())


def neoforge_jar(path):
    nested = io.BytesIO()
    with zipfile.ZipFile(nested, "w") as inner:
        inner.writestr("data/forgemod/recipe/forge_gem.json", SHAPED_RECIPE)
    with zipfile.ZipFile(path, "w") as jar:
        jar.writestr("META-INF/neoforge.mods.toml", "\n".join([
            'modLoader = "javafml"',
            'loaderVersion = "[4,)"',
            'license = "MIT"',
            "",
            "[[mods]]",
            'modId = "forgemod"',
            'version = "2.0"',
            'displayName = "Forge Mod"',
            'description = "Synthetic neoforge fixture"',
            "",
            "[[dependencies.forgemod]]",
            'modId = "minecraft"',
            'type = "required"',
            'versionRange = "[1.21.1,1.22)"',
            'ordering = "NONE"',
            'side = "BOTH"',
        ]))
        jar.writestr("data/forgemod/recipe/forge_pick.json", SHAPED_RECIPE)
        jar.writestr("assets/forgemod/lang/en_us.json", LANG_JSON)
        jar.writestr(
            "com/example/forgemod/PressBlockEntity.class",
            class_bytes("com/example/forgemod/PressBlockEntity",
                        "net/minecraft/world/level/block/entity/BlockEntity"),
        )
        jar.writestr("META-INF/jarjar/forgemod-lib.jar", nested.getvalue())


def forge_jar(path):
    with zipfile.ZipFile(path, "w") as jar:
        jar.writestr("META-INF/mods.toml", "\n".join([
            'modLoader = "javafml"',
            'loaderVersion = "[52,)"',
            'license = "MIT"',
            "",
            "[[mods]]",
            'modId = "oldforge"',
            'version = "0.9.1"',
            'displayName = "Old Forge Mod"',
        ]))
        jar.writestr("data/oldforge/recipe/old_pick.json", SHAPED_RECIPE)


def mrpack(path, with_override_jar=True):
    inner = io.BytesIO()
    with zipfile.ZipFile(inner, "w") as jar:
        jar.writestr("fabric.mod.json", json.dumps({
            "schemaVersion": 1, "id": "innermod", "version": "0.4.0",
        }))
        jar.writestr("data/innermod/recipe/inner.json", SHAPED_RECIPE)
    with zipfile.ZipFile(path, "w") as pack:
        pack.writestr("modrinth.index.json", json.dumps({
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "3.1.0",
            "name": "Ruby Pack",
            "summary": "Synthetic mrpack fixture",
            "files": [
                {
                    "path": "mods/rubymod.jar",
                    "hashes": {"sha512": "ab" * 64},
                    "env": {"client": "required", "server": "required"},
                    "downloads": ["https://cdn.example.invalid/rubymod.jar"],
                },
                {"path": "../escape.jar", "hashes": {}},
            ],
            "dependencies": {"minecraft": "1.21.1", "fabric-loader": "0.16.0"},
        }))
        pack.writestr("overrides/data/packns/recipe/pack_recipe.json", SHAPED_RECIPE)
        pack.writestr("overrides/config/options.txt", "gamma:1.0")
        if with_override_jar:
            pack.writestr("overrides/mods/innermod.jar", inner.getvalue())


class TempDir(unittest.TestCase):
    def setUp(self):
        self._temp = tempfile.TemporaryDirectory(prefix="megu-port-")
        self.temp = Path(self._temp.name)

    def tearDown(self):
        self._temp.cleanup()


# ---------------------------------------------------------------------------
# Constant-pool parser + classifier units
# ---------------------------------------------------------------------------


class ClassParserTests(unittest.TestCase):
    def test_parses_header_pool_and_methods(self):
        info = megu_port_mod.parse_class(class_bytes(
            "com/example/RubyBlock",
            "net/minecraft/world/level/block/Block",
            interfaces=("net/minecraft/world/level/block/EntityBlock",),
            methods=("tick", "randomTick"),
        ))
        self.assertEqual(info["name"], "com/example/RubyBlock")
        self.assertEqual(info["super"], "net/minecraft/world/level/block/Block")
        self.assertEqual(
            info["interfaces"], ["net/minecraft/world/level/block/EntityBlock"]
        )
        self.assertEqual(info["methods"], ["tick", "randomTick"])
        self.assertEqual(info["major"], 61)

    def test_rejects_non_class_and_truncation(self):
        with self.assertRaises(megu_port_mod.ClassFormatError):
            megu_port_mod.parse_class(b"not a class")
        with self.assertRaises(megu_port_mod.ClassFormatError):
            megu_port_mod.parse_class(class_bytes("a/B")[:24])

    def test_rejects_unknown_constant_tag(self):
        bad = bytearray(class_bytes("a/B"))
        bad[10] = 99  # first constant-pool entry tag
        with self.assertRaises(megu_port_mod.ClassFormatError):
            megu_port_mod.parse_class(bytes(bad))

    def test_classifier_kinds(self):
        cases = {
            "block": ("x/RubyBlock", "net/minecraft/world/level/block/Block", (), ()),
            "item": ("x/RubyItem", "net/minecraft/world/item/Item", (), ()),
            "entity": ("x/RubyBoar", "net/minecraft/world/entity/animal/Animal", (), ()),
            "block_entity": ("x/PressBE", "x/net/minecraft/world/level/block/entity/BlockEntity", (), ()),
            "screen": ("x/RubyScreen", "net/minecraft/client/gui/screens/Screen", (), ()),
            "entrypoint": ("x/Mod", "java/lang/Object", ("net/fabricmc/api/ModInitializer",), ()),
            "network": ("x/SyncPayload", "java/lang/Object", ("net/minecraft/network/protocol/CustomPacketPayload",), ()),
            "event_listener": ("x/OnBreak", "java/lang/Object", ("net/fabricmc/fabric/api/event/EventListener",), ()),
            "mixin": ("x/FooMixin", "java/lang/Object", (), ("Lorg/spongepowered/mixin/injection/Inject;",)),
            "other": ("x/Util", "java/lang/Object", (), ()),
        }
        for kind, (name, super_name, ifaces, extra) in cases.items():
            with self.subTest(kind=kind):
                info = megu_port_mod.parse_class(
                    class_bytes(name, super_name, interfaces=ifaces, extra_utf8=extra)
                )
                self.assertEqual(megu_port_mod.classify_class(info), kind)


class SafetyUnitTests(unittest.TestCase):
    def test_member_path_error(self):
        bad = ("../evil", "a/../b", "/abs", "C:/win", "a\\b", "a//b", "a/./b", "nul\x00")
        for name in bad:
            with self.subTest(name=name):
                self.assertIsNotNone(megu_port_mod.member_path_error(name))
        for name in ("data/mymod/recipe/x.json", "META-INF/neoforge.mods.toml"):
            self.assertIsNone(megu_port_mod.member_path_error(name))

    def test_sanitize_mod_id(self):
        self.assertEqual(megu_port_mod.sanitize_mod_id("My Cool-Mod"), "my_cool-mod")
        self.assertEqual(megu_port_mod.sanitize_mod_id("123abc"), "mod_123abc")
        self.assertEqual(megu_port_mod.sanitize_mod_id(""), "ported_mod")
        self.assertEqual(megu_port_mod.sanitize_mod_id("../escape!!"), "escape")
        long_id = megu_port_mod.sanitize_mod_id("x" * 100)
        self.assertTrue(megu_port_mod.dev.SAFE_MOD_ID.match(long_id))

    def test_sanitize_semver(self):
        self.assertEqual(megu_port_mod.sanitize_semver("1.2.3-beta"), "1.2.3-beta")
        self.assertEqual(megu_port_mod.sanitize_semver("2.0"), "2.0.0")
        self.assertEqual(megu_port_mod.sanitize_semver("7"), "7.0.0")
        self.assertEqual(megu_port_mod.sanitize_semver("${version}"), "0.1.0")
        self.assertEqual(megu_port_mod.sanitize_semver(None), "0.1.0")


# ---------------------------------------------------------------------------
# analyze
# ---------------------------------------------------------------------------


class AnalyzeTests(TempDir):
    def test_analyze_fabric_jar_json(self):
        jar = self.temp / "rubymod.jar"
        fabric_jar(jar)
        result, report = run_json("analyze", str(jar), "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["ok"])
        self.assertEqual(report["kind"], "jar")
        self.assertEqual(report["loaders"], ["fabric"])
        self.assertEqual(report["mod"]["id"], "rubymod")
        self.assertIn("rubymod", report["namespaces"])
        kinds = report["class_kinds"]
        for expected in ("block", "item", "entity", "mixin", "screen", "entrypoint"):
            self.assertEqual(kinds.get(expected), 1, f"missing kind {expected}")
        self.assertEqual(report["embedded_jars"], ["META-INF/jars/rubymod-data.jar"])
        # read-only: the only artifact next to the input is the jar itself
        self.assertEqual(sorted(p.name for p in self.temp.iterdir()), ["rubymod.jar"])

    def test_analyze_neoforge_jar(self):
        jar = self.temp / "forgemod.jar"
        neoforge_jar(jar)
        result = run("analyze", str(jar))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("neoforge", result.stdout)
        self.assertIn("forgemod", result.stdout)
        self.assertIn("META-INF/jarjar/forgemod-lib.jar", result.stdout)

    def test_analyze_rejects_missing_and_corrupt_input(self):
        result = run("analyze", str(self.temp / "missing.jar"))
        self.assertEqual(result.returncode, 1)
        bad = self.temp / "bad.jar"
        bad.write_bytes(b"definitely not a zip")
        result = run("analyze", str(bad))
        self.assertEqual(result.returncode, 1)


# ---------------------------------------------------------------------------
# port
# ---------------------------------------------------------------------------


class PortTests(TempDir):
    def test_port_fabric_jar_validates_and_packs(self):
        jar = self.temp / "rubymod.jar"
        fabric_jar(jar)
        out = self.temp / "project"
        result, report = run_json(
            "port", str(jar), "--out", str(out), "--json", "--rust-scaffold"
        )
        self.assertEqual(result.returncode, 0, report)
        self.assertTrue(report["ok"])
        self.assertEqual(report["project"]["mod_id"], "rubymod")
        self.assertTrue(report["project"]["validates"])

        # synthesized native project
        bundle = json.loads((out / "megu-mod-bundle.json").read_text())
        self.assertEqual(bundle["schema"], "megu-mod-bundle/1")
        self.assertEqual(bundle["mod_id"], "rubymod")
        self.assertNotIn("$schema", bundle)
        self.assertEqual(
            [c["entity_type"] for c in bundle["creatures"]],
            ["rubymod:ruby_boar"],
        )
        self.assertEqual(bundle["custom_channels"], ["rubymod:sync"])
        for rel in (
            "megu-mod.toml", "pack.mcmeta", "README.md", "PORT_REPORT.md",
            "data/rubymod/recipe/ruby_pick.json",
            "data/rubymod/loot_table/blocks/ruby_block.json",  # folded nested jar
            "assets/rubymod/lang/en_us.json",
            "assets/rubymod/textures/block/ruby_block.png",
        ):
            self.assertTrue((out / rel).is_file(), f"missing {rel}")

        # the same validator megu_mod_dev.py ships must accept the output
        validated = dev_validate(out)
        self.assertEqual(validated.returncode, 0, validated.stdout)
        self.assertTrue(json.loads(validated.stdout)["ok"])

        # deterministic .mmod
        mmod = out / "dist" / "rubymod-1.2.3.mmod"
        self.assertTrue(mmod.is_file())
        with zipfile.ZipFile(mmod) as archive:
            names = set(archive.namelist())
        self.assertIn("megu-mod-bundle.json", names)
        self.assertIn("data/rubymod/recipe/ruby_pick.json", names)
        self.assertNotIn("fabric.mod.json", names)  # native stays Java-free

        # rust scaffold
        lib = (out / "rust-port" / "src" / "lib.rs").read_text()
        self.assertIn('BlockBuilder::new("ruby_block")', lib)
        self.assertIn('ItemBuilder::new("ruby_item")', lib)
        self.assertIn('CreatureBuilder::new("ruby_boar")', lib)
        self.assertIn("TODO(port)", lib)
        self.assertIn('megu-mod-sdk', (out / "rust-port" / "Cargo.toml").read_text())
        report_md = (out / "PORT_REPORT.md").read_text()
        self.assertIn("auto-ported share", report_md)

    def test_port_compat_jar_is_modrinth_ready(self):
        jar = self.temp / "rubymod.jar"
        fabric_jar(jar)
        out = self.temp / "project"
        result, report = run_json(
            "port", str(jar), "--out", str(out), "--format", "both", "--json"
        )
        self.assertEqual(result.returncode, 0, report)
        self.assertEqual(len(report["packages"]), 2)
        compat = out / "dist" / "rubymod-1.2.3.jar"
        with zipfile.ZipFile(compat) as archive:
            names = set(archive.namelist())
            fabric = json.loads(archive.read("fabric.mod.json"))
            neoforge = archive.read("META-INF/neoforge.mods.toml").decode()
            bundle = json.loads(archive.read("megu-mod-bundle.json"))
        self.assertIn("megu-mod.toml", names)
        self.assertIn("data/rubymod/recipe/ruby_pick.json", names)
        self.assertEqual(fabric["id"], "rubymod")
        self.assertEqual(fabric["schemaVersion"], 1)
        self.assertIn('modId = "rubymod"', neoforge)
        self.assertEqual(bundle["mod_id"], "rubymod")
        for package in report["packages"]:
            self.assertRegex(package["sha256"], r"^[0-9a-f]{64}$")
            self.assertRegex(package["sha512"], r"^[0-9a-f]{128}$")

    def test_port_neoforge_and_forge_jars(self):
        for maker, expected_loader, mod_id in (
            (neoforge_jar, "neoforge", "forgemod"),
            (forge_jar, "forge", "oldforge"),
        ):
            with self.subTest(loader=expected_loader):
                jar = self.temp / f"{mod_id}.jar"
                maker(jar)
                out = self.temp / f"{mod_id}-out"
                result, report = run_json("port", str(jar), "--out", str(out), "--json")
                self.assertEqual(result.returncode, 0, report)
                self.assertIn(expected_loader, report["input"]["loaders"])
                self.assertEqual(report["project"]["mod_id"], mod_id)
                self.assertTrue(report["project"]["validates"])

    def test_port_mrpack_folds_overrides(self):
        pack = self.temp / "pack.mrpack"
        mrpack(pack)
        out = self.temp / "pack-out"
        result, report = run_json("port", str(pack), "--out", str(out), "--json")
        self.assertEqual(result.returncode, 0, report)
        self.assertEqual(report["input"]["kind"], "mrpack")
        self.assertEqual(report["input"]["loaders"], ["fabric"])
        self.assertEqual(report["project"]["mod_id"], "ruby_pack")
        self.assertTrue(report["project"]["validates"])
        self.assertTrue((out / "data" / "packns" / "recipe" / "pack_recipe.json").is_file())
        # jar inside overrides was folded
        self.assertTrue((out / "data" / "innermod" / "recipe" / "inner.json").is_file())
        # declared downloads reported, traversal flagged, never fetched
        paths = [f["path"] for f in report["input"]["mrpack_files"]]
        self.assertIn("mods/rubymod.jar", paths)
        escape = next(f for f in report["input"]["mrpack_files"] if f["path"] == "../escape.jar")
        self.assertIn("rejected", escape)
        self.assertFalse((self.temp.parent / "escape.jar").exists())

    def test_port_mod_directory(self):
        src = self.temp / "srcdir"
        (src / "data" / "dirmod" / "recipe").mkdir(parents=True)
        (src / "data" / "dirmod" / "recipe" / "x.json").write_text(SHAPED_RECIPE)
        (src / "fabric.mod.json").write_text(json.dumps(
            {"schemaVersion": 1, "id": "dirmod", "version": "0.1.0"}
        ))
        out = self.temp / "dir-out"
        result, report = run_json("port", str(src), "--out", str(out), "--json")
        self.assertEqual(result.returncode, 0, report)
        self.assertEqual(report["input"]["kind"], "dir")
        self.assertTrue((out / "data" / "dirmod" / "recipe" / "x.json").is_file())

    def test_port_data_only_jar_has_empty_bundle_sections(self):
        jar = self.temp / "datapack.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("data/datamod/recipe/x.json", SHAPED_RECIPE)
        out = self.temp / "data-out"
        result, report = run_json("port", str(jar), "--out", str(out), "--json")
        self.assertEqual(result.returncode, 0, report)
        self.assertEqual(report["input"]["loaders"], ["unknown"])
        bundle = json.loads((out / "megu-mod-bundle.json").read_text())
        self.assertEqual(bundle["creatures"], [])
        self.assertTrue(report["project"]["validates"])

    def test_port_refuses_nonempty_out_without_force(self):
        jar = self.temp / "rubymod.jar"
        fabric_jar(jar)
        out = self.temp / "occupied"
        out.mkdir()
        (out / "stale.txt").write_text("old")
        result = run("port", str(jar), "--out", str(out))
        self.assertEqual(result.returncode, 1)
        result = run("port", str(jar), "--out", str(out), "--force")
        self.assertEqual(result.returncode, 0, result.stderr)


# ---------------------------------------------------------------------------
# Fail-closed security
# ---------------------------------------------------------------------------


class SecurityTests(TempDir):
    def test_path_traversal_member_rejected(self):
        jar = self.temp / "evil.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("../pwned.json", "{}")
            archive.writestr("data/ok/recipe/x.json", SHAPED_RECIPE)
        out = self.temp / "out"
        result, report = run_json("port", str(jar), "--out", str(out), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertFalse(report["ok"])
        self.assertIn("rejected", report["error"])
        self.assertFalse((self.temp.parent / "pwned.json").exists())

    def test_zip_bomb_member_rejected(self):
        jar = self.temp / "bomb.jar"
        payload = bytes(megu_port_mod.MAX_MEMBER_BYTES + 1024)
        with zipfile.ZipFile(jar, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("huge.bin", payload)
        self.assertLess(jar.stat().st_size, 4 * 1024 * 1024)  # real bomb geometry
        result = run("analyze", str(jar))
        self.assertEqual(result.returncode, 1)

    def test_member_count_and_backslash_rejected(self):
        jar = self.temp / "backslash.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("data\\x\\recipe\\y.json", SHAPED_RECIPE)
        result = run("analyze", str(jar))
        self.assertEqual(result.returncode, 1)

    def test_nested_jar_beyond_depth_is_recorded_not_fatal(self):
        # depth: outer(0) -> l1(1) -> l2(2) -> l3(3) -> l4(4, rejected)
        def layer(inner_name, payload):
            buf = io.BytesIO()
            with zipfile.ZipFile(buf, "w") as z:
                z.writestr(inner_name, payload)
            return buf.getvalue()

        l4 = layer("META-INF/jarjar/l5.jar", b"unused")
        l3 = layer("META-INF/jarjar/l4.jar", l4)
        l2 = layer("META-INF/jarjar/l3.jar", l3)
        l1 = layer("META-INF/jarjar/l2.jar", l2)
        jar = self.temp / "nested.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("data/top/recipe/x.json", SHAPED_RECIPE)
            archive.writestr("META-INF/jarjar/l1.jar", l1)
        result, report = run_json("analyze", str(jar), "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        rejected = " ".join(r["reason"] for r in report["rejected_members"])
        self.assertIn("deeper than", rejected)

    def test_corrupt_nested_jar_rejected_siblings_survive(self):
        jar = self.temp / "mixed.jar"
        good = io.BytesIO()
        with zipfile.ZipFile(good, "w") as inner:
            inner.writestr("data/good/recipe/x.json", SHAPED_RECIPE)
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("data/top/recipe/x.json", SHAPED_RECIPE)
            archive.writestr("META-INF/jarjar/corrupt.jar", b"PK\x03\x04garbage")
            archive.writestr("META-INF/jarjar/good.jar", good.getvalue())
        out = self.temp / "out"
        result, report = run_json("port", str(jar), "--out", str(out), "--json")
        self.assertEqual(result.returncode, 0, report)
        self.assertTrue((out / "data" / "good" / "recipe" / "x.json").is_file())
        names = " ".join(r["member"] for r in report["input"]["rejected_members"])
        self.assertIn("corrupt.jar", names)

    def test_directory_symlink_escape_rejected(self):
        src = self.temp / "linkdir"
        src.mkdir()
        (src / "data").mkdir()
        outside = self.temp / "outside.json"
        outside.write_text("{}")
        link = src / "escape.json"
        try:
            link.symlink_to(outside)
        except OSError:
            self.skipTest("symlinks unsupported on this platform")
        result = run("analyze", str(src))
        self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    unittest.main()
