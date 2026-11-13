#!/usr/bin/env python3
"""Build/resolve mc-spine, dump offline port chunks, and run causal chunk_triage in one command.

  tools/port_triage.py <world> <cx> <cz> [--n 1] [options]

Why this exists: running a parity check previously took manual `cargo build
--message-format=json-render-diagnostics`, `cargo_executable.py`, sanitized
`MC_SPINE_WG_*` environment variables, reading `level.dat` for the native seed
and stage flags (`file/nofeat`), and invoking `chunk_triage.py` per chunk.

This tool automates the full loop fail-closed:
  1. Reads `<world>/level.dat` (unless `--seed` overrides) and rejects
     `file/nofeat` / `generate_features=0` mismatches against `--mode full`.
  2. Verifies native target chunk(s) are present and `minecraft:full` BEFORE
     building or running the port.
  3. Resolves the exact executable from Cargo JSON output (or `--binary`),
     records its SHA-256, runs an isolated offline dump in clean env, and
     immediately runs single-chunk or NxN `chunk_triage`.

Exit codes:
  0  triage completed (or 0 diffs when `--fail-on-diff` is set)
  1  diffs > 0 with `--fail-on-diff`
  2  invalid input, stage/seed mismatch, build/dump failure, or incomplete chunk
"""
import argparse
import gzip
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import cargo_executable  # noqa: E402
import chunk_triage  # noqa: E402
from vanilla_region_blocks import parse_nbt  # noqa: E402

STRIP_ENV_PREFIXES = ("MC_SPINE_", "SPC_", "FSEED_", "BIOMEF_", "FEATURE_")


class PortTriageError(Exception):
    """Fail-closed error during preflight, build, dump, or triage."""


def read_level_meta(world):
    level_dat = Path(world) / "level.dat"
    if not level_dat.is_file():
        return None
    try:
        raw = gzip.decompress(level_dat.read_bytes())
        root, _ = parse_nbt(raw)
    except Exception as exc:
        raise PortTriageError(f"failed to parse {level_dat}: {exc}") from exc
    if not isinstance(root, dict):
        raise PortTriageError(f"invalid NBT root in {level_dat}")
    data = root.get("Data") or {}
    wgs = data.get("WorldGenSettings") or {}
    seed = wgs.get("seed")
    if seed is None:
        seed = data.get("RandomSeed")
    gen_features = wgs.get("generate_features")
    dp = data.get("DataPacks") or {}
    enabled_packs = list(dp.get("Enabled") or [])
    return {
        "path": str(level_dat),
        "seed": int(seed) if seed is not None else None,
        "generate_features": int(gen_features) if gen_features is not None else None,
        "enabled_packs": enabled_packs,
    }


def resolve_binary(args):
    if args.binary:
        binary = Path(args.binary).resolve()
        if not binary.is_file():
            raise PortTriageError(f"--binary does not exist: {binary}")
        return binary, 0.0

    cmd = [
        "cargo", "build", "--offline", "--locked",
        "-j", str(args.jobs),
        "--message-format=json-render-diagnostics",
    ]
    if args.release:
        cmd.append("--release")
    t0 = time.monotonic()
    proc = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    wall = time.monotonic() - t0
    if proc.returncode != 0:
        tail = "\n".join((proc.stdout + proc.stderr).splitlines()[-25:])
        raise PortTriageError(f"cargo build failed (exit {proc.returncode}):\n{tail}")
    try:
        binary = cargo_executable.executable(proc.stdout.splitlines())
    except Exception as exc:
        raise PortTriageError(f"failed to resolve cargo executable: {exc}") from exc
    return binary, wall


