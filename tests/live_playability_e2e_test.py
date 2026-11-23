#!/usr/bin/env python3
"""Hermetic tests for tools/live_playability_e2e.py.

Two layers, neither needs a compiled Rust binary:

1. Unit tests pin the parsers (CHUNKPROF, joined marker, [live-mods]
   report), the forbidden-stderr gate, the test-datapack name scan, the
   symlink/world-dir inspector, runtime-root resolution, the relative
   --server-bin computation, and the synthetic mod JAR builder.

2. A mock `megu-client` shell-out stands in for the real integrated
   launch: it builds <gameDir>/saves/<world> links from the --server-bin
   ancestor walk (the same layout client-app/src/integrated.rs produces),
   emits server-shaped stderr, and writes world_overlay.bin on exit.
   Running the whole gate against it — and against deliberately broken
   variants — proves every check can go red: missing world_overlay.bin,
   `testpack` in logs, `unsupported fluid shape` on stderr, broken and
   self-referential symlinks, a silent mod-JAR scan, and a join that
   misses the budget.

The live-binary run (`tools/live_playability_e2e.py` against
target/{release,debug}/megu + client-app/target/.../megu-client) happens
outside this file and is recorded in the change report.
"""
import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile

HERE = Path(__file__).resolve()
ROOT = HERE.parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import live_playability_e2e as gate  # noqa: E402


