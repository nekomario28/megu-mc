#!/usr/bin/env python3
"""Single-command causal triage for one chunk between native Anvil and port output.

  tools/chunk_triage.py <world> <cx> <cz> (--port-tsv PATH | --port-bin PATH) [options]

Why this exists: chunk parity investigations previously stitched together
`terrain_chunk_dump.py` (which drops `cave_air` and `void_air` unless `--all`),
ad-hoc structure NBT scripts, `world_diff.py`, and `heightmap_diff.py`. Skipping
`cave_air` hid 19 `cave_air -> air` cells on seed 4242 chunk (3,-2), and missing
coord/status checks turned mismatched inputs into misleading diff totals.

This tool reads the saved native chunk directly from the Anvil region file,
fails closed if the chunk is missing or not `full`, preserves `minecraft:air`
vs `minecraft:cave_air` vs `minecraft:void_air`, checks `--world-b` native noise
if supplied, maps every differing cell against referenced `structures.starts`
child bounding boxes (and a configurable `--halo`, default 1, for e.g. sub-floor
support planks), and prints a compact causal breakdown in one pass.

Exit codes:
  0  report completed (or 0 diffs when `--fail-on-diff` is set)
  1  diffs > 0 with `--fail-on-diff`
  2  invalid input, missing/incomplete target chunk, or coordinate mismatch
"""
import argparse
import hashlib
import json
import os
import re
import struct
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_region_blocks import chunk_bytes, parse_nbt  # noqa: E402

AIR_STATES = ("minecraft:air", "minecraft:cave_air", "minecraft:void_air")
DEFAULT_AIR = "minecraft:air"
HM_BITS = 9
HM_PER_LONG = 64 // HM_BITS
HM_MASK = (1 << HM_BITS) - 1
CHUNK_HEADER_RE = re.compile(r"^#\s*chunk\s+(-?\d+),(-?\d+)\b(?:\s+min_y\s+(-?\d+))?")


class TriageError(Exception):
    """Fail-closed error for missing, incomplete, or mismatched triage inputs."""


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            buf = f.read(1024 * 1024)
            if not buf:
                break
            h.update(buf)
    return h.hexdigest()


def collapse_air(state):
    return DEFAULT_AIR if state in AIR_STATES else state


def canonical_palette_entry(entry):
    name = entry.get("Name") or DEFAULT_AIR
    props = entry.get("Properties") or {}
    if not props:
        return name
    joined = ",".join(f"{k}={props[k]}" for k in sorted(props))
    return f"{name}[{joined}]"


_BLOCK_TABLE_CACHE = None