def preflight(args):
    if args.n <= 0:
        raise PortTriageError(f"--n must be positive, got {args.n}")
    if args.jobs <= 0:
        raise PortTriageError(f"--jobs must be positive, got {args.jobs}")
    if args.preset != "overworld" and args.mode == "full":
        raise PortTriageError(f"--preset {args.preset} requires --mode terrain or --mode carvers (features unsupported)")
    if args.pop_order and not Path(args.pop_order).is_file():
        raise PortTriageError(f"missing --pop-order file: {args.pop_order}")

    world = Path(args.world)
    if not world.is_dir():
        raise PortTriageError(f"native world directory does not exist: {world}")

    meta_a = read_level_meta(world)
    if args.seed is not None:
        seed = args.seed
        if meta_a and meta_a["seed"] is not None and meta_a["seed"] != seed and not args.allow_seed_mismatch:
            raise PortTriageError(
                f"--seed {seed} != {meta_a['path']} seed {meta_a['seed']} (pass --allow-seed-mismatch to override)"
            )
    else:
        if not meta_a or meta_a["seed"] is None:
            raise PortTriageError(f"cannot auto-detect seed from {world}/level.dat; pass --seed explicitly")
        seed = meta_a["seed"]

    if args.world_b:
        world_b = Path(args.world_b)
        if not world_b.is_dir():
            raise PortTriageError(f"--world-b directory does not exist: {world_b}")
        meta_b = read_level_meta(world_b)
        if meta_b and meta_b["seed"] is not None and meta_b["seed"] != seed and not args.allow_seed_mismatch:
            raise PortTriageError(
                f"world B seed {meta_b['seed']} != target seed {seed}"
            )

    if meta_a and args.mode == "full" and not args.allow_stage_mismatch:
        packs = meta_a["enabled_packs"]
        if any("nofeat" in p for p in packs):
            raise PortTriageError(
                f"{meta_a['path']} has nofeat pack enabled ({packs}); refusing --mode full "
                "(use --mode terrain/--mode carvers or --allow-stage-mismatch)"
            )
        if meta_a["generate_features"] == 0:
            raise PortTriageError(
                f"{meta_a['path']} has generate_features=0; refusing --mode full "
                "(use --mode terrain/--mode carvers or --allow-stage-mismatch)"
            )

    # Validate native chunk availability/status BEFORE spending time building or running the port.
    nbt_cache = {}
    coords = [(args.cx + dx, args.cz + dz) for dz in range(args.n) for dx in range(args.n)]
    ok_chunks = 0
    first_err = None
    for cx, cz in coords:
        try:
            nbt = chunk_triage.cached_read_chunk_nbt(str(world), cx, cz, nbt_cache)
            st = nbt.get("Status")
            if st not in ("minecraft:full", "full"):
                raise chunk_triage.TriageError(f"{world} chunk ({cx},{cz}) Status={st!r}, expected 'minecraft:full'")
            if args.world_b:
                nbt_b = chunk_triage.cached_read_chunk_nbt(str(args.world_b), cx, cz, nbt_cache)
                st_b = nbt_b.get("Status")
                if st_b not in ("minecraft:full", "full"):
                    raise chunk_triage.TriageError(f"{args.world_b} chunk ({cx},{cz}) Status={st_b!r}, expected 'minecraft:full'")
            ok_chunks += 1
        except chunk_triage.TriageError as exc:
            if not args.allow_incomplete:
                raise PortTriageError(str(exc)) from exc
            if first_err is None:
                first_err = str(exc)
    if ok_chunks == 0:
        raise PortTriageError(first_err or f"0 complete native chunks in {args.n}x{args.n} grid at ({args.cx},{args.cz})")

    return seed, meta_a


def build_dump_env(args, seed, dump_bin, dump_hm, dump_canon):
    env = {k: v for k, v in os.environ.items() if not k.startswith(STRIP_ENV_PREFIXES)}
    env.update({
        "MC_SPINE_SEED": str(seed),
        "MC_SPINE_WG_PROFILE": "0",
        "MC_SPINE_WG_PRESET": args.preset,
        "MC_SPINE_WG_DUMP": str(dump_bin),
        "MC_SPINE_WG_DUMP_X": str(args.cx),
        "MC_SPINE_WG_DUMP_Z": str(args.cz),
        "MC_SPINE_WG_DUMP_N": str(args.n),
    })
    if args.mode == "terrain":
        env["MC_SPINE_NOFEAT"] = "1"
        env["MC_SPINE_NOCARVE"] = "1"
    elif args.mode == "carvers":
        env["MC_SPINE_NOFEAT"] = "1"
    if args.noaq:
        env["MC_SPINE_NOAQ"] = "1"
    if dump_hm is not None:
        env["MC_SPINE_WG_DUMP_HM"] = str(dump_hm)
    if dump_canon is not None:
        env["MC_SPINE_WG_DUMP_CANON"] = str(dump_canon)
    if args.pop_order:
        env["MC_SPINE_POP_ORDER"] = str(Path(args.pop_order).resolve())
        if args.pop_order_scope:
            env["MC_SPINE_POP_ORDER_SCOPE"] = args.pop_order_scope
    return env