FAKE_CLIENT = """#!/usr/bin/env python3
# Mock megu-client: emulates the integrated --quickPlaySingleplayer launch
# (world-dir links, server-shaped stderr, save on exit). Scenario flags
# arrive as FAKE_* environment variables.
import json
import os
import shutil
import sys
import time
from pathlib import Path


def arg_value(flag, default=None):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


def runtime_root(server_bin, cwd):
    server_bin = Path(server_bin)
    resolved = server_bin if server_bin.is_absolute() else cwd / server_bin
    resolved = resolved.resolve()
    for base in list(resolved.parents) + [cwd, *cwd.parents]:
        if ((base / "config_packets.bin").is_file()
                and (base / "datapacks").is_dir()
                and (base / "assets" / "block_states_1211.json").is_file()):
            return base
    return None


game_dir = Path(arg_value("--gameDir"))
world = arg_value("--quickPlaySingleplayer")
server_bin = arg_value("--server-bin")
username = arg_value("--username", "E2EGate")
world_dir = game_dir / "saves" / world
root = runtime_root(server_bin, Path.cwd())

if os.environ.get("FAKE_SPAWN_FAIL"):
    sys.stderr.write("megu-client: failed to spawn megu server\\n")
    sys.exit(2)

world_dir.mkdir(parents=True, exist_ok=True)
if root is not None:
    def relink(name, target):
        dst = world_dir / name
        if dst.is_symlink() or dst.exists():
            if dst.is_symlink():
                dst.unlink()
            elif dst.is_dir():
                shutil.rmtree(dst)
            else:
                dst.unlink()
        dst.symlink_to(target)

    if os.environ.get("FAKE_LOOP_LINK"):
        (world_dir / "config_packets.bin").symlink_to(
            "config_packets.bin")
        relink("assets", root / "assets")
    else:
        relink("config_packets.bin", root / "config_packets.bin")
        if os.environ.get("FAKE_BROKEN_LINK"):
            relink("assets", root / "assets-missing")
        else:
            relink("assets", root / "assets")
    dp = world_dir / "datapacks"
    if os.environ.get("FAKE_WHOLE_DATAPACKS"):
        if dp.exists() and not dp.is_symlink():
            shutil.rmtree(dp)
        if not dp.exists():
            dp.symlink_to(root / "datapacks")
    else:
        dp.mkdir(exist_ok=True)
        vanilla = dp / "vanilla"
        if not vanilla.exists():
            vanilla.symlink_to(root / "datapacks" / "vanilla")

sys.stderr.write("[wg] vanilla datapack worldgen active (seed 12026)\\n")
sys.stderr.flush()

extra = os.environ.get("FAKE_EXTRA_STDERR")
if extra:
    sys.stderr.write(Path(extra).read_text())
    if not Path(extra).read_text().endswith("\\n"):
        sys.stderr.write("\\n")
    sys.stderr.flush()

if os.environ.get("FAKE_LIVEMODS"):
    sys.stderr.write(os.environ["FAKE_LIVEMODS"])
    if not os.environ["FAKE_LIVEMODS"].endswith("\\n"):
        sys.stderr.write("\\n")
    sys.stderr.flush()

delay = float(os.environ.get("FAKE_JOIN_DELAY", "0.05"))
time.sleep(delay)
if not os.environ.get("FAKE_NO_JOIN"):
    sys.stderr.write(
        f"[spine] {username} joined from 127.0.0.1:60000 (uuid 0123abcd)\\n")
    sys.stderr.flush()

if not os.environ.get("FAKE_NO_CHUNKPROF"):
    sys.stderr.write(
        "CHUNKPROF n=60 map=1.0ms data=1.0 section=1.0 light=1.0 "
        "(sky=0.1 block=0.1 floors=0.2 x1.0 block_at=10 miss=0)\\n")
    sys.stderr.write(
        "CHUNKPROF   band lookups=2.0 new=1.0 build=3.50ms cells=256.0 "
        "(per new=3.50ms)\\n")
    sys.stderr.write(
        "CHUNKPROF   band parts: range=0.10ms sections=0.20ms "
        "cols_miss=0.0 epoch_bumps=0.00/chunk\\n")
    sys.stderr.write(
        "CHUNKPROF   floors: calls=1.00/chunk cells=128/chunk "
        "starts=0.30ms/chunk\\n")
    sys.stderr.flush()
sys.stderr.write("[sz] t=100  | mobs=3 drops=0 xps=0 arrows=0 bob=0 "
                 "pearl=0 primed=0 chests=0 furn=0 crops=0 world=200 "
                 "inv=0 clients=1\\n")
sys.stderr.flush()

time.sleep(float(os.environ.get("FAKE_TAIL_DELAY", "0.05")))

overlay = world_dir / "world_overlay.bin"
if os.environ.get("FAKE_EMPTY_OVERLAY"):
    overlay.write_bytes(b"")
elif not os.environ.get("FAKE_NO_OVERLAY"):
    overlay.write_bytes(b"SED1" + b"\\x00" * 64)

if (os.environ.get("MEGU_ENABLE_CREATE") == "1"
        and not os.environ.get("FAKE_NO_CREATE_SIDECAR")):
    (world_dir / "create_factory.bin").write_bytes(b"MCF1\\x00\\x00\\x00\\x00\\x00")

print(json.dumps({"status": "ok", "play_ok": True, "chunks_loaded": 81}))
sys.exit(int(os.environ.get("FAKE_EXIT_CODE", "0")))
"""


def make_fake_root(base):
    """A synthetic megu runtime root: binary, config, datapacks, assets."""
    root = Path(base)
    (root / "target" / "release").mkdir(parents=True)
    (root / "config_packets.bin").write_bytes(b"CFG1")
    (root / "assets").mkdir()
    (root / "assets" / "block_states_1211.json").write_text("{}")
    (root / "datapacks" / "vanilla").mkdir(parents=True)
    (root / "datapacks" / "vanilla" / "pack.mcmeta").write_text("{}")
    for name in ("testpack", "modtest"):
        (root / "datapacks" / name).mkdir()
        (root / "datapacks" / name / "pack.mcmeta").write_text("{}")
    server = root / "target" / "release" / "megu"
    server.write_text("#!/bin/sh\nexit 0\n")
    server.chmod(0o755)
    client = root / "megu-client"
    client.write_text(FAKE_CLIENT)
    client.chmod(0o755)
    return root, server, client


