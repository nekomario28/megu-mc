#!/usr/bin/env python3
"""Exercise the compiled mod adapter and client launch boundary in CI.

This gate deliberately uses only Python's standard library. Its synthetic JARs
are generated in a temporary game directory, so it also runs in the public
clean-room checkout where Mojang-derived fixtures are absent.
"""

import argparse
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import zlib
import zipfile


ROOT = Path(__file__).resolve().parents[1]
FORBIDDEN_STDERR = re.compile(
    r"(?i)\b(?:panic(?:ked)?|assertion failed|segmentation fault|stack overflow)\b"
)
FIXTURE_PATHS = (Path("config_packets.bin"), Path("assets/block_states_1211.json"))


class GateError(RuntimeError):
    """A required playability check failed."""


def resolve_path(path, root=ROOT):
    path = Path(path)
    return path if path.is_absolute() else root / path


def require_binary(path, label):
    candidate = Path(path)
    if not candidate.is_file():
        raise GateError(f"missing {label} binary: {candidate}")
    if os.name != "nt" and not os.access(candidate, os.X_OK):
        raise GateError(f"{label} binary is not executable: {candidate}")
    return candidate.resolve()


def has_full_fixtures(root=ROOT):
    return all((root / rel).is_file() for rel in FIXTURE_PATHS)


def run_checked(argv, *, cwd=ROOT, expected=0, label="command", timeout=120):
    result = subprocess.run(
        [str(arg) for arg in argv], cwd=cwd, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, timeout=timeout, check=False,
    )
    forbidden = FORBIDDEN_STDERR.search(result.stderr)
    if forbidden:
        raise GateError(f"{label} wrote forbidden stderr: {forbidden.group(0)}\n{result.stderr}")
    if result.returncode != expected:
        raise GateError(
            f"{label} exited {result.returncode}, expected {expected}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def png_bytes():
    """Return a deterministic, valid 1x1 RGBA PNG for the synthetic addon."""
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">2I5B", 1, 1, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"\x00\x72\x9c\xb0\xff", 9))
        + chunk(b"IEND", b"")
    )


def synthetic_mod_members():
    create_manifest = {
        "schemaVersion": 1,
        "id": "mini_create_addon",
        "version": "1.0.0",
        "name": "Mini Create Addon",
        "depends": {"fabricloader": ">=0.15.0", "minecraft": "1.21.1", "create": "*"},
    }
    food_manifest = {
        "schemaVersion": 1,
        "id": "mini_food",
        "version": "1.0.0",
        "name": "Mini Food",
        "depends": {"fabricloader": ">=0.15.0", "minecraft": "1.21.1"},
    }
    return {
        "mini_create_addon.jar": {
            "fabric.mod.json": json.dumps(create_manifest, sort_keys=True, separators=(",", ":")).encode(),
            "assets/mini_create/blockstates/mini_shaft.json": b'{"variants":{"axis=x":{"model":"mini_create:block/mini_shaft"}}}',
            "assets/mini_create/textures/block/mini_shaft.png": png_bytes(),
            "data/mini_create/recipes/pressing/plate.json": b'{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:iron_ingot"],"result":{"id":"minecraft:iron_nugget","count":9}}',
        },
        "mini_food.jar": {
            "fabric.mod.json": json.dumps(food_manifest, sort_keys=True, separators=(",", ":")).encode(),
            "assets/mini_food/lang/en_us.json": b'{"item.mini_food.apple_pie":"Apple Pie"}',
            "assets/mini_food/lang/ja_jp.json": '{"item.mini_food.apple_pie":"アップルパイ"}'.encode(),
            "data/mini_food/recipes/cooking.json": b'{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:apple"],"result":{"id":"mini_food:apple_pie","count":1}}',
        },
    }


def write_synthetic_mods(mods_dir):
    mods_dir.mkdir(parents=True, exist_ok=True)
    for filename, members in synthetic_mod_members().items():
        with zipfile.ZipFile(mods_dir / filename, "w", compression=zipfile.ZIP_DEFLATED) as jar:
            for name, contents in sorted(members.items()):
                info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o100644 << 16
                jar.writestr(info, contents)