def run_port_dump(binary, env, dump_log):
    t0 = time.monotonic()
    proc = subprocess.run([str(binary)], cwd=ROOT, env=env, capture_output=True, text=True)
    wall = time.monotonic() - t0
    log_text = proc.stdout + proc.stderr
    dump_log.write_text(log_text, encoding="utf-8")
    if proc.returncode != 0:
        tail = "\n".join(log_text.splitlines()[-25:])
        raise PortTriageError(f"mc-spine dump failed (exit {proc.returncode}):\n{tail}")
    return wall


def execute(args):
    seed, meta_a = preflight(args)
    binary, build_wall = resolve_binary(args)
    binary_sha256 = chunk_triage.sha256_file(binary)

    cleanup_tmp = None
    if args.out_dir:
        out_dir = Path(args.out_dir).resolve()
        out_dir.mkdir(parents=True, exist_ok=True)
    elif args.keep_dump:
        out_dir = Path(tempfile.mkdtemp(prefix="port-triage-"))
    else:
        cleanup_tmp = tempfile.TemporaryDirectory(prefix="port-triage-")
        out_dir = Path(cleanup_tmp.name)

    try:
        dump_bin = out_dir / "dump.bin"
        dump_hm = (out_dir / "hm.bin") if args.port_hm else None
        dump_canon = (out_dir / "canon.tsv") if args.canon else None
        dump_log = out_dir / "dump.log"

        env = build_dump_env(args, seed, dump_bin, dump_hm, dump_canon)
        dump_wall = run_port_dump(binary, env, dump_log)

        if not dump_bin.is_file():
            raise PortTriageError(f"mc-spine exited 0 without writing {dump_bin}")
        if dump_hm is not None and not dump_hm.is_file():
            raise PortTriageError(f"mc-spine exited 0 without writing {dump_hm}")

        ct_args = SimpleNamespace(
            world=str(args.world),
            world_b=str(args.world_b) if args.world_b else None,
            cx=args.cx,
            cz=args.cz,
            n=args.n,
            port_tsv=None,
            port_bin=str(dump_bin),
            bin_ox=args.cx,
            bin_oz=args.cz,
            bin_n=args.n,
            min_y=args.min_y,
            height=args.height,
            port_hm=str(dump_hm) if dump_hm is not None else None,
            hm_name=args.hm_name,
            halo=args.halo,
            samples=args.samples,
            per_chunk=args.per_chunk,
            allow_incomplete=args.allow_incomplete,
            fail_on_diff=args.fail_on_diff,
            json_out=args.json_out,
        )
        try:
            triage_res = chunk_triage.triage_grid(ct_args)
        except chunk_triage.TriageError as exc:
            raise PortTriageError(str(exc)) from exc

        provenance = {
            "seed": seed,
            "preset": args.preset,
            "mode": args.mode,
            "noaq": args.noaq,
            "binary": str(binary),
            "binary_sha256": binary_sha256,
            "build_wall_s": round(build_wall, 3),
            "dump_wall_s": round(dump_wall, 3),
            "out_dir": str(out_dir) if (args.out_dir or args.keep_dump) else None,
            "level_dat": meta_a,
        }
        return provenance, triage_res, ct_args
    finally:
        if cleanup_tmp is not None:
            cleanup_tmp.cleanup()


def format_output(provenance, triage_res, ct_args):
    hdr = (
        f"== port_triage seed={provenance['seed']} preset={provenance['preset']} "
        f"mode={provenance['mode']} noaq={provenance['noaq']} ==\n"
        f"port binary: {provenance['binary']} sha256={provenance['binary_sha256'][:16]}... "
        f"build={provenance['build_wall_s']:.2f}s dump={provenance['dump_wall_s']:.2f}s\n"
    )
    if provenance["out_dir"]:
        hdr += f"artifacts: {provenance['out_dir']}\n"
    return hdr + chunk_triage.format_grid_report(triage_res, ct_args)