def run_gate(*argv, env=None):
    """Invoke gate.main in-process, capturing stdout."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = gate.main(list(argv))
    return code, out.getvalue()


class UnitParsers(unittest.TestCase):
    def test_joined_marker(self):
        m = gate.JOINED_RE.search(
            "[spine] E2EGate joined from 127.0.0.1:45512 (uuid 00ff)")
        self.assertEqual(m.group("user"), "E2EGate")
        self.assertIsNone(gate.JOINED_RE.search("[spine] listening on 25565"))

    def test_chunkprof_metrics(self):
        metrics = gate.parse_chunkprof([
            "CHUNKPROF n=90 map=1.0ms data=1.0 section=1.0 light=1.0 "
            "(sky=0.1 block=0.1 floors=0.2 x1.0 block_at=10 miss=0)",
            "CHUNKPROF   band lookups=2.0 new=1.5 build=4.00ms cells=256.0 "
            "(per new=2.67ms)",
            "CHUNKPROF   band parts: range=0.10ms sections=0.20ms "
            "cols_miss=0.0 epoch_bumps=0.50/chunk",
            "CHUNKPROF   floors: calls=1.00/chunk cells=128/chunk "
            "starts=0.30ms/chunk",
            "[sz] t=5  | mobs=7 drops=0 xps=0 arrows=0 bob=0 pearl=0 "
            "primed=0 chests=0 furn=0 crops=0 world=10 inv=0 clients=1",
        ])
        self.assertEqual(metrics["chunks_delivered"], 90)
        self.assertEqual(metrics["band_lookups_per_chunk"], 2.0)
        self.assertEqual(metrics["band_ms_per_new"], 2.67)
        self.assertEqual(metrics["epoch_bumps_per_chunk"], 0.5)
        self.assertEqual(metrics["floors_cells_per_chunk"], 128.0)
        self.assertEqual(metrics["mobs_last"], 7)
        self.assertEqual(metrics["chunkprof_reports"], 4)

    def test_chunkprof_band_zero_detected(self):
        metrics = gate.parse_chunkprof([
            "CHUNKPROF   band lookups=0.0 new=0.0 build=0.00ms cells=0.0 "
            "(per new=0.00ms)",
        ])
        self.assertEqual(metrics["band_lookups_per_chunk"], 0.0)

    def test_forbidden_gate_hits_every_needle(self):
        for needle in gate.FORBIDDEN_STDERR:
            hits = gate.find_forbidden([f"prefix {needle} suffix"])
            self.assertEqual(len(hits), 1, needle)
            self.assertEqual(hits[0][1], needle)

    def test_forbidden_gate_clean(self):
        self.assertEqual(gate.find_forbidden(
            ["[spine] E2EGate joined from 127.0.0.1:1 (uuid 0)",
             "[live-mods] 1 jar(s): 0 auto-skipped, 1 native-bundle, "
             "0 tier2-shim, 0 tier3"]), [])

    def test_test_datapack_names(self):
        hits = gate.find_test_datapack_mentions(
            ["[spine] loading datapack testpack from datapacks/testpack",
             "[spine] datapacks loaded fine",
             "modtest data dir scanned"])
        self.assertEqual([h[1] for h in hits], ["testpack", "modtest"])
        # Word boundaries: 'testpacks', 'xmodtest' must not match.
        self.assertEqual(gate.find_test_datapack_mentions(
            ["testpacks dir", "xmodtest-y"]), [])
        self.assertEqual(gate.find_test_datapack_mentions(
            ["[live-mods] mods dir scanned: 1 jar(s), none classified"]), [])

    def test_live_mods_report(self):
        report = gate.parse_live_mods_report([
            "[live-mods] dual-loader: 1 unified, 0 client-only, 0 superseded, "
            "0 rejected, 0 duplicate ids, 0 missing deps, 0 verified hooks",
            "[live-mods] 1 jar(s): 0 auto-skipped, 1 native-bundle, "
            "0 tier2-shim, 0 tier3",
        ])
        self.assertEqual(report["jars"], 1)
        self.assertEqual(report["native_bundle"], 1)
        self.assertEqual(report["tier3"], 0)
        scanned = gate.parse_live_mods_report(
            ["[live-mods] mods dir scanned: 0 jar(s), none classified"])
        self.assertTrue(scanned["none_classified"])
        self.assertIsNone(gate.parse_live_mods_report(["[wg] flat"]))


class UnitFilesystem(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="e2e-fs-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)

    def make_world(self, name="E2EWorld"):
        world = self.base / "saves" / name
        world.mkdir(parents=True)
        (world / "assets").mkdir()
        (world / "config_packets.bin").write_bytes(b"x")
        (world / "datapacks" / "vanilla").mkdir(parents=True)
        return world

    def test_inspect_clean_world(self):
        world = self.make_world()
        problems, info = gate.inspect_world_dir(world)
        self.assertEqual(problems, [])
        self.assertEqual(info["datapacks_entries"], ["vanilla"])

    def test_inspect_broken_symlink(self):
        world = self.make_world()
        broken = world / "assets-link"
        broken.symlink_to(world / "nonexistent-target")
        problems, _ = gate.inspect_world_dir(world)
        self.assertTrue(any("broken symlink" in p for p in problems), problems)

    def test_inspect_symlink_loop(self):
        world = self.make_world()
        loop = world / "self"
        loop.symlink_to("self")
        problems, _ = gate.inspect_world_dir(world)
        self.assertTrue(any("loop" in p for p in problems), problems)

    def test_inspect_test_datapack_leak(self):
        world = self.make_world()
        (world / "datapacks" / "testpack").mkdir()
        problems, _ = gate.inspect_world_dir(world)
        self.assertTrue(any("testpack" in p for p in problems), problems)

    def test_inspect_whole_dir_link_leak(self):
        world = self.make_world()
        import shutil
        shutil.rmtree(world / "datapacks")
        runtime = self.base / "runtime"
        (runtime / "datapacks" / "vanilla").mkdir(parents=True)
        (runtime / "datapacks" / "modtest").mkdir(parents=True)
        (world / "datapacks").symlink_to(runtime / "datapacks")
        problems, info = gate.inspect_world_dir(world)
        self.assertTrue(info["datapacks_is_link"])
        self.assertTrue(any("modtest" in p for p in problems), problems)

    def test_inspect_missing_world_dir(self):
        problems, info = gate.inspect_world_dir(self.base / "saves" / "none")
        self.assertFalse(info["exists"])
        self.assertTrue(any("missing" in p for p in problems))

    def test_runtime_root_via_binary_ancestors(self):
        root, server, _ = make_fake_root(self.base / "root")
        found = gate.runtime_root_for(server, Path("/nonexistent-cwd"))
        self.assertEqual(found, root)

    def test_runtime_root_via_cwd_ancestors(self):
        root, server, _ = make_fake_root(self.base / "root")
        server2 = self.base / "elsewhere" / "megu"
        server2.parent.mkdir(parents=True)
        server2.write_text("x")
        found = gate.runtime_root_for(server2, root / "sub" / "dir")
        self.assertEqual(found, root)

    def test_runtime_root_missing(self):
        bare = self.base / "bare"
        (bare / "target").mkdir(parents=True)
        self.assertIsNone(gate.runtime_root_for(bare / "target" / "megu",
                                                bare))

    def test_relative_server_arg(self):
        root, server, _ = make_fake_root(self.base / "root")
        rel = gate.relative_server_arg(server, root)
        self.assertEqual(rel, "." + os.sep + os.path.join(
            "target", "release", "megu"))
        outside = self.base / "other" / "megu"
        outside.parent.mkdir(parents=True)
        outside.write_text("x")
        self.assertEqual(
            gate.relative_server_arg(outside, root), str(outside))

    def test_resolve_arg_path(self):
        self.assertEqual(
            gate.resolve_arg_path("./a/b", "/base"), Path("/base/a/b"))
        self.assertEqual(
            gate.resolve_arg_path("/x/y", "/base"), Path("/x/y"))


class UnitSyntheticJar(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="e2e-jar-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)

    def test_jar_members_and_manifest(self):
        jar = gate.build_synthetic_mod_jar(self.base / "e2e_test_mod.jar")
        self.assertTrue(zipfile.is_zipfile(jar))
        with zipfile.ZipFile(jar) as archive:
            names = set(archive.namelist())
            for wanted in (
                    "META-INF/neoforge.mods.toml",
                    "pack.mcmeta",
                    "assets/e2emod/blockstates/e2e_block.json",
                    "assets/e2emod/models/block/e2e_block.json",
                    "assets/e2emod/textures/block/e2e_block.png",
                    "assets/e2emod/lang/en_us.json",
                    "data/e2emod/recipes/e2e_shard.json"):
                self.assertIn(wanted, names)
            import tomllib
            toml = tomllib.loads(
                archive.read("META-INF/neoforge.mods.toml").decode())
            self.assertEqual(toml["mods"][0]["modId"], "e2emod")
            deps = toml["dependencies"]["e2emod"]
            self.assertEqual(deps[0]["modId"], "minecraft")
            png = archive.read("assets/e2emod/textures/block/e2e_block.png")
            self.assertTrue(png.startswith(b"\x89PNG\r\n\x1a\n"))
            self.assertIn(b"IEND", png)
            recipe = json.loads(
                archive.read("data/e2emod/recipes/e2e_shard.json"))
            self.assertEqual(recipe["result"]["id"], "e2emod:e2e_shard")

    def test_png_decodes(self):
        import zlib as z
        data = gate.png_1x1()
        # IHDR is the first chunk: length, tag, 13 payload bytes, CRC.
        self.assertEqual(data[12:16], b"IHDR")
        idat_at = data.find(b"IDAT")
        self.assertGreater(idat_at, 0)
        length = int.from_bytes(data[idat_at - 4:idat_at], "big")
        raw = z.decompress(data[idat_at + 4:idat_at + 4 + length])
        self.assertEqual(len(raw), 5)  # filter byte + 1 RGBA pixel

    def test_create_jar_has_required_neoforge_and_create_resources(self):
        jar = gate.build_synthetic_create_jar(self.base / "create_e2e.jar")
        self.assertTrue(zipfile.is_zipfile(jar))
        with zipfile.ZipFile(jar) as archive:
            names = set(archive.namelist())
            for wanted in (
                    "META-INF/neoforge.mods.toml",
                    "assets/create/blockstates/shaft.json",
                    "assets/create/textures/block/shaft.png",
                    "assets/create/lang/ja_jp.json"):
                self.assertIn(wanted, names)
            import tomllib
            manifest = tomllib.loads(
                archive.read("META-INF/neoforge.mods.toml").decode())
            self.assertEqual(manifest["mods"][0]["modId"], "create_e2e")
            blockstate = json.loads(
                archive.read("assets/create/blockstates/shaft.json"))
            self.assertIn("axis=x", blockstate["variants"])
            self.assertTrue(archive.read(
                "assets/create/textures/block/shaft.png").startswith(
                    b"\x89PNG\r\n\x1a\n"))
            lang = json.loads(archive.read("assets/create/lang/ja_jp.json"))
            self.assertEqual(lang["block.create.shaft"], "シャフト")

    def test_gpu_probe_receives_modded_game_dir(self):
        gpu = self.base / "fake-gpu"
        gpu.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            "assert sys.argv[1:] == ['--headless-probe', '--gameDir', "
            + repr(str(self.base / "game")) + "]\n")
        gpu.chmod(0o755)
        result = gate.run_gpu_probe(
            gpu, self.base, timeout=5,
            game_dir=self.base / "game")
        self.assertEqual(result.exit_code, 0, result.stderr_lines)
        self.assertEqual(result.cmd[-2:], ["--gameDir", str(self.base / "game")])


class MockHarness(unittest.TestCase):
    """Drive the full gate against the fake client; prove checks go red."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="e2e-harness-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root, self.server, self.client = make_fake_root(
            self.base / "root")
        self.env_patcher = None
        self._saved = dict(os.environ)

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self._saved)

    def set_fake(self, **flags):
        for key in list(os.environ):
            if key.startswith("FAKE_"):
                del os.environ[key]
        os.environ.update(flags)

    def argv(self, *extra, mode="vanilla"):
        return [
            "--server-bin", "./target/release/megu",
            "--client-bin", "./megu-client",
            "--run-cwd", str(self.root),
            "--mode", mode,
            "--ticks", "5",
            "--join-timeout", "10",
            "--exit-timeout", "30",
            "--save-timeout", "2",
            *extra,
        ]

    def test_happy_path_vanilla(self):
        self.set_fake()
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 0, out)
        self.assertIn("PASS: ", out)
        self.assertIn("C1", out)
        self.assertIn("C4", out)

    def test_happy_path_modded(self):
        self.set_fake(FAKE_LIVEMODS=(
            "[live-mods] auto-adapt: synthesized megu-mod-bundle/1 for mod "
            "id 'e2emod' (mc_version_range '[1.21,1.22)')\n"
            "[live-mods] native bundle create_e2e (create_e2e.jar): 0 portals, "
            "0 progression barriers, 0 regions, 0 creatures, 0 channels\n"
            "[live-mods] 1 jar(s): 0 auto-skipped, 1 native-bundle, "
            "0 tier2-shim, 0 tier3\n"))
        code, out = run_gate(*self.argv(mode="modded"))
        self.assertEqual(code, 0, out)
        self.assertIn("C5", out)
        self.assertIn("C6", out)
        # The synthetic JAR really landed in the temp mods dir.
        self.assertIn("e2e_test_mod.jar", out)
        self.assertIn("create_e2e.jar", out)

    def test_create_runtime_without_sidecar_fails(self):
        self.set_fake(
            FAKE_NO_CREATE_SIDECAR="1",
            FAKE_LIVEMODS=(
                "[live-mods] native bundle create_e2e (create_e2e.jar): "
                "0 portals, 0 progression barriers, 0 regions, 0 creatures, "
                "0 channels\n[live-mods] 2 jar(s): 0 auto-skipped, "
                "2 native-bundle, 0 tier2-shim, 0 tier3\n"))
        code, out = run_gate(*self.argv(mode="modded"))
        self.assertEqual(code, 1, out)
        self.assertIn("Create was not observably armed", out)

    def test_create_stderr_error_fails(self):
        self.set_fake(
            FAKE_EXTRA_STDERR=str(self.base / "stderr-errors.txt"),
            FAKE_LIVEMODS=(
                "[live-mods] native bundle create_e2e (create_e2e.jar): "
                "0 portals, 0 progression barriers, 0 regions, 0 creatures, "
                "0 channels\n[live-mods] 2 jar(s): 0 auto-skipped, "
                "2 native-bundle, 0 tier2-shim, 0 tier3\n"))
        (self.base / "stderr-errors.txt").write_text("[ERROR] synthetic startup failure\n")
        code, out = run_gate(*self.argv(mode="modded"))
        self.assertEqual(code, 1, out)
        self.assertIn("stderr error(s)", out)

    def test_mod_jar_silently_ignored_fails(self):
        self.set_fake()  # no FAKE_LIVEMODS lines at all
        code, out = run_gate(*self.argv(mode="modded"))
        self.assertEqual(code, 1, out)
        self.assertIn("silently ignored", out)

    def test_mod_jar_not_a_directory_fails(self):
        self.set_fake(FAKE_LIVEMODS=(
            "[live-mods] MEGU_MODS_DIR /tmp/x is not a directory — mod "
            "scan skipped"))
        code, out = run_gate(*self.argv(mode="modded"))
        self.assertEqual(code, 1, out)

    def test_missing_overlay_fails(self):
        self.set_fake(FAKE_NO_OVERLAY="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)
        self.assertIn("world_overlay.bin", out)

    def test_empty_overlay_fails(self):
        self.set_fake(FAKE_EMPTY_OVERLAY="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_testpack_in_logs_fails(self):
        log = self.base / "extra.log"
        log.write_text("[spine] loaded datapack testpack "
                       "(datapacks/testpack)\n")
        self.set_fake(FAKE_EXTRA_STDERR=str(log))
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)
        self.assertIn("testpack", out)

    def test_unsupported_fluid_shape_fails(self):
        log = self.base / "extra.log"
        log.write_text("unsupported fluid shape minecraft:glow_lichen at "
                       "BlockPos{x=1, y=2, z=3}\n")
        self.set_fake(FAKE_EXTRA_STDERR=str(log))
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)
        self.assertIn("unsupported fluid shape", out)

    def test_pwcat_death_fails(self):
        log = self.base / "extra.log"
        log.write_text("pw-cat exited while streaming (exit status: 1)\n")
        self.set_fake(FAKE_EXTRA_STDERR=str(log))
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_panic_fails(self):
        log = self.base / "extra.log"
        log.write_text("thread 'main' panicked at src/main.rs:1: oh no\n")
        self.set_fake(FAKE_EXTRA_STDERR=str(log))
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_no_such_file_fails(self):
        log = self.base / "extra.log"
        log.write_text("cannot open assets: No such file or directory "
                       "(os error 2)\n")
        self.set_fake(FAKE_EXTRA_STDERR=str(log))
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_symlink_loop_message_fails(self):
        log = self.base / "extra.log"
        log.write_text("cannot stat config_packets.bin: Too many levels of "
                       "symbolic links (os error 40)\n")
        self.set_fake(FAKE_EXTRA_STDERR=str(log))
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_broken_symlink_fails(self):
        self.set_fake(FAKE_BROKEN_LINK="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)
        self.assertIn("broken symlink", out)

    def test_self_referential_symlink_fails(self):
        self.set_fake(FAKE_LOOP_LINK="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_whole_datapacks_link_fails(self):
        self.set_fake(FAKE_WHOLE_DATAPACKS="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)
        self.assertIn("testpack", out)

    def test_no_join_fails(self):
        self.set_fake(FAKE_NO_JOIN="1", FAKE_EXIT_CODE="2")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)
        self.assertIn("join", out.lower())

    def test_slow_join_fails_budget(self):
        self.set_fake(FAKE_JOIN_DELAY="1.2")
        code, out = run_gate(*self.argv("--max-join-secs", "0.3"))
        self.assertEqual(code, 1, out)
        self.assertIn("budget", out)

    def test_client_nonzero_exit_fails(self):
        self.set_fake(FAKE_EXIT_CODE="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_spawn_failure_fails_closed(self):
        self.set_fake(FAKE_SPAWN_FAIL="1")
        code, out = run_gate(*self.argv())
        self.assertEqual(code, 1, out)

    def test_json_report_shape(self):
        self.set_fake()
        code, out = run_gate(*self.argv("--json"))
        self.assertEqual(code, 0, out)
        report = json.loads(out)
        self.assertEqual(report["overall"], "pass")
        ids = [c["id"] for c in report["checks"]]
        for wanted in ("C1", "C2", "C3", "C4"):
            self.assertIn(wanted, ids)
        sessions = report["sessions"]
        self.assertEqual(len(sessions), 2)
        self.assertTrue(sessions[0]["join_seen"])
        self.assertIsNotNone(sessions[0]["join_secs"])
        self.assertTrue(sessions[0]["overlay_exists"])
        # The relative --server-bin form reached the child verbatim.
        self.assertIn("--server-bin", sessions[0]["cmd"])
        arg = sessions[0]["cmd"][
            sessions[0]["cmd"].index("--server-bin") + 1]
        self.assertFalse(Path(arg).is_absolute(), arg)
        # The restart leg used the absolute form.
        arg2 = sessions[1]["cmd"][
            sessions[1]["cmd"].index("--server-bin") + 1]
        self.assertTrue(Path(arg2).is_absolute(), arg2)
        self.assertEqual(
            sessions[0]["chunkprof"]["band_lookups_per_chunk"], 2.0)
        self.assertEqual(sessions[0]["chunkprof"]["mobs_last"], 3)

    def test_missing_server_bin_exits_2(self):
        code, _ = run_gate("--server-bin", "./target/release/nope",
                           "--client-bin", "./megu-client",
                           "--run-cwd", str(self.root))
        self.assertEqual(code, 2)

    def test_world_name_and_username_flags(self):
        self.set_fake()
        code, out = run_gate(*self.argv("--world", "OtherWorld",
                                        "--username", "Tester"))
        self.assertEqual(code, 0, out)


class RunSessionDirect(unittest.TestCase):
    """Session-runner-level assertions without the full gate."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="e2e-sess-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root, self.server, self.client = make_fake_root(
            self.base / "root")
        self._saved = dict(os.environ)
        for key in list(os.environ):
            if key.startswith("FAKE_"):
                del os.environ[key]

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self._saved)

    def test_session_fields(self):
        result = gate.run_session(
            label="t", mode="vanilla",
            client_bin=self.client, server_bin_arg="./target/release/megu",
            run_cwd=self.root, game_dir=self.base / "game",
            world="E2EWorld", username="E2EGate", ticks=5,
            view_distance=4, seed=12026, join_timeout=10.0,
            exit_timeout=30.0, save_timeout=5.0)
        self.assertTrue(result.join_seen, result.stderr_lines)
        self.assertIsNotNone(result.join_secs)
        self.assertEqual(result.exit_code, 0)
        self.assertTrue(result.overlay_exists)
        self.assertGreater(result.overlay_bytes, 0)
        self.assertEqual(result.client_summary["status"], "ok")
        self.assertEqual(result.chunkprof["band_lookups_per_chunk"], 2.0)
        rel = result.cmd[result.cmd.index("--server-bin") + 1]
        self.assertEqual(rel, "./target/release/megu")

    def test_create_factory_header_versions(self):
        world = self.base / "world"
        world.mkdir()
        for header in (b"MCF1", b"MCF2"):
            with self.subTest(header=header):
                (world / "create_factory.bin").write_bytes(header + b"\x00" * 5)
                self.assertTrue(gate.valid_create_factory(
                    (world / "create_factory.bin").read_bytes()))
        for data in (b"MCF0\x00" * 2, b"XXXX\x00\x00\x00\x00\x00", b"MCF2\x00"):
            with self.subTest(data=data):
                (world / "create_factory.bin").write_bytes(data)
                self.assertFalse(gate.valid_create_factory(
                    (world / "create_factory.bin").read_bytes()))

    def test_session_join_timeout(self):
        result = gate.run_session(
            label="t", mode="vanilla",
            client_bin=self.client, server_bin_arg="./target/release/megu",
            run_cwd=self.root, game_dir=self.base / "game",
            world="E2EWorld", username="E2EGate", ticks=5,
            view_distance=4, seed=12026, join_timeout=0.4,
            exit_timeout=30.0, save_timeout=5.0,
            env_extra={"FAKE_JOIN_DELAY": "1.5"})
        # The watch window expired while the client was still running...
        self.assertTrue(result.join_timed_out)
        # ...and the late marker is still recorded with its real stamp, so
        # the budget check fails on join_secs rather than "never joined".
        self.assertTrue(result.join_seen)
        self.assertGreater(result.join_secs, 0.4)


class LiveBinaries(unittest.TestCase):
    """Optional: run the gate for real when compiled binaries + generated
    runtime data exist under this checkout. Skipped on bare worktrees."""

    def test_real_gate_when_binaries_present(self):
        server = gate.find_default_server_bin()
        client = gate.find_default_client_bin()
        runtime = gate.runtime_root_for(server, ROOT) if server else None
        if not (server and client and runtime):
            self.skipTest(
                "no compiled megu/megu-client + generated runtime data "
                "in this checkout")
        code, out = run_gate(
            "--server-bin", str(server),
            "--client-bin", str(client),
            "--mode", "vanilla")
        self.assertEqual(code, 0, out)


if __name__ == "__main__":
    unittest.main()