def load_block_state_table():
    global _BLOCK_TABLE_CACHE
    blocks_path = ROOT / "assets" / "blocks_1211.json"
    if not blocks_path.is_file():
        raise TriageError(f"missing block state table: {blocks_path}")
    st = blocks_path.stat()
    sig = (st.st_mtime_ns, st.st_size)
    if _BLOCK_TABLE_CACHE is not None and _BLOCK_TABLE_CACHE[0] == sig:
        return _BLOCK_TABLE_CACHE[1]
    by_id = {}
    for block in json.loads(blocks_path.read_text()):
        props = block["states"]
        base = block["minStateId"]
        name = "minecraft:" + block["name"]
        for state in range(base, block["maxStateId"] + 1):
            off = state - base
            kv = []
            mult = 1
            for p in reversed(props):
                vals = p.get("values") or ["true", "false"]
                kv.append((p["name"], vals[(off // mult) % len(vals)]))
                mult *= len(vals)
            if not kv:
                by_id[state] = name
            else:
                kv.sort(key=lambda item: item[0])
                joined = ",".join(f"{k}={v}" for k, v in kv)
                by_id[state] = f"{name}[{joined}]"
    _BLOCK_TABLE_CACHE = (sig, by_id)
    return by_id


def read_chunk_nbt(world, cx, cz):
    rx, rz = cx >> 5, cz >> 5
    mca = Path(world) / "region" / f"r.{rx}.{rz}.mca"
    if not mca.is_file():
        raise TriageError(f"missing region file {mca} for chunk ({cx},{cz})")
    try:
        raw = chunk_bytes(str(world), cx, cz)
    except SystemExit as exc:
        raise TriageError(str(exc)) from exc
    nbt, _ = parse_nbt(raw)
    if not isinstance(nbt, dict):
        raise TriageError(f"invalid NBT root in {world} chunk ({cx},{cz})")
    return nbt


def cached_read_chunk_nbt(world, cx, cz, nbt_cache=None):
    if nbt_cache is None:
        return read_chunk_nbt(world, cx, cz)
    key = (str(world), cx, cz)
    if key in nbt_cache:
        val = nbt_cache[key]
        if isinstance(val, TriageError):
            raise val
        return val
    try:
        val = read_chunk_nbt(world, cx, cz)
    except TriageError as exc:
        nbt_cache[key] = exc
        raise
    nbt_cache[key] = val
    return val


def unpack_heightmap(longs, min_y):
    out = []
    for i in range(256):
        word_idx = i // HM_PER_LONG
        if word_idx >= len(longs):
            raise TriageError("truncated Heightmaps long array")
        word = longs[word_idx] & 0xFFFFFFFFFFFFFFFF
        val = (word >> ((i % HM_PER_LONG) * HM_BITS)) & HM_MASK
        out.append(val + min_y)
    return out


def unpack_section_biomes(section):
    biomes = section.get("biomes") or {}
    palette = biomes.get("palette") or []
    if not palette:
        return ["<unknown>"] * 64
    if len(palette) == 1:
        return [palette[0]] * 64
    data = biomes.get("data") or []
    bits = max(1, (len(palette) - 1).bit_length())
    per = 64 // bits
    mask = (1 << bits) - 1
    out = []
    for i in range(64):
        word_idx = i // per
        if word_idx >= len(data):
            out.append("<truncated-biome>")
            continue
        word = data[word_idx] & 0xFFFFFFFFFFFFFFFF
        val = (word >> ((i % per) * bits)) & mask
        out.append(palette[val] if val < len(palette) else "<bad-biome>")
    return out


def _section_is_air(sec):
    """Is this chunk section an all-air palette (i.e. nothing to compare)?"""
    bs = sec.get("block_states") or {}
    palette = [canonical_palette_entry(e) for e in (bs.get("palette") or [])]
    if not palette:
        return True
    return len(palette) == 1 and palette[0] in AIR_STATES


def reject_out_of_window_sections(nbt, min_y, height, cx, cz):
    """Fail closed when the native chunk holds blocks the port's window does not cover.

    A section is written only when it is not air-only, so the port's window (`min_y`,
    `height`) must contain every non-air section of the native chunk. Silently ignoring
    a section outside the window would compare a subset of the native chunk and report
    a diff-free chunk that is not actually equal.
    """
    lo = min_y
    hi = min_y + height
    for sec in nbt.get("sections") or []:
        if "Y" not in sec or "block_states" not in sec:
            continue
        sy = int(sec["Y"])
        if lo <= sy * 16 and (sy + 1) * 16 <= hi:
            continue
        if _section_is_air(sec):
            continue
        raise TriageError(
            f"{cx},{cz}: native section Y={sy} has blocks outside the comparison window "
            f"y={lo}..{hi - 1}; pass --min-y/--height that cover it"
        )


def load_native_chunk(world, cx, cz, *, min_y_override=None, height_override=None, nbt_cache=None):
    nbt = cached_read_chunk_nbt(world, cx, cz, nbt_cache)
    status = nbt.get("Status")
    if status not in ("minecraft:full", "full"):
        raise TriageError(
            f"{world} chunk ({cx},{cz}) Status={status!r}, expected 'minecraft:full'"
        )
    sections_raw = nbt.get("sections") or []
    block_sections = {
        int(s["Y"]): s
        for s in sections_raw
        if "Y" in s and "block_states" in s
    }
    if not block_sections:
        raise TriageError(f"{world} chunk ({cx},{cz}) has no block_states sections")

    if min_y_override is not None:
        min_y = min_y_override
    elif nbt.get("yPos") is not None:
        min_y = int(nbt["yPos"]) * 16
    else:
        min_y = min(block_sections) * 16

    if height_override is not None:
        height = height_override
    else:
        max_sy = max(block_sections)
        min_sy = min_y // 16
        height = (max_sy - min_sy + 1) * 16

    if height <= 0 or height % 16 != 0 or min_y % 16 != 0:
        raise TriageError(f"chunk height/min_y must be section-aligned (got min_y={min_y}, height={height})")
    if min_y_override is not None or height_override is not None:
        reject_out_of_window_sections(nbt, min_y, height, cx, cz)

    num_sections = height // 16
    min_sy = min_y // 16
    cells = [DEFAULT_AIR] * (num_sections * 4096)
    quarts = {}
    biome_counts = Counter()

    for sidx in range(num_sections):
        sy = min_sy + sidx
        sec = block_sections.get(sy)
        if sec is None:
            continue
        bs = sec["block_states"]
        palette = [canonical_palette_entry(e) for e in (bs.get("palette") or [])]
        if not palette:
            continue
        data = bs.get("data") or []
        base = sidx * 4096
        if len(palette) == 1:
            st = palette[0]
            for i in range(4096):
                cells[base + i] = st
        else:
            bits = max(4, (len(palette) - 1).bit_length())
            per = 64 // bits
            mask = (1 << bits) - 1
            for i in range(4096):
                word_idx = i // per
                if word_idx >= len(data):
                    raise TriageError(f"truncated block_states data in section {sy}")
                word = data[word_idx] & 0xFFFFFFFFFFFFFFFF
                val = (word >> ((i % per) * bits)) & mask
                cells[base + i] = palette[val] if val < len(palette) else "<bad-palette>"

        qlist = unpack_section_biomes(sec)
        quarts[sy] = qlist
        for b in qlist:
            biome_counts[b] += 1

    air_kinds = Counter()
    non_air = 0
    for st in cells:
        if st in AIR_STATES:
            air_kinds[st] += 1
        else:
            non_air += 1

    hm_raw = nbt.get("Heightmaps") or {}
    heightmaps = {}
    for hm_name, longs in hm_raw.items():
        if isinstance(longs, list) and longs:
            heightmaps[hm_name] = unpack_heightmap(longs, min_y)

    return {
        "nbt": nbt,
        "world": str(world),
        "cx": cx,
        "cz": cz,
        "min_y": min_y,
        "height": height,
        "sections": num_sections,
        "cells": cells,
        "quarts": quarts,
        "biome_counts": biome_counts,
        "heightmaps": heightmaps,
        "meta": {
            "DataVersion": nbt.get("DataVersion"),
            "Status": status,
            "LastUpdate": nbt.get("LastUpdate"),
            "InhabitedTime": nbt.get("InhabitedTime"),
            "isLightOn": nbt.get("isLightOn"),
            "block_entities": len(nbt.get("block_entities") or []),
            "block_ticks": len(nbt.get("block_ticks") or []),
            "fluid_ticks": len(nbt.get("fluid_ticks") or []),
            "air": air_kinds["minecraft:air"],
            "cave_air": air_kinds["minecraft:cave_air"],
            "void_air": air_kinds["minecraft:void_air"],
            "non_air": non_air,
            "total": len(cells),
        },
    }


def decode_chunk_pos_long(val):
    x = val & 0xFFFFFFFF
    if x >= 0x80000000:
        x -= 0x100000000
    z = (val >> 32) & 0xFFFFFFFF
    if z >= 0x80000000:
        z -= 0x100000000
    return int(x), int(z)


def bb_intersects(bb, box):
    return (
        bb[0] <= box[3] and bb[3] >= box[0]
        and bb[1] <= box[4] and bb[4] >= box[1]
        and bb[2] <= box[5] and bb[5] >= box[2]
    )


def bb_distance(bb, x, y, z):
    dx = 0 if bb[0] <= x <= bb[3] else (bb[0] - x if x < bb[0] else x - bb[3])
    dy = 0 if bb[1] <= y <= bb[4] else (bb[1] - y if y < bb[1] else y - bb[4])
    dz = 0 if bb[2] <= z <= bb[5] else (bb[2] - z if z < bb[2] else z - bb[5])
    return max(dx, dy, dz)


def collect_structure_pieces(world, native_chunk, halo, *, nbt_cache=None):
    cx, cz = native_chunk["cx"], native_chunk["cz"]
    min_y, height = native_chunk["min_y"], native_chunk["height"]
    chunk_box = (cx * 16, min_y, cz * 16, cx * 16 + 15, min_y + height - 1, cz * 16 + 15)
    halo_box = (
        chunk_box[0] - halo, chunk_box[1] - halo, chunk_box[2] - halo,
        chunk_box[3] + halo, chunk_box[4] + halo, chunk_box[5] + halo,
    )

    nbt = native_chunk["nbt"]
    structs = nbt.get("structures") or nbt.get("Structures") or {}
    refs_map = structs.get("References") or {}
    starts_self = structs.get("starts") or {}

    references = {}
    start_chunks_to_read = {}
    for sid, packed_list in sorted(refs_map.items()):
        coords = [decode_chunk_pos_long(v) for v in (packed_list or [])]
        references[sid] = coords
        for scx, scz in coords:
            start_chunks_to_read.setdefault((scx, scz), set()).add(sid)

    pieces = []
    unavailable_starts = []

    def scan_starts(starts_dict, scx, scz, filter_sids=None):
        for sid, start_obj in sorted((starts_dict or {}).items()):
            if filter_sids is not None and sid not in filter_sids:
                continue
            if not isinstance(start_obj, dict):
                continue
            for idx, child in enumerate(start_obj.get("Children") or []):
                bb = child.get("BB")
                if not bb or len(bb) != 6:
                    continue
                bb_t = tuple(int(v) for v in bb)
                if bb_intersects(bb_t, halo_box):
                    pieces.append({
                        "structure": sid,
                        "start_chunk": (scx, scz),
                        "child_index": idx,
                        "child_id": child.get("id", "<unknown>"),
                        "BB": bb_t,
                        "intersects_chunk": bb_intersects(bb_t, chunk_box),
                    })

    scan_starts(starts_self, cx, cz)

    for (scx, scz), sids in sorted(start_chunks_to_read.items()):
        if (scx, scz) == (cx, cz):
            continue
        try:
            start_nbt = cached_read_chunk_nbt(world, scx, scz, nbt_cache)
        except TriageError as exc:
            unavailable_starts.append({"chunk": (scx, scz), "structures": sorted(sids), "reason": str(exc)})
            continue
        s_structs = start_nbt.get("structures") or start_nbt.get("Structures") or {}
        scan_starts(s_structs.get("starts") or {}, scx, scz, filter_sids=sids)

    return {
        "references": references,
        "pieces": pieces,
        "unavailable_starts": unavailable_starts,
    }


def load_port_tsv(path, cx, cz, min_y, height):
    cells = [DEFAULT_AIR] * ((height // 16) * 4096)
    seen = set()
    x0, z0 = cx * 16, cz * 16
    y_max = min_y + height - 1
    rows = 0
    cave_air = 0
    void_air = 0
    non_air = 0
    with open(path, "r", encoding="utf-8") as f:
        for lineno, raw in enumerate(f, 1):
            line = raw.rstrip("\r\n")
            if not line:
                continue
            if line.startswith("#"):
                m = CHUNK_HEADER_RE.match(line)
                if m:
                    hcx, hcz = int(m.group(1)), int(m.group(2))
                    if (hcx, hcz) != (cx, cz):
                        raise TriageError(
                            f"{path}:{lineno}: header chunk ({hcx},{hcz}) != requested ({cx},{cz})"
                        )
                    if m.group(3) is not None and int(m.group(3)) != min_y:
                        raise TriageError(
                            f"{path}:{lineno}: header min_y {m.group(3)} != native min_y {min_y}"
                        )
                continue
            parts = line.split("\t")
            if len(parts) != 4:
                raise TriageError(f"{path}:{lineno}: expected 4 tab-separated columns, got {len(parts)}")
            x, y, z = int(parts[0]), int(parts[1]), int(parts[2])
            state = parts[3]
            if not (x0 <= x < x0 + 16 and z0 <= z < z0 + 16 and min_y <= y <= y_max):
                raise TriageError(
                    f"{path}:{lineno}: coord ({x},{y},{z}) outside chunk ({cx},{cz}) y={min_y}..{y_max}"
                )
            lx, lz = x - x0, z - z0
            off = ((y - min_y) >> 4) * 4096 + ((y - min_y) & 15) * 256 + lz * 16 + lx
            if off in seen:
                raise TriageError(f"{path}:{lineno}: duplicate coordinate ({x},{y},{z})")
            seen.add(off)
            cells[off] = state
            rows += 1
            if state == "minecraft:cave_air":
                cave_air += 1
            elif state == "minecraft:void_air":
                void_air += 1
            elif state != DEFAULT_AIR:
                non_air += 1
    return {
        "kind": "tsv",
        "path": str(path),
        "sha256": sha256_file(path),
        "cells": cells,
        "rows": rows,
        "cave_air": cave_air,
        "void_air": void_air,
        "non_air": non_air,
    }


def load_port_tsv_grid(path, chunk_coords, min_y, height):
    want = set(chunk_coords)
    chunk_cells = (height // 16) * 4096
    per_chunk = {}
    seen_per_chunk = {}
    cur = None
    y_max = min_y + height - 1
    file_hash = sha256_file(path)

    with open(path, "r", encoding="utf-8") as f:
        for lineno, raw in enumerate(f, 1):
            line = raw.rstrip("\r\n")
            if not line:
                continue
            if line.startswith("#"):
                m = CHUNK_HEADER_RE.match(line)
                if m:
                    hcx, hcz = int(m.group(1)), int(m.group(2))
                    if m.group(3) is not None and int(m.group(3)) != min_y:
                        raise TriageError(
                            f"{path}:{lineno}: header min_y {m.group(3)} != native min_y {min_y}"
                        )
                    cur = (hcx, hcz)
                    if cur in want and cur not in per_chunk:
                        per_chunk[cur] = {
                            "kind": "tsv",
                            "path": str(path),
                            "sha256": file_hash,
                            "cells": [DEFAULT_AIR] * chunk_cells,
                            "rows": 0,
                            "cave_air": 0,
                            "void_air": 0,
                            "non_air": 0,
                        }
                        seen_per_chunk[cur] = set()
                continue
            if cur is None:
                raise TriageError(f"{path}:{lineno}: block row before '# chunk' header in multi-chunk TSV")
            parts = line.split("\t")
            if len(parts) != 4:
                raise TriageError(f"{path}:{lineno}: expected 4 tab-separated columns, got {len(parts)}")
            x, y, z = int(parts[0]), int(parts[1]), int(parts[2])
            state = parts[3]
            hcx, hcz = cur
            x0, z0 = hcx * 16, hcz * 16
            if not (x0 <= x < x0 + 16 and z0 <= z < z0 + 16 and min_y <= y <= y_max):
                raise TriageError(
                    f"{path}:{lineno}: coord ({x},{y},{z}) outside chunk ({hcx},{hcz}) y={min_y}..{y_max}"
                )
            if cur not in want:
                continue
            entry = per_chunk[cur]
            seen = seen_per_chunk[cur]
            lx, lz = x - x0, z - z0
            off = ((y - min_y) >> 4) * 4096 + ((y - min_y) & 15) * 256 + lz * 16 + lx
            if off in seen:
                raise TriageError(f"{path}:{lineno}: duplicate coordinate ({x},{y},{z})")
            seen.add(off)
            entry["cells"][off] = state
            entry["rows"] += 1
            if state == "minecraft:cave_air":
                entry["cave_air"] += 1
            elif state == "minecraft:void_air":
                entry["void_air"] += 1
            elif state != DEFAULT_AIR:
                entry["non_air"] += 1

    missing = sorted(want - set(per_chunk))
    if missing:
        raise TriageError(f"{path}: missing '# chunk' sections for {missing[:4]}")
    return per_chunk


def load_port_bin(path, cx, cz, min_y, height, *, bin_ox, bin_oz, bin_n,
                  raw_bytes=None, sha256_hex=None, by_id=None):
    if bin_n <= 0:
        raise TriageError(f"--bin-n must be positive, got {bin_n}")
    dx = cx - bin_ox
    dz = cz - bin_oz
    if not (0 <= dx < bin_n and 0 <= dz < bin_n):
        raise TriageError(
            f"target chunk ({cx},{cz}) outside binary dump window ({bin_ox},{bin_oz}) + {bin_n}x{bin_n}"
        )
    chunk_cells = (height // 16) * 4096
    want_bytes = bin_n * bin_n * chunk_cells * 4
    raw = Path(path).read_bytes() if raw_bytes is None else raw_bytes
    if len(raw) != want_bytes:
        raise TriageError(
            f"binary dump {path} has {len(raw)} bytes, expected {want_bytes} for {bin_n}x{bin_n} height={height}"
        )
    if by_id is None:
        by_id = load_block_state_table()
    if sha256_hex is None:
        sha256_hex = hashlib.sha256(raw).hexdigest()
    chunk_idx = dz * bin_n + dx
    start = chunk_idx * chunk_cells * 4
    ids = struct.unpack_from(f"<{chunk_cells}I", raw, start)
    cells = []
    cave_air = 0
    void_air = 0
    non_air = 0
    for st in ids:
        name = by_id.get(st)
        if name is None:
            raise TriageError(f"unknown block state id {st} in {path}")
        cells.append(name)
        if name == "minecraft:cave_air":
            cave_air += 1
        elif name == "minecraft:void_air":
            void_air += 1
        elif name != DEFAULT_AIR:
            non_air += 1
    return {
        "kind": "bin",
        "path": str(path),
        "sha256": sha256_hex,
        "cells": cells,
        "rows": chunk_cells,
        "cave_air": cave_air,
        "void_air": void_air,
        "non_air": non_air,
    }


def quart_biome_at(native_chunk, x, y, z):
    cx, cz = native_chunk["cx"], native_chunk["cz"]
    sy = y >> 4
    qlist = native_chunk["quarts"].get(sy)
    if not qlist:
        return "<unknown>"
    qx = (x - cx * 16) >> 2
    qy = (y - sy * 16) >> 2
    qz = (z - cz * 16) >> 2
    return qlist[qy * 16 + qz * 4 + qx]


def compute_native_tops(cells, min_y, height):
    """Highest non-air y (for layer classification) and first free y above non-air."""
    tops = [min_y - 1] * 256
    sections = height // 16
    for sidx in range(sections - 1, -1, -1):
        sbase = sidx * 4096
        sy_base = min_y + sidx * 16
        for ly in range(15, -1, -1):
            y = sy_base + ly
            ybase = sbase + ly * 256
            for xz in range(256):
                if tops[xz] == min_y - 1 and cells[ybase + xz] not in AIR_STATES:
                    tops[xz] = y
    return tops


def classify_cell(x, y, z, lx, lz, min_y, native_tops, pieces, halo):
    inside = []
    near = []
    for p in pieces:
        dist = bb_distance(p["BB"], x, y, z)
        if dist == 0:
            inside.append(p)
        elif halo > 0 and dist <= halo:
            near.append((dist, p))
    if inside:
        inside.sort(key=lambda p: (p["structure"], p["child_index"]))
        p = inside[0]
        return f"inside_structure_bb | {p['structure']} | {p['child_id']}#{p['child_index']}"
    if near:
        near.sort(key=lambda item: (item[0], item[1]["structure"], item[1]["child_index"]))
        dist, p = near[0]
        return f"near_structure_halo(d={dist}) | {p['structure']} | {p['child_id']}#{p['child_index']}"
    top = native_tops[lz * 16 + lx]
    if y <= min_y + 4:
        band = "bedrock"
    elif y > top:
        band = "above_surface"
    elif y >= top - 4:
        band = "surface"
    else:
        band = "underground"
    return f"outside_structures | {band}"


def load_port_hm(path, cx, cz, *, bin_ox, bin_oz, bin_n, raw_bytes=None):
    raw = Path(path).read_bytes() if raw_bytes is None else raw_bytes
    want_bytes = bin_n * bin_n * 256 * 4
    if len(raw) != want_bytes:
        raise TriageError(f"port heightmap {path} has {len(raw)} bytes, expected {want_bytes} for {bin_n}x{bin_n}")
    dx = cx - bin_ox
    dz = cz - bin_oz
    if not (0 <= dx < bin_n and 0 <= dz < bin_n):
        raise TriageError(f"target chunk ({cx},{cz}) outside heightmap window ({bin_ox},{bin_oz}) + {bin_n}x{bin_n}")
    start = (dz * bin_n + dx) * 256 * 4
    return list(struct.unpack_from("<256i", raw, start))


def _resolve_bin_window(args):
    n = getattr(args, "n", 1)
    if n <= 0:
        raise TriageError(f"--n must be positive, got {n}")
    bin_n = n if args.bin_n is None else args.bin_n
    bin_ox = args.cx if args.bin_ox is None else args.bin_ox
    bin_oz = args.cz if args.bin_oz is None else args.bin_oz
    return bin_ox, bin_oz, bin_n


def _compare_chunk(args, native_a, native_b, port, struct_info, port_hm):
    min_y = native_a["min_y"]
    height = native_a["height"]

    x0, z0 = native_a["cx"] * 16, native_a["cz"] * 16
    num_sections = native_a["sections"]

    all_air_diffs = 0
    non_air_diffs = 0
    native_ab_all_diffs = 0
    native_ab_non_air_diffs = 0
    consistent_diffs = 0
    unstable_diffs = 0
    unstable_patterns = Counter()

    buckets = defaultdict(lambda: {
        "count": 0,
        "min_xyz": [10**9, 10**9, 10**9],
        "max_xyz": [-10**9, -10**9, -10**9],
        "biomes": Counter(),
        "pairs": Counter(),
        "samples": defaultdict(list),
    })
    overall_pairs = Counter()
    overall_pair_yrange = {}

    cells_a = native_a["cells"]
    cells_b = native_b["cells"] if native_b else None
    cells_p = port["cells"]
    expected_cells = num_sections * 4096
    for side, cells in (("native_a", cells_a), ("native_b", cells_b), ("port", cells_p)):
        if cells is not None and len(cells) != expected_cells:
            raise TriageError(f"{side} has {len(cells)} cells, expected {expected_cells}")
    native_tops = compute_native_tops(cells_a, min_y, height)

    for sidx in range(num_sections):
        sbase = sidx * 4096
        sy_base = min_y + sidx * 16
        for ly in range(16):
            y = sy_base + ly
            ybase = sbase + ly * 256
            # Exact state equality includes air kinds and every property. Native B
            # must also match: a port match alone does not remove A/B variation.
            row_a = cells_a[ybase:ybase + 256]
            if row_a == cells_p[ybase:ybase + 256] and (
                cells_b is None or row_a == cells_b[ybase:ybase + 256]
            ):
                continue
            for lz in range(16):
                z = z0 + lz
                zbase = ybase + lz * 16
                for lx in range(16):
                    idx = zbase + lx
                    va = cells_a[idx]
                    vp = cells_p[idx]
                    vb = cells_b[idx] if cells_b is not None else va
                    if cells_b is not None and va != vb:
                        native_ab_all_diffs += 1
                        if collapse_air(va) != collapse_air(vb):
                            native_ab_non_air_diffs += 1
                    if va == vp:
                        continue
                    x = x0 + lx
                    all_air_diffs += 1
                    if collapse_air(va) != collapse_air(vp):
                        non_air_diffs += 1
                    if cells_b is not None:
                        if va == vb:
                            consistent_diffs += 1
                        else:
                            unstable_diffs += 1
                            unstable_patterns[(va, vb, vp)] += 1

                    overall_pairs[(va, vp)] += 1
                    if (va, vp) not in overall_pair_yrange:
                        overall_pair_yrange[(va, vp)] = [y, y]
                    else:
                        yr = overall_pair_yrange[(va, vp)]
                        if y < yr[0]:
                            yr[0] = y
                        if y > yr[1]:
                            yr[1] = y

                    bucket_key = classify_cell(x, y, z, lx, lz, min_y, native_tops,
                                               struct_info["pieces"], args.halo)
                    b = buckets[bucket_key]
                    b["count"] += 1
                    for axis, val in enumerate((x, y, z)):
                        if val < b["min_xyz"][axis]:
                            b["min_xyz"][axis] = val
                        if val > b["max_xyz"][axis]:
                            b["max_xyz"][axis] = val
                    b["biomes"][quart_biome_at(native_a, x, y, z)] += 1
                    b["pairs"][(va, vp)] += 1
                    if len(b["samples"][(va, vp)]) < args.samples:
                        b["samples"][(va, vp)].append((x, y, z))

    hm_report = {}
    for hm_name in ("WORLD_SURFACE", "OCEAN_FLOOR", "MOTION_BLOCKING", "MOTION_BLOCKING_NO_LEAVES"):
        vals = native_a["heightmaps"].get(hm_name)
        if vals is not None:
            hm_report[hm_name] = [min(vals), max(vals)]
    saved_ws = native_a["heightmaps"].get("WORLD_SURFACE")
    if saved_ws is not None:
        hm_report["WORLD_SURFACE_vs_non_air_top_mismatches"] = sum(
            1 for i in range(256) if saved_ws[i] != native_tops[i] + 1
        )
    if port_hm is not None:
        saved_hm = native_a["heightmaps"].get(args.hm_name)
        if saved_hm is None:
            raise TriageError(f"native chunk missing Heightmaps[{args.hm_name!r}]")
        hm_hist = Counter(port_hm[i] - saved_hm[i] for i in range(256) if port_hm[i] != saved_hm[i])
        hm_report["port_hm_name"] = args.hm_name
        hm_report["port_vs_native_saved_diff_cols"] = sum(hm_hist.values())
        hm_report["port_vs_native_saved_deltas"] = dict(hm_hist)

    return {
        "native_a": native_a,
        "native_b": native_b,
        "port": port,
        "structures": struct_info,
        "totals": {
            "all_air_diffs": all_air_diffs,
            "non_air_projection_diffs": non_air_diffs,
            "native_ab_all_diffs": native_ab_all_diffs,
            "native_ab_non_air_diffs": native_ab_non_air_diffs,
            "consistent_diffs": consistent_diffs,
            "unstable_diffs": unstable_diffs,
        },
        "unstable_patterns": unstable_patterns,
        "buckets": buckets,
        "overall_pairs": overall_pairs,
        "overall_pair_yrange": overall_pair_yrange,
        "heightmaps": hm_report,
    }


def triage(args):
    nbt_cache = {}
    native_a = load_native_chunk(args.world, args.cx, args.cz,
                                 min_y_override=args.min_y, height_override=args.height,
                                 nbt_cache=nbt_cache)
    min_y = native_a["min_y"]
    height = native_a["height"]

    native_b = None
    if args.world_b:
        native_b = load_native_chunk(args.world_b, args.cx, args.cz,
                                     min_y_override=min_y, height_override=height,
                                     nbt_cache=nbt_cache)

    bin_ox, bin_oz, bin_n = _resolve_bin_window(args)

    if args.port_tsv:
        port = load_port_tsv(args.port_tsv, args.cx, args.cz, min_y, height)
    else:
        port = load_port_bin(args.port_bin, args.cx, args.cz, min_y, height,
                             bin_ox=bin_ox, bin_oz=bin_oz, bin_n=bin_n)

    struct_info = collect_structure_pieces(args.world, native_a, args.halo, nbt_cache=nbt_cache)
    port_hm = None
    if args.port_hm:
        port_hm = load_port_hm(args.port_hm, args.cx, args.cz,
                               bin_ox=bin_ox, bin_oz=bin_oz, bin_n=bin_n)

    return _compare_chunk(args, native_a, native_b, port, struct_info, port_hm)


def triage_grid(args):
    n = getattr(args, "n", 1)
    if n == 1:
        return triage(args)

    bin_ox, bin_oz, bin_n = _resolve_bin_window(args)
    allow_incomplete = getattr(args, "allow_incomplete", False)
    coords = [(args.cx + dx, args.cz + dz) for dz in range(n) for dx in range(n)]
    nbt_cache = {}

    native_chunks = []
    skipped = []
    ref_min_y = args.min_y
    ref_height = args.height

    for cx, cz in coords:
        try:
            na = load_native_chunk(args.world, cx, cz,
                                   min_y_override=ref_min_y, height_override=ref_height,
                                   nbt_cache=nbt_cache)
        except TriageError as exc:
            if allow_incomplete:
                skipped.append({"chunk": [cx, cz], "side": "native_a", "reason": str(exc)})
                continue
            raise
        if ref_min_y is None:
            ref_min_y = na["min_y"]
        elif na["min_y"] != ref_min_y:
            raise TriageError(f"chunk ({cx},{cz}) min_y={na['min_y']} != grid min_y={ref_min_y}")
        if ref_height is None:
            ref_height = na["height"]
        elif na["height"] != ref_height:
            raise TriageError(f"chunk ({cx},{cz}) height={na['height']} != grid height={ref_height}")

        nb = None
        if args.world_b:
            try:
                nb = load_native_chunk(args.world_b, cx, cz,
                                       min_y_override=ref_min_y, height_override=ref_height,
                                       nbt_cache=nbt_cache)
            except TriageError as exc:
                if allow_incomplete:
                    skipped.append({"chunk": [cx, cz], "side": "native_b", "reason": str(exc)})
                    continue
                raise
        native_chunks.append((na, nb))

    if not native_chunks:
        raise TriageError(f"0 complete native chunks in {n}x{n} grid at ({args.cx},{args.cz})")

    compared_coords = [(na["cx"], na["cz"]) for na, _ in native_chunks]
    if args.port_tsv:
        port_map = load_port_tsv_grid(args.port_tsv, compared_coords, ref_min_y, ref_height)
    else:
        raw_bin = Path(args.port_bin).read_bytes()
        sha_bin = hashlib.sha256(raw_bin).hexdigest()
        by_id = load_block_state_table()
        port_map = {
            (na["cx"], na["cz"]): load_port_bin(
                args.port_bin, na["cx"], na["cz"], ref_min_y, ref_height,
                bin_ox=bin_ox, bin_oz=bin_oz, bin_n=bin_n,
                raw_bytes=raw_bin, sha256_hex=sha_bin, by_id=by_id,
            )
            for na, _ in native_chunks
        }

    raw_hm = Path(args.port_hm).read_bytes() if args.port_hm else None

    chunk_results = []
    agg_totals = {
        "all_air_diffs": 0,
        "non_air_projection_diffs": 0,
        "native_ab_all_diffs": 0,
        "native_ab_non_air_diffs": 0,
        "consistent_diffs": 0,
        "unstable_diffs": 0,
        "total_cells": 0,
    }
    agg_unstable = Counter()
    agg_pairs = Counter()
    agg_pair_yrange = {}
    agg_buckets = defaultdict(lambda: {
        "count": 0,
        "min_xyz": [10**9, 10**9, 10**9],
        "max_xyz": [-10**9, -10**9, -10**9],
        "biomes": Counter(),
        "pairs": Counter(),
        "samples": defaultdict(list),
    })
    agg_hm_diff_cols = 0

    for na, nb in native_chunks:
        cx, cz = na["cx"], na["cz"]
        struct_info = collect_structure_pieces(args.world, na, args.halo, nbt_cache=nbt_cache)
        port_hm = (
            load_port_hm(args.port_hm, cx, cz, bin_ox=bin_ox, bin_oz=bin_oz, bin_n=bin_n, raw_bytes=raw_hm)
            if raw_hm is not None
            else None
        )
        res = _compare_chunk(args, na, nb, port_map[(cx, cz)], struct_info, port_hm)
        chunk_results.append(res)

        for k in ("all_air_diffs", "non_air_projection_diffs", "native_ab_all_diffs",
                  "native_ab_non_air_diffs", "consistent_diffs", "unstable_diffs"):
            agg_totals[k] += res["totals"][k]
        agg_totals["total_cells"] += na["meta"]["total"]
        agg_unstable.update(res["unstable_patterns"])
        agg_pairs.update(res["overall_pairs"])
        for pair, (y0, y1) in res["overall_pair_yrange"].items():
            if pair not in agg_pair_yrange:
                agg_pair_yrange[pair] = [y0, y1]
            else:
                agg_pair_yrange[pair][0] = min(agg_pair_yrange[pair][0], y0)
                agg_pair_yrange[pair][1] = max(agg_pair_yrange[pair][1], y1)
        for key, b in res["buckets"].items():
            ab = agg_buckets[key]
            ab["count"] += b["count"]
            for axis in range(3):
                ab["min_xyz"][axis] = min(ab["min_xyz"][axis], b["min_xyz"][axis])
                ab["max_xyz"][axis] = max(ab["max_xyz"][axis], b["max_xyz"][axis])
            ab["biomes"].update(b["biomes"])
            ab["pairs"].update(b["pairs"])
            for pair, pts in b["samples"].items():
                room = args.samples - len(ab["samples"][pair])
                if room > 0:
                    ab["samples"][pair].extend(pts[:room])
        if raw_hm is not None:
            agg_hm_diff_cols += res["heightmaps"].get("port_vs_native_saved_diff_cols", 0)

    sample_port = chunk_results[0]["port"]
    return {
        "is_grid": True,
        "origin": [args.cx, args.cz],
        "n": n,
        "min_y": ref_min_y,
        "height": ref_height,
        "world": str(args.world),
        "world_b": str(args.world_b) if args.world_b else None,
        "port_meta": {
            "kind": sample_port["kind"],
            "path": sample_port["path"],
            "sha256": sample_port["sha256"],
        },
        "skipped": skipped,
        "chunks": chunk_results,
        "totals": agg_totals,
        "unstable_patterns": agg_unstable,
        "overall_pairs": agg_pairs,
        "overall_pair_yrange": agg_pair_yrange,
        "buckets": agg_buckets,
        "heightmap_diff_cols": agg_hm_diff_cols if raw_hm is not None else None,
    }


def format_report(res, args):
    na = res["native_a"]
    nb = res["native_b"]
    port = res["port"]
    totals = res["totals"]
    lines = []
    ma = na["meta"]
    lines.append(
        f"== chunk_triage ({na['cx']},{na['cz']}) min_y={na['min_y']} height={na['height']} =="
    )
    lines.append(
        f"native A: {na['world']}  DataVersion={ma['DataVersion']} Status={ma['Status']} "
        f"LastUpdate={ma['LastUpdate']} InhabitedTime={ma['InhabitedTime']} "
        f"BE={ma['block_entities']} bTicks={ma['block_ticks']} fTicks={ma['fluid_ticks']}"
    )
    lines.append(
        f"  cells={ma['total']} non_air={ma['non_air']} air={ma['air']} "
        f"cave_air={ma['cave_air']} void_air={ma['void_air']}"
    )
    top_biomes = ", ".join(f"{k}:{v}" for k, v in na["biome_counts"].most_common(6))
    lines.append(f"  quart biomes (4x4x4): {top_biomes}")
    if nb is not None:
        mb = nb["meta"]
        lines.append(
            f"native B: {nb['world']}  LastUpdate={mb['LastUpdate']} non_air={mb['non_air']} "
            f"cave_air={mb['cave_air']} (A vs B diffs: all_air={totals['native_ab_all_diffs']}, "
            f"non_air={totals['native_ab_non_air_diffs']})"
        )
    lines.append(
        f"port ({port['kind']}): {port['path']} sha256={port['sha256'][:16]}... "
        f"non_air={port['non_air']} cave_air={port['cave_air']} void_air={port['void_air']}"
    )

    refs = res["structures"]["references"]
    pieces = res["structures"]["pieces"]
    unavail = res["structures"]["unavailable_starts"]
    lines.append(f"structures: references={ {k: v for k, v in refs.items()} }")
    for p in pieces:
        tag = "INTERSECTS" if p["intersects_chunk"] else f"HALO<={args.halo}"
        lines.append(
            f"  [{tag}] {p['structure']} start={p['start_chunk']} "
            f"{p['child_id']}#{p['child_index']} BB={list(p['BB'])}"
        )
    for u in unavail:
        lines.append(f"  [UNAVAILABLE start {u['chunk']} for {u['structures']}: {u['reason']}]")

    lines.append(
        f"diff summary: all_air_kinds={totals['all_air_diffs']} / {ma['total']}  "
        f"(lossy non_air_projection={totals['non_air_projection_diffs']})"
    )
    if nb is not None:
        lines.append(
            f"  consistency vs native B: consistent(A==B!=port)={totals['consistent_diffs']}  "
            f"native_unstable(A!=B)={totals['unstable_diffs']}"
        )
        for (va, vb, vp), cnt in res["unstable_patterns"].most_common(6):
            lines.append(f"    unstable {cnt:5d}: A={va} B={vb} port={vp}")

    if res["heightmaps"]:
        lines.append(f"heightmaps: {res['heightmaps']}")

    if res["overall_pairs"]:
        lines.append("top state pairs (native_A -> port):")
        for (va, vp), cnt in res["overall_pairs"].most_common(12):
            y0, y1 = res["overall_pair_yrange"][(va, vp)]
            lines.append(f"  {cnt:6d}  y={y0}..{y1}  {va} -> {vp}")

    if res["buckets"]:
        lines.append("causal attribution:")
        ordered = sorted(res["buckets"].items(), key=lambda kv: (-kv[1]["count"], kv[0]))
        for key, b in ordered:
            mn, mx = b["min_xyz"], b["max_xyz"]
            bstr = ",".join(f"{k}:{v}" for k, v in b["biomes"].most_common(3))
            lines.append(
                f"  {b['count']:6d}  {key}  bbox=[{mn[0]}..{mx[0]}, {mn[1]}..{mx[1]}, {mn[2]}..{mx[2]}]  biomes=({bstr})"
            )
            for (va, vp), cnt in b["pairs"].most_common(8):
                samples = " ".join(f"({x},{y},{z})" for x, y, z in b["samples"][(va, vp)])
                sample_txt = f"  samples: {samples}" if samples else ""
                lines.append(f"    {cnt:5d}  {va} -> {vp}{sample_txt}")

    return "\n".join(lines) + "\n"


def format_grid_report(gres, args):
    if not gres.get("is_grid"):
        return format_report(gres, args)
    ox, oz = gres["origin"]
    n = gres["n"]
    totals = gres["totals"]
    pm = gres["port_meta"]
    lines = []
    lines.append(
        f"== chunk_triage grid ({ox},{oz})+{n}x{n} compared={len(gres['chunks'])}/{n*n} "
        f"min_y={gres['min_y']} height={gres['height']} =="
    )
    lines.append(f"native A: {gres['world']}")
    if gres["world_b"]:
        lines.append(f"native B: {gres['world_b']}")
    lines.append(f"port ({pm['kind']}): {pm['path']} sha256={pm['sha256'][:16]}...")
    if gres["skipped"]:
        sdesc = ", ".join(f"({s['chunk'][0]},{s['chunk'][1]}:{s['side']})" for s in gres["skipped"])
        lines.append(f"skipped incomplete chunks ({len(gres['skipped'])}): {sdesc}")

    lines.append("per-chunk scoreboard:")
    for r in gres["chunks"]:
        na = r["native_a"]
        t = r["totals"]
        top_bucket = "-"
        if r["buckets"]:
            bk, bobj = min(r["buckets"].items(), key=lambda kv: (-kv[1]["count"], kv[0]))
            top_bucket = f"{bk} ({bobj['count']})"
        nb_part = (
            f"  consistent={t['consistent_diffs']:5d}  unstable={t['unstable_diffs']:4d}"
            if gres["world_b"]
            else ""
        )
        hm_part = (
            f"  hm_diff={r['heightmaps']['port_vs_native_saved_diff_cols']:3d}"
            if gres["heightmap_diff_cols"] is not None
            else ""
        )
        lines.append(
            f"  ({na['cx']:4d},{na['cz']:4d})  diffs={t['all_air_diffs']:6d}  "
            f"non_air={t['non_air_projection_diffs']:6d}{nb_part}{hm_part}  top={top_bucket}"
        )

    lines.append(
        f"grid diff summary: all_air_kinds={totals['all_air_diffs']} / {totals['total_cells']}  "
        f"(lossy non_air_projection={totals['non_air_projection_diffs']})"
    )
    if gres["world_b"]:
        lines.append(
            f"  consistency vs native B: consistent={totals['consistent_diffs']}  "
            f"native_unstable={totals['unstable_diffs']}  "
            f"(A vs B: all_air={totals['native_ab_all_diffs']}, non_air={totals['native_ab_non_air_diffs']})"
        )
    if gres["heightmap_diff_cols"] is not None:
        lines.append(f"heightmap diff columns ({args.hm_name}): {gres['heightmap_diff_cols']}")

    if gres["overall_pairs"]:
        lines.append("top state pairs across grid (native_A -> port):")
        for (va, vp), cnt in gres["overall_pairs"].most_common(12):
            y0, y1 = gres["overall_pair_yrange"][(va, vp)]
            lines.append(f"  {cnt:6d}  y={y0}..{y1}  {va} -> {vp}")

    if gres["buckets"]:
        lines.append("aggregated causal attribution:")
        ordered = sorted(gres["buckets"].items(), key=lambda kv: (-kv[1]["count"], kv[0]))
        for key, b in ordered:
            mn, mx = b["min_xyz"], b["max_xyz"]
            bstr = ",".join(f"{k}:{v}" for k, v in b["biomes"].most_common(3))
            lines.append(
                f"  {b['count']:6d}  {key}  bbox=[{mn[0]}..{mx[0]}, {mn[1]}..{mx[1]}, {mn[2]}..{mx[2]}]  biomes=({bstr})"
            )
            for (va, vp), cnt in b["pairs"].most_common(8):
                samples = " ".join(f"({x},{y},{z})" for x, y, z in b["samples"][(va, vp)])
                sample_txt = f"  samples: {samples}" if samples else ""
                lines.append(f"    {cnt:5d}  {va} -> {vp}{sample_txt}")

    if getattr(args, "per_chunk", False):
        for r in gres["chunks"]:
            if r["totals"]["all_air_diffs"] > 0 or not gres["buckets"]:
                lines.append("")
                lines.append(format_report(r, args).rstrip("\n"))

    return "\n".join(lines) + "\n"


def json_summary(res):
    if res.get("is_grid"):
        return {
            "grid": {"origin": res["origin"], "n": res["n"]},
            "min_y": res["min_y"],
            "height": res["height"],
            "world": res["world"],
            "world_b": res["world_b"],
            "port_meta": res["port_meta"],
            "compared_chunks": len(res["chunks"]),
            "skipped": res["skipped"],
            "totals": res["totals"],
            "heightmap_diff_cols": res["heightmap_diff_cols"],
            "top_pairs": [
                {
                    "native": va,
                    "port": vp,
                    "count": cnt,
                    "y_range": res["overall_pair_yrange"][(va, vp)],
                }
                for (va, vp), cnt in res["overall_pairs"].most_common()
            ],
            "attribution": [
                {
                    "bucket": key,
                    "count": b["count"],
                    "bbox": b["min_xyz"] + b["max_xyz"],
                    "biomes": dict(b["biomes"]),
                    "pairs": [
                        {"native": va, "port": vp, "count": cnt, "samples": b["samples"][(va, vp)]}
                        for (va, vp), cnt in b["pairs"].most_common()
                    ],
                }
                for key, b in sorted(res["buckets"].items(), key=lambda kv: (-kv[1]["count"], kv[0]))
            ],
            "chunks": [json_summary(c) for c in res["chunks"]],
        }
    na = res["native_a"]
    return {
        "chunk": [na["cx"], na["cz"]],
        "min_y": na["min_y"],
        "height": na["height"],
        "native_a_meta": na["meta"],
        "port_meta": {
            "kind": res["port"]["kind"],
            "path": res["port"]["path"],
            "sha256": res["port"]["sha256"],
            "non_air": res["port"]["non_air"],
            "cave_air": res["port"]["cave_air"],
            "void_air": res["port"]["void_air"],
        },
        "totals": res["totals"],
        "structures": {
            "references": {k: [list(c) for c in v] for k, v in res["structures"]["references"].items()},
            "pieces": [
                {
                    "structure": p["structure"],
                    "start_chunk": list(p["start_chunk"]),
                    "child_index": p["child_index"],
                    "child_id": p["child_id"],
                    "BB": list(p["BB"]),
                    "intersects_chunk": p["intersects_chunk"],
                }
                for p in res["structures"]["pieces"]
            ],
            "unavailable_starts": res["structures"]["unavailable_starts"],
        },
        "heightmaps": res["heightmaps"],
        "attribution": [
            {
                "bucket": key,
                "count": b["count"],
                "bbox": b["min_xyz"] + b["max_xyz"],
                "biomes": dict(b["biomes"]),
                "pairs": [
                    {"native": va, "port": vp, "count": cnt, "samples": b["samples"][(va, vp)]}
                    for (va, vp), cnt in b["pairs"].most_common()
                ],
            }
            for key, b in sorted(res["buckets"].items(), key=lambda kv: (-kv[1]["count"], kv[0]))
        ],
    }


def build_parser():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("world", help="Native Anvil world directory")
    ap.add_argument("cx", type=int, help="Target chunk X coordinate (or grid origin X with --n)")
    ap.add_argument("cz", type=int, help="Target chunk Z coordinate (or grid origin Z with --n)")
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--port-tsv", help="Port canonical-state TSV (from MC_SPINE_WG_DUMP_CANON or terrain_chunk_dump.py)")
    src.add_argument("--port-bin", help="Port binary dump (from MC_SPINE_WG_DUMP)")
    ap.add_argument("--n", type=int, default=1, help="Grid width N from (cx,cz) (default: 1)")
    ap.add_argument("--world-b", help="Optional second native world run to isolate native run-to-run noise")
    ap.add_argument("--bin-ox", type=int, help="Binary/HM dump origin chunk X (default: cx)")
    ap.add_argument("--bin-oz", type=int, help="Binary/HM dump origin chunk Z (default: cz)")
    ap.add_argument("--bin-n", type=int, default=None, help="Binary/HM dump grid width N (default: --n)")
    ap.add_argument("--min-y", type=int, help="Override min_y (default: from native yPos)")
    ap.add_argument("--height", type=int, help="Override height (default: from native sections)")
    ap.add_argument("--port-hm", help="Optional port OCEAN_FLOOR binary dump (MC_SPINE_WG_DUMP_HM)")
    ap.add_argument("--hm-name", default="OCEAN_FLOOR", help="Native Heightmaps entry name for --port-hm (default: OCEAN_FLOOR)")
    ap.add_argument("--halo", type=int, default=1, help="Halo radius around structure child BBs (default: 1)")
    ap.add_argument("--samples", type=int, default=5, help="Sample coords per pair/bucket (default: 5)")
    ap.add_argument("--per-chunk", action="store_true", help="In --n > 1 mode, also print per-chunk full reports")
    ap.add_argument("--allow-incomplete", action="store_true", help="In --n > 1 mode, skip missing/non-full chunks and report them")
    ap.add_argument("--fail-on-diff", action="store_true", help="Exit 1 when all_air_diffs > 0")
    ap.add_argument("--json-out", help="Optional path to write JSON triage report")
    return ap


def main(argv=None):
    ap = build_parser()
    args = ap.parse_args(argv)
    try:
        res = triage_grid(args)
    except TriageError as exc:
        print(f"chunk_triage: ERROR: {exc}", file=sys.stderr)
        return 2
    sys.stdout.write(format_grid_report(res, args))
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(json_summary(res), indent=2) + "\n")
    if args.fail_on_diff and res["totals"]["all_air_diffs"] > 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