def verify_adaptation_report(stdout, out_dir):
    try:
        report = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise GateError(f"megu-mod-check did not emit valid JSON: {exc}\n{stdout}") from exc
    if report.get("schema") != "megu-mods-dir-adaptation/1":
        raise GateError(f"unexpected mod adaptation report schema: {report.get('schema')!r}")
    if report.get("jars_found") != 2:
        raise GateError(f"expected two synthetic mods, got {report.get('jars_found')!r}")
    native_count = report.get("totals", {}).get("native_bundle", 0)
    if not isinstance(native_count, int) or native_count < 1:
        raise GateError(f"no synthetic mod was adapted into a native bundle: {report.get('totals')!r}")

    jars = report.get("jars")
    if not isinstance(jars, list) or len(jars) != 2:
        raise GateError("adaptation report is missing the two per-JAR results")
    data = [jar.get("data_bundle", {}) for jar in jars]
    blockstates = {item for row in data for item in row.get("blockstates", [])}
    recipes = {item for row in data for item in row.get("recipes", [])}
    if "mini_create:mini_shaft" not in blockstates:
        raise GateError(f"Create-family blockstate was not discovered: {sorted(blockstates)}")
    for recipe in ("mini_create:pressing/plate", "mini_food:cooking"):
        if recipe not in recipes:
            raise GateError(f"synthetic recipe was not discovered: {recipe}; found {sorted(recipes)}")

    bundle_files = sorted(out_dir.glob("*.bundle.json"))
    if len(bundle_files) != native_count:
        raise GateError(f"expected {native_count} emitted bundles, found {len(bundle_files)}")
    if not (out_dir / "adaptation-report.json").is_file():
        raise GateError("megu-mod-check did not write adaptation-report.json")
    return native_count


def check_mod_adapter(binary, temp_root):
    mods = temp_root / "game" / "mods"
    adapted = temp_root / "adapted"
    write_synthetic_mods(mods)
    result = run_checked(
        [binary, "--adapt-mods-dir", mods, "--out", adapted, "--json"],
        label="megu-mod-check synthetic multi-mod adaptation",
    )
    count = verify_adaptation_report(result.stdout, adapted)
    print(f"PASS megu-mod-check adapted {count} synthetic mod(s), discovered blockstate and recipes")


def check_client(binary, temp_root):
    help_result = run_checked([binary, "--help"], label="megu-client --help")
    if "USAGE:" not in help_result.stdout or "--quickPlaySingleplayer" not in help_result.stdout:
        raise GateError("megu-client help is missing launch options")

    # A relative server path must be resolved from the caller's working
    # directory and rejected before creating the requested relative game dir.
    isolated = temp_root / "client-cwd"
    isolated.mkdir()
    relative_game_dir = Path("relative-game-dir")
    missing_server = Path("missing-bin/megu")
    failed = run_checked(
        [binary, "--quickPlaySingleplayer", "world", "--gameDir", relative_game_dir,
         "--server-bin", missing_server],
        cwd=isolated, expected=2, label="megu-client relative server-bin fail-closed check",
    )
    if "does not exist or is not a file" not in failed.stderr:
        raise GateError(f"client did not reject the missing relative server binary: {failed.stderr}")
    if (isolated / relative_game_dir).exists():
        raise GateError("client created a relative game directory after rejecting server-bin")
    print("PASS megu-client CLI help and relative server-bin fail-closed validation")


def check_optional_live_e2e(client_binary, server_binary):
    script = ROOT / "tools/live_playability_e2e.py"
    if not script.is_file():
        print("SKIP live playability E2E: tools/live_playability_e2e.py is absent")
        return
    tests = ROOT / "tests/live_playability_e2e_test.py"
    if not tests.is_file():
        raise GateError("live playability script exists but tests/live_playability_e2e_test.py is missing")
    run_checked([sys.executable, tests], label="live_playability_e2e unit tests")
    print("PASS live_playability_e2e unit tests")

    if has_full_fixtures():
        server_rel = Path(os.path.relpath(server_binary, ROOT))
        client_rel = Path(os.path.relpath(client_binary, ROOT))
        run_checked(
            [sys.executable, script, "--server-bin", server_rel, "--client-bin", client_rel],
            label="live playability end-to-end runtime",
            timeout=600,
        )
        print("PASS live playability end-to-end runtime with local Mojang fixtures")
    else:
        print("SKIP live playability runtime: clean-room mode (Mojang fixtures absent)")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mod-check-bin", default="mod-adapter-core/target/debug/megu-mod-check")
    parser.add_argument("--client-bin", default="client-app/target/debug/megu-client")
    parser.add_argument("--server-bin", default="target/debug/megu")
    args = parser.parse_args(argv)

    try:
        mod_check = require_binary(resolve_path(args.mod_check_bin), "megu-mod-check")
        client = require_binary(resolve_path(args.client_bin), "megu-client")
        server = resolve_path(args.server_bin).resolve()
        if not server.is_file() or (os.name != "nt" and not os.access(server, os.X_OK)):
            raise GateError(f"missing or non-executable megu server binary: {server}")
        mode = "full-fixture" if has_full_fixtures() else "clean-room"
        print(f"Playability gate mode: {mode}")
        with tempfile.TemporaryDirectory(prefix="megu-playability-gate-") as temp:
            temp_root = Path(temp)
            check_mod_adapter(mod_check, temp_root)
            check_client(client, temp_root)
        check_optional_live_e2e(client, server)
    except (GateError, OSError, subprocess.TimeoutExpired) as exc:
        print(f"FAIL playability gate: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