def build_parser():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("world", help="Native Anvil world directory")
    ap.add_argument("cx", type=int, help="Target chunk X coordinate (or grid origin X with --n)")
    ap.add_argument("cz", type=int, help="Target chunk Z coordinate (or grid origin Z with --n)")
    ap.add_argument("--n", type=int, default=1, help="Grid width N from (cx,cz) (default: 1)")
    ap.add_argument("--seed", type=int, help="World seed override (default: auto-read from <world>/level.dat)")
    ap.add_argument("--world-b", help="Optional second native world run to separate native run-to-run noise")
    ap.add_argument("--mode", choices=("full", "terrain", "carvers"), default="full",
                    help="Port worldgen stage mode (default: full)")
    ap.add_argument("--preset", choices=("overworld", "nether", "end"), default="overworld",
                    help="Offline noise preset (default: overworld)")
    ap.add_argument("--noaq", action="store_true", help="Set MC_SPINE_NOAQ=1 during port dump")
    ap.add_argument("--pop-order", help="Optional MC_SPINE_POP_ORDER chunk order TSV")
    ap.add_argument("--pop-order-scope", choices=("exact", "recorded"), default="exact",
                    help="MC_SPINE_POP_ORDER_SCOPE (default: exact)")
    ap.add_argument("--allow-stage-mismatch", action="store_true",
                    help="Allow --mode full even if native level.dat enables nofeat or generate_features=0")
    ap.add_argument("--allow-seed-mismatch", action="store_true",
                    help="Allow --seed / --world-b differing from level.dat seed")
    ap.add_argument("--release", action="store_true", help="Build and run --release binary")
    ap.add_argument("--jobs", type=int, default=2, help="Cargo build -j worker count (default: 2)")
    ap.add_argument("--binary", help="Use an explicit prebuilt mc-spine binary (skips cargo build)")
    ap.add_argument("--out-dir", help="Keep dump.bin / dump.log (and hm.bin / canon.tsv) in DIR")
    ap.add_argument("--keep-dump", action="store_true", help="Keep temporary dump directory and print its path")
    ap.add_argument("--port-hm", action="store_true", help="Also dump MC_SPINE_WG_DUMP_HM and compare heightmaps")
    ap.add_argument("--hm-name", default="OCEAN_FLOOR", help="Native Heightmaps entry for --port-hm (default: OCEAN_FLOOR)")
    ap.add_argument("--canon", action="store_true", help="Also emit MC_SPINE_WG_DUMP_CANON into --out-dir/--keep-dump")
    ap.add_argument("--min-y", type=int, help="Override min_y passed to chunk_triage")
    ap.add_argument("--height", type=int, help="Override height passed to chunk_triage")
    ap.add_argument("--halo", type=int, default=1, help="Halo radius around structure child BBs (default: 1)")
    ap.add_argument("--samples", type=int, default=5, help="Sample coords per pair/bucket (default: 5)")
    ap.add_argument("--per-chunk", action="store_true", help="In --n > 1 mode, print full report per differing chunk")
    ap.add_argument("--allow-incomplete", action="store_true", help="In --n > 1 mode, skip missing/non-full chunks")
    ap.add_argument("--json-out", help="Write JSON triage + port provenance report")
    ap.add_argument("--fail-on-diff", action="store_true", help="Exit 1 when all_air_diffs > 0")
    return ap


def main(argv=None):
    ap = build_parser()
    args = ap.parse_args(argv)
    try:
        provenance, triage_res, ct_args = execute(args)
    except PortTriageError as exc:
        print(f"port_triage: ERROR: {exc}", file=sys.stderr)
        return 2
    sys.stdout.write(format_output(provenance, triage_res, ct_args))
    if args.json_out:
        payload = chunk_triage.json_summary(triage_res)
        payload["port_provenance"] = provenance
        Path(args.json_out).write_text(json.dumps(payload, indent=2) + "\n")
    if args.fail_on_diff and triage_res["totals"]["all_air_diffs"] > 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
