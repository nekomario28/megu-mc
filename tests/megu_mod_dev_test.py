"""End-to-end tests for the Megu mod dev kit (`tools/megu_mod_dev.py`).

Everything here runs the shipped CLI as a subprocess against temporary
directories, so the assertions cover the real command-line contract an author
or an AI agent sees: exit statuses, `validate --json` diagnostics, deterministic
`.mmod` bytes, and atomic `watch --once` installs. `sdk/schemas/` is checked
against both the JSON Schema draft rules and the authoritative key list in
`mod-adapter-core/src/unified_bundle.rs`, so schema drift is a red test rather
than a silent IDE/autocomplete mismatch.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "megu_mod_dev.py"
SCHEMA = ROOT / "sdk" / "schemas" / "megu-mod-bundle-v1.schema.json"
EXAMPLE = ROOT / "sdk" / "examples" / "ruby-starter"
RUST_BUNDLE = ROOT / "mod-adapter-core" / "src" / "unified_bundle.rs"

TEMPLATES = ("starter", "machine", "creature", "shader")


def run(*args, cwd=ROOT, timeout=180):
    """Run the CLI with the current interpreter and the repository root as cwd."""
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


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class TempProject(unittest.TestCase):
    """Base class: one temporary directory per test, cleaned up automatically."""

    def setUp(self):
        self._temp = tempfile.TemporaryDirectory(prefix="megu-mod-dev-")
        self.temp = Path(self._temp.name)

    def tearDown(self):
        self._temp.cleanup()

    def init(self, mod_id="ruby_starter", template="starter", extra=(), out=None):
        out = out or (self.temp / template)
        result = run("init", mod_id, "--out", str(out), "--template", template, *extra)
        self.assertEqual(result.returncode, 0, result.stderr)
        return out

    def rewrite_bundle(self, project, mutate):
        path = Path(project) / "megu-mod-bundle.json"
        bundle = json.loads(path.read_text(encoding="utf-8"))
        mutate(bundle)
        path.write_text(json.dumps(bundle, indent=2) + "\n", encoding="utf-8")


class InitTests(TempProject):
    def test_init_scaffolds_all_four_templates(self):
        for template in TEMPLATES:
            with self.subTest(template=template):
                project = self.init(f"{template}_mod", template)
                for name in ("megu-mod.toml", "megu-mod-bundle.json", "README.md", "pack.mcmeta"):
                    self.assertTrue((project / name).is_file(), f"{template}: missing {name}")
                self.assertFalse((project / "META-INF").exists(), "native template must not ship Java manifests")
                self.assertFalse((project / "fabric.mod.json").exists(), "native template must not ship Java manifests")
                validated = run("validate", str(project))
                self.assertEqual(validated.returncode, 0, validated.stdout + validated.stderr)
                bundle = json.loads((project / "megu-mod-bundle.json").read_text(encoding="utf-8"))
                self.assertEqual(bundle["schema"], "megu-mod-bundle/1")
                self.assertEqual(bundle["mod_id"], f"{template}_mod")
                self.assertNotIn("$schema", bundle)

    def test_init_creature_template_declares_a_creature(self):
        project = self.init("boar_mod", "creature")
        bundle = json.loads((project / "megu-mod-bundle.json").read_text(encoding="utf-8"))
        self.assertEqual(len(bundle["creatures"]), 1)
        creature = bundle["creatures"][0]
        self.assertEqual(creature["entity_type"], "boar_mod:ruby_boar")
        self.assertEqual(creature["movement"]["kind"], "land")
        self.assertTrue((project / "data" / "boar_mod" / "tags" / "item" / "ruby_boar_food.json").is_file())

    def test_init_shader_template_writes_wgsl_pack(self):
        project = self.init("shader_mod", "shader")
        self.assertTrue((project / "shaderpacks" / "shader_mod" / "megu-shader.toml").is_file())
        self.assertTrue((project / "shaderpacks" / "shader_mod" / "final.wgsl").is_file())

    def test_init_compat_jar_adds_dual_loader_manifests(self):
        project = self.init("compat_mod", "starter", extra=("--compat-jar",))
        self.assertTrue((project / "META-INF" / "neoforge.mods.toml").is_file())
        fabric = json.loads((project / "fabric.mod.json").read_text(encoding="utf-8"))
        self.assertEqual(fabric["id"], "compat_mod")
        validated = run("validate", str(project))
        self.assertEqual(validated.returncode, 0, validated.stdout + validated.stderr)

    def test_init_rejects_invalid_and_traversal_mod_ids(self):
        for bad in ("../bad", "UpperCase", "with space", "", "a:b", "target/../../escape"):
            with self.subTest(mod_id=bad):
                result = run("init", bad, "--out", str(self.temp / "rejected"))
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertFalse((self.temp / "rejected").exists(), "rejected id must not create a directory")

    def test_init_refuses_to_overwrite_without_force(self):
        project = self.init("keep_mod", "starter")
        blocked = run("init", "keep_mod", "--out", str(project))
        self.assertEqual(blocked.returncode, 2)
        self.assertIn("--force", blocked.stderr)
        forced = run("init", "keep_mod", "--out", str(project), "--force")
        self.assertEqual(forced.returncode, 0, forced.stderr)


class ValidateTests(TempProject):
    def test_example_project_passes_with_zero_errors(self):
        result = run("validate", str(EXAMPLE))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("0 error(s)", result.stdout)

    def test_validate_json_contract_is_agent_friendly(self):
        result, payload = run_json("validate", str(EXAMPLE), "--json")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(set(payload), {"ok", "mod_id", "diagnostics"})
        self.assertIs(payload["ok"], True)
        self.assertEqual(payload["mod_id"], "ruby_starter")
        self.assertEqual(payload["diagnostics"], [])

    def test_validate_reports_broken_json_with_a_fix_hint(self):
        project = self.init()
        (project / "megu-mod-bundle.json").write_text("{ not json", encoding="utf-8")
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertIs(payload["ok"], False)
        codes = {diagnostic["code"] for diagnostic in payload["diagnostics"]}
        self.assertIn("json.parse", codes)
        for diagnostic in payload["diagnostics"]:
            self.assertIn("path", diagnostic)
            self.assertTrue(diagnostic["fix_hint"])

    def test_validate_rejects_invalid_mod_id(self):
        project = self.init()
        self.rewrite_bundle(project, lambda bundle: bundle.update(mod_id="Bad/Id"))
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertIn("bundle.id", {diagnostic["code"] for diagnostic in payload["diagnostics"]})

    def test_validate_rejects_missing_bundle(self):
        project = self.init()
        (project / "megu-mod-bundle.json").unlink()
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertIn("bundle.missing", {diagnostic["code"] for diagnostic in payload["diagnostics"]})

    def test_validate_rejects_dollar_schema_key_like_the_runtime_parser(self):
        project = self.init()
        self.rewrite_bundle(project, lambda bundle: bundle.update({"$schema": "./schema.json"}))
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertIn("bundle.unknown_key", {diagnostic["code"] for diagnostic in payload["diagnostics"]})

    def test_validate_rejects_unknown_section_and_bad_version_range(self):
        project = self.init()
        self.rewrite_bundle(
            project,
            lambda bundle: bundle.update({"blocks": [], "mc_version_range": "not-a-range"}),
        )
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        codes = {diagnostic["code"] for diagnostic in payload["diagnostics"]}
        self.assertIn("bundle.unknown_key", codes)
        self.assertIn("bundle.version_range", codes)

    def test_validate_rejects_duplicate_magic_ids_and_offsets(self):
        project = self.init()
        spell = {
            "id": "ruby_starter:ember",
            "mana_cost": 1,
            "base_cooldown_ticks": 1,
            "cast": {"kind": "instant"},
            "steps": [{"form": {"kind": "self_cast"}, "effects": []}],
        }
        ritual = {
            "id": "ruby_starter:altar",
            "pattern": [
                {"dx": 0, "dy": 0, "dz": 0, "block": "ruby_starter:ruby_block"},
                {"dx": 0, "dy": 0, "dz": 0, "block": "minecraft:gold_block"},
            ],
            "reagents": {"minecraft:diamond": 1},
            "duration_ticks": 20,
            "mana_per_tick": 0.5,
        }
        self.rewrite_bundle(project, lambda bundle: bundle.update({"spells": [spell, spell], "rituals": [ritual]}))
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        codes = {diagnostic["code"] for diagnostic in payload["diagnostics"]}
        self.assertIn("bundle.duplicate_id", codes)
        self.assertIn("bundle.duplicate_offset", codes)

    def test_validate_flags_unsafe_payload_extensions(self):
        project = self.init()
        (project / "payload.exe").write_bytes(b"MZ")
        (project / "libnative.so").write_bytes(b"\x7fELF")
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        unsafe = [diagnostic for diagnostic in payload["diagnostics"] if diagnostic["code"] == "path.unsafe_extension"]
        self.assertEqual(len(unsafe), 2)

    @unittest.skipIf(not hasattr(os, "symlink"), "symlinks unavailable")
    def test_validate_rejects_symlink_escape(self):
        project = self.init()
        outside = self.temp / "outside.json"
        outside.write_text("{}", encoding="utf-8")
        link = project / "data" / "ruby_starter" / "recipe" / "escaped.json"
        try:
            os.symlink(outside, link)
        except OSError as error:  # pragma: no cover - platform policy
            self.skipTest(f"symlink creation failed: {error}")
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertIn("path.symlink_escape", {diagnostic["code"] for diagnostic in payload["diagnostics"]})

    def test_validate_reports_recipe_structure_errors(self):
        project = self.init()
        recipe = project / "data" / "ruby_starter" / "recipe" / "broken.json"
        recipe.write_text(json.dumps({"type": "minecraft:crafting_shaped", "result": {"id": "ruby_starter:ruby"}}), encoding="utf-8")
        result, payload = run_json("validate", str(project), "--json")
        self.assertEqual(result.returncode, 1)
        self.assertIn("recipe.structure", {diagnostic["code"] for diagnostic in payload["diagnostics"]})


class PackTests(TempProject):
    def test_pack_is_byte_identical_across_runs_and_prints_digests(self):
        project = self.init("pack_mod", "starter")
        first = self.temp / "first.mmod"
        second = self.temp / "second.mmod"
        one = run("pack", str(project), "--out", str(first), "--deterministic")
        two = run("pack", str(project), "--out", str(second), "--deterministic")
        self.assertEqual(one.returncode, 0, one.stderr)
        self.assertEqual(two.returncode, 0, two.stderr)
        self.assertEqual(first.read_bytes(), second.read_bytes(), "deterministic packs must be byte-identical")
        self.assertIn(sha256(first), one.stdout)
        payload = first.read_bytes()
        self.assertIn(hashlib.sha512(payload).hexdigest(), one.stdout)
        self.assertNotIn(".part", one.stdout)

    def test_pack_members_are_sorted_native_and_fixed_time(self):
        project = self.init("pack_mod", "starter")
        out = self.temp / "pack_mod.mmod"
        result = run("pack", str(project), "--out", str(out), "--deterministic")
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(out) as archive:
            names = archive.namelist()
            self.assertEqual(names, sorted(names))
            for expected in ("megu-mod.toml", "megu-mod-bundle.json", "pack.mcmeta", "README.md"):
                self.assertIn(expected, names)
            self.assertNotIn("fabric.mod.json", names)
            self.assertNotIn("META-INF/neoforge.mods.toml", names)
            self.assertTrue(any(name.startswith("data/pack_mod/recipe/") for name in names))
            self.assertEqual({info.date_time for info in archive.infolist()}, {(1980, 1, 1, 0, 0, 0)})
            bundle = json.loads(archive.read("megu-mod-bundle.json").decode("utf-8"))
            self.assertEqual(bundle["schema"], "megu-mod-bundle/1")

    def test_pack_compat_jar_generates_dual_loader_manifests(self):
        project = self.init("compat_mod", "starter")
        out = self.temp / "compat_mod.jar"
        result = run("pack", str(project), "--out", str(out), "--compat-jar", "--deterministic")
        self.assertEqual(result.returncode, 0, result.stderr)
        with zipfile.ZipFile(out) as archive:
            names = set(archive.namelist())
            self.assertIn("META-INF/neoforge.mods.toml", names)
            self.assertIn("fabric.mod.json", names)
            fabric = json.loads(archive.read("fabric.mod.json").decode("utf-8"))
            self.assertEqual(fabric["id"], "compat_mod")
            self.assertIn("modId = \"compat_mod\"", archive.read("META-INF/neoforge.mods.toml").decode("utf-8"))

    def test_pack_refuses_to_pack_an_invalid_project(self):
        project = self.init()
        (project / "megu-mod-bundle.json").write_text("{", encoding="utf-8")
        out = self.temp / "should-not-exist.mmod"
        result = run("pack", str(project), "--out", str(out))
        self.assertEqual(result.returncode, 1)
        self.assertFalse(out.exists(), "pack must not write an invalid project")

    def test_pack_default_output_uses_mod_id_and_version(self):
        project = self.init("default_mod", "starter")
        result = run("pack", str(project), "--deterministic")
        self.assertEqual(result.returncode, 0, result.stderr)
        expected = project / "dist" / "default_mod-0.1.0.mmod"
        self.assertTrue(expected.is_file(), result.stdout)
        self.assertIn(str(expected), result.stdout)

    def test_pack_every_template_produces_a_valid_archive(self):
        for template in TEMPLATES:
            with self.subTest(template=template):
                project = self.init(f"{template}_mod", template)
                out = self.temp / f"{template}.mmod"
                result = run("pack", str(project), "--out", str(out), "--deterministic")
                self.assertEqual(result.returncode, 0, result.stderr)
                with zipfile.ZipFile(out) as archive:
                    self.assertIsNone(archive.testzip())
                    self.assertIn("megu-mod-bundle.json", archive.namelist())


class WatchTests(TempProject):
    def test_watch_once_installs_an_atomic_dev_package(self):
        project = self.init("watch_mod", "starter")
        mods = self.temp / "server" / "mods"
        result = run("watch", str(project), "--mods-dir", str(mods), "--once")
        self.assertEqual(result.returncode, 0, result.stderr)
        target = mods / "watch_mod-dev.mmod"
        self.assertTrue(target.is_file())
        self.assertFalse((mods / "watch_mod-dev.mmod.part").exists(), "the .part sibling must be renamed away")
        self.assertIn(sha256(target), result.stdout)
        with zipfile.ZipFile(target) as archive:
            self.assertIn("megu-mod-bundle.json", archive.namelist())

    def test_watch_once_reports_json_and_refuses_invalid_projects(self):
        project = self.init("watch_mod", "starter")
        mods = self.temp / "mods"
        good_result, good = run_json("watch", str(project), "--mods-dir", str(mods), "--once", "--json")
        self.assertEqual(good_result.returncode, 0)
        self.assertIs(good["ok"], True)
        self.assertTrue(good["out"].endswith("watch_mod-dev.mmod"))
        self.assertTrue(good["sha256"])

        (project / "megu-mod-bundle.json").write_text("{", encoding="utf-8")
        bad_result, bad = run_json("watch", str(project), "--mods-dir", str(mods), "--once", "--json")
        self.assertEqual(bad_result.returncode, 1)
        self.assertIs(bad["ok"], False)
        self.assertTrue(bad["diagnostics"])
        # The last good package is preserved rather than replaced by nothing.
        self.assertTrue((mods / "watch_mod-dev.mmod").is_file())

    def test_watch_rejects_non_positive_interval(self):
        project = self.init()
        result = run("watch", str(project), "--mods-dir", str(self.temp / "mods"), "--interval", "0")
        self.assertEqual(result.returncode, 2)


class DoctorAndSchemaTests(TempProject):
    def test_doctor_reports_a_ready_environment(self):
        result, payload = run_json("doctor", "--json")
        self.assertEqual(result.returncode, 0, json.dumps(payload, indent=2))
        self.assertIs(payload["ok"], True)
        names = {check["name"] for check in payload["checks"]}
        self.assertLessEqual({"python", "tomllib", "megu", "megu-mod-check", "schema", "example"}, names)
        python = next(check for check in payload["checks"] if check["name"] == "python")
        self.assertEqual(python["status"], "OK")
        schema = next(check for check in payload["checks"] if check["name"] == "schema")
        self.assertEqual(schema["status"], "OK")

    def test_schema_parses_as_draft_2020_12_and_mirrors_the_rust_parser(self):
        schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
        self.assertEqual(schema["$schema"], "https://json-schema.org/draft/2020-12/schema")
        self.assertEqual(schema["type"], "object")
        self.assertIs(schema["additionalProperties"], False)
        self.assertEqual(schema["properties"]["schema"]["const"], "megu-mod-bundle/1")
        self.assertIn("$defs", schema)

        rust = RUST_BUNDLE.read_text(encoding="utf-8")
        match = re.search(r"const TOP_KEYS: &\[&str\] = &\[(.*?)\];", rust, re.DOTALL)
        self.assertIsNotNone(match, "TOP_KEYS moved; update the schema and this test together")
        rust_keys = re.findall(r'"([a-z_]+)"', match.group(1))
        self.assertEqual(set(schema["properties"]), set(rust_keys), "schema keys must match DeclarativeModBundle::from_json")
        self.assertEqual(set(schema["required"]), set(rust_keys) - {"spells", "custom_statuses", "rituals"})

    def test_schema_defines_every_variant_the_runtime_parses(self):
        schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
        defs = schema["$defs"]
        kinds = lambda *names: {
            definition["properties"]["kind"]["const"]
            for name, definition in defs.items()
            if name in names
        }
        self.assertEqual(
            kinds("triggerVerticalFrame", "triggerHorizontalPool", "triggerAltitudeThreshold", "triggerDirectBlockInteract"),
            {"vertical_frame", "horizontal_pool", "altitude_threshold", "direct_block_interact"},
        )
        self.assertEqual(
            kinds(
                "formSelfCast",
                "formTouch",
                "formProjectile",
                "formRaycastBeam",
                "formAoeZone",
                "formChain",
                "formOrbit",
                "formSummon",
            ),
            {"self_cast", "touch", "projectile", "raycast_beam", "aoe_zone", "chain", "orbit", "summon"},
        )
        self.assertEqual(
            kinds(
                "effectDamage",
                "effectHeal",
                "effectShield",
                "effectApplyStatus",
                "effectKnockback",
                "effectBlink",
                "effectConjureLight",
                "effectBreakBlock",
                "effectEmitVfx",
            ),
            {"damage", "heal", "shield", "apply_status", "knockback", "blink", "conjure_light", "break_block", "emit_vfx"},
        )
        self.assertEqual(
            kinds("movementLand", "movementAmphibious", "movementAquatic", "movementGlidingFlight", "movementWallClimb"),
            {"land", "amphibious", "aquatic", "gliding_flight", "wall_climb"},
        )
        self.assertEqual(kinds("castInstant", "castTime", "castChanneled"), {"instant", "cast_time", "channeled"})

    def test_example_bundle_has_no_java_only_assumptions(self):
        bundle = json.loads((EXAMPLE / "megu-mod-bundle.json").read_text(encoding="utf-8"))
        self.assertEqual(set(bundle), {
            "schema",
            "mod_id",
            "mc_version_range",
            "portals",
            "progression_barriers",
            "biome_regions",
            "creatures",
            "custom_channels",
        })
        self.assertFalse((EXAMPLE / "META-INF").exists())
        self.assertFalse((EXAMPLE / "fabric.mod.json").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
