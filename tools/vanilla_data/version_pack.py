#!/usr/bin/env python3
"""Build, verify and diff megu VersionPacks from vanilla DataGenerator reports.

  version_pack.py build --reports <datagen_reports_dir> --out <pack_dir>
                        [--version-json <version.json>] [--mc-version <ver>]
                        [--protocol <int>] [--data-version <int>]
                        [--pack-format <int>] [--jar-sha1 <sha1>]
  version_pack.py verify --pack <pack_dir>
  version_pack.py diff --from <pack_a> --to <pack_b> [--json]
  version_pack.py self-test            (or: version_pack.py --self-test)

A VersionPack is the normalized, fail-closed description of one Minecraft Java
release: the packet-id tables per phase/direction and the registry-id tables
the wire and storage layers need. It is the asset `assets/versions/<mc_version>/`
in specs/multi-version-architecture.md is built around. Building it is a pure
function of the vanilla 1.20.5+ server jar's own `--reports` output:

  java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar \
      --reports --output gen        # writes gen/reports/{packets,registries,blocks}.json

Schema (what vanilla writes, what this tool reads):

  reports/packets.json     {phase: {direction: {packet_id: {"protocol_id": int}}}}
  reports/registries.json  {registry: {"protocol_id": int,
                                       "entries": {entry: {"protocol_id": int}}}}
  reports/blocks.json      {block: {..., "states": [{"id": int, "default": bool, ...}]}}

Schema (what this tool writes into <pack_dir>):

  manifest.json       {"format": 1, "generated_by": "tools/vanilla_data/version_pack.py",
                       "mc_version": str, "protocol_version": int,
                       "data_version": int|null, "pack_format": int|{...}|null,
                       "jar_sha1": str|null,
                       "files": {name: {"sha256": hex, "bytes": int}},
                       "counts": {...}}
  packet_ids.json     {"format": 1, "packets": {phase: {direction: {packet_id: int}}}}
  registry_ids.json   {"format": 1, "registries": {registry: {"protocol_id": int,
                                                              "entries": {entry: int}}}}
  block_states.json   {"format": 1, "block_states_total": int,
                       "blocks": {block: {"first_state_id": int, "state_count": int,
                                          "default_state_id": int}}}
                      (written only when reports/blocks.json is present)

Everything is fail-closed: malformed JSON, duplicate object keys, negative or
non-integer ids, duplicate protocol ids within a phase/direction or registry,
missing core registries, non-contiguous block-state ranges, a missing or
tampered pack file, and an unknown manifest field type all stop the run with a
non-zero exit instead of producing a half-valid pack. Output is deterministic
(sorted keys, no timestamps, no absolute paths), so the same reports always
produce byte-identical packs.

Exit status: 0 ok, 1 validation failure, 2 usage error. Pure Python 3 stdlib.
"""
import argparse
import hashlib
import json
import sys
import tempfile
from pathlib import Path

TOOL = "tools/vanilla_data/version_pack.py"
FORMAT = 1

# The registries the wire layer cannot boot without. `minecraft:data_component_type`
# exists only on 1.20.5+ reports - the same minimum this tool is built for.
REQUIRED_REGISTRIES = (
    "minecraft:block",
    "minecraft:item",
    "minecraft:entity_type",
    "minecraft:menu",
    "minecraft:data_component_type",
)

PACKET_IDS_JSON = "packet_ids.json"
REGISTRY_IDS_JSON = "registry_ids.json"
BLOCK_STATES_JSON = "block_states.json"
MANIFEST_JSON = "manifest.json"


class PackError(Exception):
    """A validation or input failure. Raised, never sys.exit, so self-test can catch it."""


# ---- strict JSON ------------------------------------------------------------

def _no_dup_object(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise PackError(f"duplicate key {key!r} in JSON object")
        out[key] = value
    return out


def load_json(path, what):
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as error:
        raise PackError(f"{what}: cannot read {path}: {error}") from error
    try:
        return json.loads(text, object_pairs_hook=_no_dup_object)
    except PackError as error:
        raise PackError(f"{what}: {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise PackError(f"{what}: malformed JSON in {path}: {error}") from error


def dump_bytes(obj):
    """The one deterministic serialization every pack file uses."""
    return (json.dumps(obj, sort_keys=True, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def require_int(value, what, minimum=0):
    if isinstance(value, bool) or not isinstance(value, int):
        raise PackError(f"{what}: expected an integer, got {value!r}")
    if value < minimum:
        raise PackError(f"{what}: {value} < {minimum}")
    return value


def require_str(value, what):
    if not isinstance(value, str) or not value:
        raise PackError(f"{what}: expected a non-empty string, got {value!r}")
    return value


def require_obj(value, what):
    if not isinstance(value, dict):
        raise PackError(f"{what}: expected a JSON object, got {value!r}")
    return value


# ---- reports -> normalized tables -------------------------------------------

def parse_packets_report(report):
    """reports/packets.json -> {phase: {direction: {packet_name: id}}}, fail-closed."""
    packets = {}
    for phase, directions in require_obj(report, "packets.json").items():
        require_str(phase, "packets.json phase")
        direction_table = {}
        for direction, entries in require_obj(directions, f"packets.json[{phase}]").items():
            require_str(direction, f"packets.json[{phase}] direction")
            table = {}
            seen_ids = {}
            for name, record in require_obj(entries, f"packets.json[{phase}][{direction}]").items():
                require_str(name, f"packets.json[{phase}][{direction}] packet name")
                record = require_obj(record, f"packets.json[{phase}][{direction}][{name}]")
                pid = require_int(record.get("protocol_id"),
                                  f"packets.json[{phase}][{direction}][{name}].protocol_id")
                if pid in seen_ids:
                    raise PackError(
                        f"packets.json[{phase}][{direction}]: protocol_id {pid} used by both "
                        f"{seen_ids[pid]!r} and {name!r}")
                seen_ids[pid] = name
                table[name] = pid
            direction_table[direction] = table
        packets[phase] = direction_table
    if not packets:
        raise PackError("packets.json: no phases present")
    return packets


def parse_registries_report(report):
    """reports/registries.json -> {registry: {protocol_id, entries: {name: id}}}."""
    registries = {}
    for registry, record in require_obj(report, "registries.json").items():
        require_str(registry, "registries.json registry name")
        record = require_obj(record, f"registries.json[{registry}]")
        reg_id = require_int(record.get("protocol_id"),
                             f"registries.json[{registry}].protocol_id")
        entries = {}
        seen_ids = {}
        for name, entry in require_obj(record.get("entries"),
                                       f"registries.json[{registry}].entries").items():
            require_str(name, f"registries.json[{registry}] entry name")
            entry = require_obj(entry, f"registries.json[{registry}].entries[{name}]")
            eid = require_int(entry.get("protocol_id"),
                              f"registries.json[{registry}].entries[{name}].protocol_id")
            if eid in seen_ids:
                raise PackError(
                    f"registries.json[{registry}]: protocol_id {eid} used by both "
                    f"{seen_ids[eid]!r} and {name!r}")
            seen_ids[eid] = name
            entries[name] = eid
        registries[registry] = {"protocol_id": reg_id, "entries": entries}
    missing = [r for r in REQUIRED_REGISTRIES if r not in registries]
    if missing:
        raise PackError(
            "registries.json: missing required core registries: " + ", ".join(missing))
    return registries


def parse_blocks_report(report):
    """reports/blocks.json -> {block: {first_state_id, state_count, default_state_id}}.

    Vanilla numbers a block's states as one contiguous ascending run, and the runs
    tile 0..N globally with no overlap; anything else means a wrong or partial
    report, so both halves are checked."""
    blocks = {}
    runs = []
    for name, record in require_obj(report, "blocks.json").items():
        require_str(name, "blocks.json block name")
        record = require_obj(record, f"blocks.json[{name}]")
        states = record.get("states")
        if not isinstance(states, list) or not states:
            raise PackError(f"blocks.json[{name}]: expected a non-empty 'states' list")
        ids = []
        default_id = None
        for i, state in enumerate(states):
            state = require_obj(state, f"blocks.json[{name}].states[{i}]")
            sid = require_int(state.get("id"), f"blocks.json[{name}].states[{i}].id")
            ids.append(sid)
            if state.get("default") is True:
                if default_id is not None:
                    raise PackError(f"blocks.json[{name}]: more than one default state")
                default_id = sid
        for i in range(1, len(ids)):
            if ids[i] != ids[i - 1] + 1:
                raise PackError(
                    f"blocks.json[{name}]: state ids are not contiguous "
                    f"(... {ids[i - 1]}, {ids[i]} ...)")
        if default_id is None:
            raise PackError(f"blocks.json[{name}]: no default state")
        blocks[name] = {
            "first_state_id": ids[0],
            "state_count": len(ids),
            "default_state_id": default_id,
        }
        runs.append((ids[0], ids[-1], name))
    runs.sort()
    expected = 0
    for first, last, name in runs:
        if first != expected:
            raise PackError(
                f"blocks.json: block states are not globally contiguous "
                f"(expected next first_state_id {expected}, {name} starts at {first})")
        expected = last + 1
    return blocks


# ---- pack file bodies ---------------------------------------------------------

def packet_ids_body(packets):
    return {"format": FORMAT, "packets": packets}


def registry_ids_body(registries):
    return {"format": FORMAT, "registries": registries}


def block_states_body(blocks):
    return {
        "format": FORMAT,
        "block_states_total": sum(b["state_count"] for b in blocks.values()),
        "blocks": blocks,
    }


def validate_packet_ids_body(body, what):
    """The same checks build applies, re-run against a written pack file."""
    body = require_obj(body, what)
    if body.get("format") != FORMAT:
        raise PackError(f"{what}: unsupported format {body.get('format')!r}, expected {FORMAT}")
    packets = {}
    for phase, directions in require_obj(body.get("packets"), f"{what}.packets").items():
        direction_table = {}
        for direction, entries in require_obj(directions, f"{what}.packets[{phase}]").items():
            table = {}
            seen = {}
            for name, pid in require_obj(entries, f"{what}.packets[{phase}][{direction}]").items():
                require_str(name, f"{what}.packets[{phase}][{direction}] packet name")
                require_int(pid, f"{what}.packets[{phase}][{direction}][{name}]")
                if pid in seen:
                    raise PackError(
                        f"{what}.packets[{phase}][{direction}]: duplicate protocol_id "
                        f"{pid} ({seen[pid]!r}, {name!r})")
                seen[pid] = name
                table[name] = pid
            direction_table[direction] = table
        packets[phase] = direction_table
    if not packets:
        raise PackError(f"{what}.packets: empty")
    return packets


def validate_registry_ids_body(body, what, require_core=True):
    body = require_obj(body, what)
    if body.get("format") != FORMAT:
        raise PackError(f"{what}: unsupported format {body.get('format')!r}, expected {FORMAT}")
    registries = {}
    for registry, record in require_obj(body.get("registries"), f"{what}.registries").items():
        require_str(registry, f"{what}.registries registry name")
        record = require_obj(record, f"{what}.registries[{registry}]")
        reg_id = require_int(record.get("protocol_id"),
                             f"{what}.registries[{registry}].protocol_id")
        entries = {}
        seen = {}
        for name, eid in require_obj(record.get("entries"),
                                     f"{what}.registries[{registry}].entries").items():
            require_str(name, f"{what}.registries[{registry}] entry name")
            require_int(eid, f"{what}.registries[{registry}].entries[{name}]")
            if eid in seen:
                raise PackError(
                    f"{what}.registries[{registry}]: duplicate protocol_id {eid} "
                    f"({seen[eid]!r}, {name!r})")
            seen[eid] = name
            entries[name] = eid
        registries[registry] = {"protocol_id": reg_id, "entries": entries}
    if require_core:
        missing = [r for r in REQUIRED_REGISTRIES if r not in registries]
        if missing:
            raise PackError(
                f"{what}.registries: missing required core registries: "
                + ", ".join(missing))
    return registries


def validate_block_states_body(body, what):
    body = require_obj(body, what)
    if body.get("format") != FORMAT:
        raise PackError(f"{what}: unsupported format {body.get('format')!r}, expected {FORMAT}")
    blocks = require_obj(body.get("blocks"), f"{what}.blocks")
    total = 0
    runs = []
    for name, record in blocks.items():
        require_str(name, f"{what}.blocks block name")
        record = require_obj(record, f"{what}.blocks[{name}]")
        first = require_int(record.get("first_state_id"), f"{what}.blocks[{name}].first_state_id")
        count = require_int(record.get("state_count"), f"{what}.blocks[{name}].state_count", 1)
        default = require_int(record.get("default_state_id"),
                              f"{what}.blocks[{name}].default_state_id")
        if not first <= default < first + count:
            raise PackError(f"{what}.blocks[{name}]: default_state_id outside its state range")
        runs.append((first, first + count - 1, name))
        total += count
    runs.sort()
    expected = 0
    for first, last, name in runs:
        if first != expected:
            raise PackError(
                f"{what}.blocks: state ranges not globally contiguous "
                f"(expected {expected}, {name} starts at {first})")
        expected = last + 1
    declared = body.get("block_states_total")
    require_int(declared, f"{what}.block_states_total")
    if declared != total:
        raise PackError(f"{what}: block_states_total {declared} != counted {total}")
    return blocks


# ---- manifest -----------------------------------------------------------------

def _version_json_meta(path):
    raw = load_json(path, "version.json")
    require_obj(raw, "version.json")
    meta = {}
    mc = raw.get("id") or raw.get("name")
    if mc is not None:
        meta["mc_version"] = require_str(mc, "version.json id")
    for field, key in (("protocol_version", "protocol_version"),
                       ("world_version", "data_version")):
        if raw.get(field) is not None:
            meta[key] = require_int(raw[field], f"version.json {field}")
    pack_version = raw.get("pack_version")
    if pack_version is not None:
        meta["pack_format"] = _pack_version_to_meta(pack_version)
    return meta


def _is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def _pack_version_to_meta(pack_version):
    """Normalize a version.json `pack_version` to int or a dict of ints.

    Spelling forms seen in the wild:
      legacy int          pack_version: 15
      flat ints           {"data": 48, "resource": 34}
      flat major/minor    {"data_major": 121, "data_minor": 0, ...}  (26.x)
      tuple entries       {"data": [121, 0], "resource": [97, 1]}
      bare data tuple     [121, 0]
    Tuple entries expand to `<key>_major`/`<key>_minor` so every form lands
    on the same flat-int manifest shape.
    """
    if _is_int(pack_version):
        return pack_version
    if isinstance(pack_version, dict):
        out = {}
        for key, value in sorted(pack_version.items()):
            require_str(key, "version.json pack_version key")
            if _is_int(value):
                out[key] = value
            elif (isinstance(value, (list, tuple)) and value
                    and all(_is_int(v) for v in value)):
                out[f"{key}_major"] = value[0]
                out[f"{key}_minor"] = value[1] if len(value) > 1 else 0
            else:
                raise PackError(
                    f"version.json pack_version.{key}: "
                    f"unexpected value {value!r}")
        return out
    if (isinstance(pack_version, (list, tuple)) and pack_version
            and all(_is_int(v) for v in pack_version)):
        return {"data_major": pack_version[0],
                "data_minor": pack_version[1] if len(pack_version) > 1 else 0}
    raise PackError(
        f"version.json pack_version: unexpected value {pack_version!r}")


def _pick(field, flag_value, json_value):
    if flag_value is not None and json_value is not None and flag_value != json_value:
        raise PackError(
            f"--{field} {flag_value!r} conflicts with version.json value {json_value!r}")
    return flag_value if flag_value is not None else json_value


def build_manifest(meta, file_digests, counts):
    return {
        "format": FORMAT,
        "generated_by": TOOL,
        "mc_version": meta["mc_version"],
        "protocol_version": meta["protocol_version"],
        "data_version": meta.get("data_version"),
        "pack_format": meta.get("pack_format"),
        "jar_sha1": meta.get("jar_sha1"),
        "files": file_digests,
        "counts": counts,
    }


def validate_manifest(manifest, what="manifest.json"):
    require_obj(manifest, what)
    if manifest.get("format") != FORMAT:
        raise PackError(f"{what}: unsupported format {manifest.get('format')!r}, expected {FORMAT}")
    if manifest.get("generated_by") != TOOL:
        raise PackError(f"{what}: generated_by {manifest.get('generated_by')!r}, expected {TOOL!r}")
    require_str(manifest.get("mc_version"), f"{what}.mc_version")
    require_int(manifest.get("protocol_version"), f"{what}.protocol_version")
    data_version = manifest.get("data_version")
    if data_version is not None:
        require_int(data_version, f"{what}.data_version")
    pack_format = manifest.get("pack_format")
    if pack_format is not None:
        if isinstance(pack_format, bool):
            raise PackError(f"{what}.pack_format: expected int or object")
        if isinstance(pack_format, dict):
            for key, value in pack_format.items():
                require_int(value, f"{what}.pack_format.{key}")
        elif not isinstance(pack_format, int):
            raise PackError(f"{what}.pack_format: expected int or object")
    jar_sha1 = manifest.get("jar_sha1")
    if jar_sha1 is not None:
        require_str(jar_sha1, f"{what}.jar_sha1")
        if len(jar_sha1) != 40 or any(c not in "0123456789abcdef" for c in jar_sha1):
            raise PackError(f"{what}.jar_sha1: expected a 40-hex-digit sha1")
    files = require_obj(manifest.get("files"), f"{what}.files")
    if not files:
        raise PackError(f"{what}.files: empty")
    for name in (PACKET_IDS_JSON, REGISTRY_IDS_JSON):
        if name not in files:
            raise PackError(f"{what}.files: required {name} not listed")
    for name, record in files.items():
        require_str(name, f"{what}.files name")
        record = require_obj(record, f"{what}.files[{name}]")
        digest = require_str(record.get("sha256"), f"{what}.files[{name}].sha256")
        if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
            raise PackError(f"{what}.files[{name}].sha256: expected a 64-hex-digit sha256")
        require_int(record.get("bytes"), f"{what}.files[{name}].bytes")
    require_obj(manifest.get("counts"), f"{what}.counts")
    return files


# ---- subcommands ---------------------------------------------------------------

def cmd_build(args):
    reports = Path(args.reports)
    meta = {}
    if args.version_json is not None:
        meta = _version_json_meta(args.version_json)
    mc_version = _pick("mc-version", args.mc_version, meta.get("mc_version"))
    protocol = _pick("protocol", args.protocol, meta.get("protocol_version"))
    if mc_version is None:
        raise PackError("mc_version unknown: pass --mc-version or --version-json")
    if protocol is None:
        raise PackError("protocol_version unknown: pass --protocol or --version-json")
    pack_meta = {
        "mc_version": require_str(mc_version, "mc_version"),
        "protocol_version": require_int(protocol, "protocol_version"),
        "data_version": _pick("data-version", args.data_version, meta.get("data_version")),
        "pack_format": _pick("pack-format", args.pack_format, meta.get("pack_format")),
        "jar_sha1": args.jar_sha1,
    }
    if pack_meta["data_version"] is not None:
        require_int(pack_meta["data_version"], "data_version")
    pack_format = pack_meta["pack_format"]
    if isinstance(pack_format, dict):
        for key, value in pack_format.items():
            require_int(value, f"pack_format.{key}")
    elif pack_format is not None:
        require_int(pack_format, "pack_format")
    if pack_meta["jar_sha1"] is not None:
        sha1 = require_str(pack_meta["jar_sha1"], "jar_sha1")
        if len(sha1) != 40 or any(c not in "0123456789abcdef" for c in sha1):
            raise PackError("--jar-sha1: expected a 40-hex-digit sha1")

    packets = parse_packets_report(load_json(reports / "packets.json", "packets.json"))
    registries = parse_registries_report(load_json(reports / "registries.json", "registries.json"))
    blocks_path = reports / "blocks.json"
    blocks = parse_blocks_report(load_json(blocks_path, "blocks.json")) \
        if blocks_path.is_file() else None

    bodies = {
        PACKET_IDS_JSON: packet_ids_body(packets),
        REGISTRY_IDS_JSON: registry_ids_body(registries),
    }
    if blocks is not None:
        bodies[BLOCK_STATES_JSON] = block_states_body(blocks)

    out = Path(args.out)
    if out.exists() and any(out.iterdir()):
        raise PackError(f"--out {out}: directory exists and is not empty")
    out.mkdir(parents=True, exist_ok=True)

    file_digests = {}
    for name, body in bodies.items():
        data = dump_bytes(body)
        (out / name).write_bytes(data)
        file_digests[name] = {"sha256": sha256_hex(data), "bytes": len(data)}

    packet_counts = {
        f"{phase}/{direction}": len(table)
        for phase, directions in packets.items()
        for direction, table in directions.items()
    }
    counts = {
        "packets_total": sum(packet_counts.values()),
        "packets": packet_counts,
        "registries": len(registries),
        "registry_entries": sum(len(r["entries"]) for r in registries.values()),
        "block_states": bodies.get(BLOCK_STATES_JSON, {}).get("block_states_total"),
    }
    manifest = build_manifest(pack_meta, file_digests, counts)
    (out / MANIFEST_JSON).write_bytes(dump_bytes(manifest))

    print(f"built {out} ({pack_meta['mc_version']} protocol {pack_meta['protocol_version']}): "
          f"{counts['packets_total']} packets, {counts['registries']} registries "
          f"({counts['registry_entries']} entries)"
          + (f", {counts['block_states']} block states" if blocks is not None else ""))
    return 0


def verify_pack(pack_dir):
    """Fail-closed pack validation. Returns (manifest, bodies) on success."""
    pack = Path(pack_dir)
    if not pack.is_dir():
        raise PackError(f"{pack}: not a directory")
    manifest = load_json(pack / MANIFEST_JSON, MANIFEST_JSON)
    file_records = validate_manifest(manifest)
    bodies = {}
    for name, record in file_records.items():
        path = pack / name
        try:
            data = path.read_bytes()
        except OSError as error:
            raise PackError(f"{name}: cannot read: {error}") from error
        if len(data) != record["bytes"]:
            raise PackError(
                f"{name}: {len(data)} bytes, manifest records {record['bytes']}")
        got = sha256_hex(data)
        if got != record["sha256"]:
            raise PackError(
                f"{name}: sha256 mismatch (manifest {record['sha256'][:16]}..., "
                f"actual {got[:16]}...)")
        bodies[name] = load_json(path, name)
    packets = validate_packet_ids_body(bodies[PACKET_IDS_JSON], PACKET_IDS_JSON)
    registries = validate_registry_ids_body(bodies[REGISTRY_IDS_JSON], REGISTRY_IDS_JSON)
    blocks = None
    if BLOCK_STATES_JSON in bodies:
        blocks = validate_block_states_body(bodies[BLOCK_STATES_JSON], BLOCK_STATES_JSON)
    # Any other listed file only needed its integrity check above.
    return manifest, packets, registries, blocks


def cmd_verify(args):
    manifest, packets, registries, blocks = verify_pack(args.pack)
    print(f"OK {args.pack} ({manifest['mc_version']} protocol "
          f"{manifest['protocol_version']}): "
          f"{sum(len(t) for d in packets.values() for t in d.values())} packets, "
          f"{len(registries)} registries"
          + (f", {len(blocks)} blocks" if blocks is not None else ""))
    return 0


# ---- diff ----------------------------------------------------------------------

def _table_delta(before, after):
    """added/removed/renumbered/unchanged_count between two {name: id} tables."""
    added = {k: v for k, v in after.items() if k not in before}
    removed = {k: v for k, v in before.items() if k not in after}
    renumbered = {
        k: {"from_id": before[k], "to_id": after[k]}
        for k in before.keys() & after.keys()
        if before[k] != after[k]
    }
    unchanged = sum(1 for k in before.keys() & after.keys() if before[k] == after[k])
    return {
        "added": added,
        "removed": removed,
        "renumbered": renumbered,
        "unchanged_count": unchanged,
    }


def _remap(delta):
    """forward: from_id -> to_id for renumbered entries; reverse: to_id -> from_id."""
    forward = {str(v["from_id"]): v["to_id"] for v in delta["renumbered"].values()}
    reverse = {str(v["to_id"]): v["from_id"] for v in delta["renumbered"].values()}
    return {
        "forward": dict(sorted(forward.items(), key=lambda kv: int(kv[0]))),
        "reverse": dict(sorted(reverse.items(), key=lambda kv: int(kv[0]))),
        "added_ids": sorted(delta["added"].values()),
        "removed_ids": sorted(delta["removed"].values()),
    }


def diff_packs(pack_a, pack_b):
    manifest_a, packets_a, registries_a, blocks_a = verify_pack(pack_a)
    manifest_b, packets_b, registries_b, blocks_b = verify_pack(pack_b)

    delta = {
        "format": FORMAT,
        "generated_by": TOOL,
        "from": {"mc_version": manifest_a["mc_version"],
                 "protocol_version": manifest_a["protocol_version"]},
        "to": {"mc_version": manifest_b["mc_version"],
               "protocol_version": manifest_b["protocol_version"]},
        "packets": {},
        "registries": {},
        "id_remap": {"packets": {}, "registries": {}},
    }

    groups = set()
    for phase, directions in packets_a.items():
        groups |= {(phase, d) for d in directions}
    for phase, directions in packets_b.items():
        groups |= {(phase, d) for d in directions}
    for phase, direction in sorted(groups):
        before = packets_a.get(phase, {}).get(direction, {})
        after = packets_b.get(phase, {}).get(direction, {})
        d = _table_delta(before, after)
        key = f"{phase}/{direction}"
        delta["packets"][key] = d
        delta["id_remap"]["packets"][key] = _remap(d)

    reg_names = sorted(set(registries_a) | set(registries_b))
    for reg in reg_names:
        before = registries_a.get(reg)
        after = registries_b.get(reg)
        entry_delta = _table_delta(
            before["entries"] if before else {},
            after["entries"] if after else {})
        record = {
            "added_registry": before is None,
            "removed_registry": after is None,
            **entry_delta,
        }
        if before and after and before["protocol_id"] != after["protocol_id"]:
            record["registry_renumbered"] = {
                "from_id": before["protocol_id"],
                "to_id": after["protocol_id"],
            }
        delta["registries"][reg] = record
        delta["id_remap"]["registries"][reg] = _remap(entry_delta)

    if blocks_a is not None and blocks_b is not None:
        state_delta = _table_delta(
            {k: v["first_state_id"] for k, v in blocks_a.items()},
            {k: v["first_state_id"] for k, v in blocks_b.items()})
        resized = {
            k: {"from_count": blocks_a[k]["state_count"],
                "to_count": blocks_b[k]["state_count"]}
            for k in blocks_a.keys() & blocks_b.keys()
            if blocks_a[k]["state_count"] != blocks_b[k]["state_count"]
        }
        delta["block_states"] = {**state_delta, "resized": resized}
        delta["id_remap"]["block_states"] = _remap(state_delta)

    return delta


def _print_delta(delta):
    f, t = delta["from"], delta["to"]
    print(f"{f['mc_version']} (protocol {f['protocol_version']}) -> "
          f"{t['mc_version']} (protocol {t['protocol_version']})")
    for group, d in delta["packets"].items():
        print(f"  packets {group}: +{len(d['added'])} -{len(d['removed'])} "
              f"~{len(d['renumbered'])} (={d['unchanged_count']} unchanged)")
        for name, pid in sorted(d["added"].items()):
            print(f"    + {name} -> {pid}")
        for name, pid in sorted(d["removed"].items()):
            print(f"    - {name} (was {pid})")
        for name, ids in sorted(d["renumbered"].items()):
            print(f"    ~ {name}: {ids['from_id']} -> {ids['to_id']}")
    for reg, d in delta["registries"].items():
        if d["added_registry"]:
            note = " (new registry)"
        elif d["removed_registry"]:
            note = " (removed registry)"
        elif "registry_renumbered" in d:
            rr = d["registry_renumbered"]
            note = f" (registry id {rr['from_id']} -> {rr['to_id']})"
        else:
            note = ""
        print(f"  registry {reg}{note}: +{len(d['added'])} -{len(d['removed'])} "
              f"~{len(d['renumbered'])} (={d['unchanged_count']} unchanged)")
        for name, pid in sorted(d["added"].items()):
            print(f"    + {name} -> {pid}")
        for name, pid in sorted(d["removed"].items()):
            print(f"    - {name} (was {pid})")
        for name, ids in sorted(d["renumbered"].items()):
            print(f"    ~ {name}: {ids['from_id']} -> {ids['to_id']}")
    if "block_states" in delta:
        d = delta["block_states"]
        print(f"  block_states: +{len(d['added'])} -{len(d['removed'])} "
              f"~{len(d['renumbered'])} (={d['unchanged_count']} unchanged, "
              f"{len(d['resized'])} resized)")
        for name, ids in sorted(d["renumbered"].items()):
            print(f"    ~ {name}: first_state_id {ids['from_id']} -> {ids['to_id']}")


def cmd_diff(args):
    delta = diff_packs(args.from_dir, args.to_dir)
    if args.json:
        sys.stdout.write(dump_bytes(delta).decode("utf-8"))
    else:
        _print_delta(delta)
    return 0


# ---- self-test -----------------------------------------------------------------

def _fixture(version):
    """A synthetic but schema-faithful DataGenerator reports dir.

    v767 models 1.21.1, v769 models 1.21.4, v777 models latest vanilla 26.3:
    between them one play packet is added (inserting an id), one removed, one
    renumbered; the data-component, entity-type and menu registries
    gain/lose/renumber entries; one registry's own protocol id moves; one
    whole registry appears; block-state ids shift."""
    reports = {
        "packets.json": {
            "handshake": {"serverbound": {"minecraft:intention": {"protocol_id": 0}}},
            "status": {
                "clientbound": {
                    "minecraft:status_response": {"protocol_id": 0},
                    "minecraft:pong_response": {"protocol_id": 1},
                },
                "serverbound": {
                    "minecraft:status_request": {"protocol_id": 0},
                    "minecraft:ping_request": {"protocol_id": 1},
                },
            },
            "login": {
                "clientbound": {"minecraft:login_finished": {"protocol_id": 2}},
                "serverbound": {"minecraft:hello": {"protocol_id": 0}},
            },
            "configuration": {
                "clientbound": {
                    "minecraft:registry_data": {"protocol_id": 7},
                    "minecraft:finish_configuration": {"protocol_id": 3},
                },
                "serverbound": {"minecraft:finish_configuration": {"protocol_id": 3}},
            },
            "play": {
                "clientbound": {
                    "minecraft:bundle_delimiter": {"protocol_id": 0},
                    "minecraft:add_entity": {"protocol_id": 1},
                    "minecraft:player_chat": {"protocol_id": 2},
                    "minecraft:keep_alive": {"protocol_id": 38},
                    "minecraft:level_chunk_with_light": {"protocol_id": 39},
                    "minecraft:legacy_removed_packet": {"protocol_id": 99},
                },
                "serverbound": {
                    "minecraft:accept_teleportation": {"protocol_id": 0},
                    "minecraft:chat": {"protocol_id": 8},
                    "minecraft:interact": {"protocol_id": 16},
                },
            },
        },
        "registries.json": {
            "minecraft:block": {
                "protocol_id": 0,
                "entries": {
                    "minecraft:air": {"protocol_id": 0},
                    "minecraft:stone": {"protocol_id": 1},
                    "minecraft:dirt": {"protocol_id": 2},
                    "minecraft:oak_log": {"protocol_id": 3},
                },
            },
            "minecraft:item": {
                "protocol_id": 1,
                "entries": {
                    "minecraft:air": {"protocol_id": 0},
                    "minecraft:stick": {"protocol_id": 1},
                    "minecraft:apple": {"protocol_id": 2},
                },
            },
            "minecraft:entity_type": {
                "protocol_id": 7,
                "entries": {
                    "minecraft:pig": {"protocol_id": 100},
                    "minecraft:zombie": {"protocol_id": 124},
                    "minecraft:player": {"protocol_id": 128},
                },
            },
            "minecraft:menu": {
                "protocol_id": 12,
                "entries": {
                    "minecraft:generic_9x3": {"protocol_id": 6},
                    "minecraft:anvil": {"protocol_id": 8},
                },
            },
            "minecraft:data_component_type": {
                "protocol_id": 40,
                "entries": {
                    "minecraft:food": {"protocol_id": 0},
                    "minecraft:custom_name": {"protocol_id": 1},
                    "minecraft:enchantments": {"protocol_id": 2},
                    "minecraft:legacy_thing": {"protocol_id": 3},
                },
            },
            "minecraft:biome": {
                "protocol_id": 60,
                "entries": {
                    "minecraft:plains": {"protocol_id": 0},
                    "minecraft:forest": {"protocol_id": 1},
                },
            },
        },
        "blocks.json": {
            "minecraft:air": {"states": [{"id": 0, "default": True}]},
            "minecraft:stone": {"states": [{"id": 1, "default": True},
                                           {"id": 2, "properties": {"variant": "b"}}]},
            "minecraft:dirt": {"states": [{"id": 3, "default": True}]},
        },
    }
    if version == 767:
        return reports
    if version == 777:
        # ---- 26.3 (protocol 777) delta, the latest-vanilla target ---------
        play_c2s = reports["packets.json"]["play"]["clientbound"]
        del play_c2s["minecraft:legacy_removed_packet"]
        play_c2s["minecraft:player_chat"]["protocol_id"] = 3       # shifted by insert
        play_c2s["minecraft:new_inserted_packet"] = {"protocol_id": 2}
        play_c2s["minecraft:minecart_along_track"] = {"protocol_id": 40}
        play_c2s["minecraft:chunk_batch_finished_v2"] = {"protocol_id": 41}
        reports["packets.json"]["configuration"]["clientbound"][
            "minecraft:select_known_packs"] = {"protocol_id": 14}
        reports["packets.json"]["configuration"]["clientbound"][
            "minecraft:transfer_v2"] = {"protocol_id": 15}

        comp = reports["registries.json"]["minecraft:data_component_type"]["entries"]
        del comp["minecraft:legacy_thing"]
        comp["minecraft:equippable"] = {"protocol_id": 1}          # inserted at 1
        comp["minecraft:custom_name"]["protocol_id"] = 2           # renumbered
        comp["minecraft:enchantments"]["protocol_id"] = 3          # renumbered
        comp["minecraft:tooltip_style"] = {"protocol_id": 4}       # appended
        comp["minecraft:kinetic_weapon"] = {"protocol_id": 5}      # 26.x addition

        et = reports["registries.json"]["minecraft:entity_type"]["entries"]
        et["minecraft:creaking"] = {"protocol_id": 129}
        et["minecraft:copper_golem"] = {"protocol_id": 130}

        menu = reports["registries.json"]["minecraft:menu"]
        menu["protocol_id"] = 13                                   # registry renumbered
        menu["entries"]["minecraft:crafter_3x3"] = {"protocol_id": 9}

        reports["registries.json"]["minecraft:variant_registry"] = {
            "protocol_id": 99,
            "entries": {"minecraft:variant_a": {"protocol_id": 0}},
        }
        reports["registries.json"]["minecraft:timeline"] = {
            "protocol_id": 100,
            "entries": {"minecraft:default": {"protocol_id": 0}},
        }

        reports["blocks.json"] = {
            "minecraft:air": {"states": [{"id": 0, "default": True}]},
            "minecraft:new_block": {"states": [{"id": 1, "default": True}]},
            "minecraft:stone": {"states": [{"id": 2, "default": True},
                                           {"id": 3, "properties": {"variant": "b"}}]},
            "minecraft:dirt": {"states": [{"id": 4, "default": True},
                                          {"id": 5, "properties": {"moist": "7"}}]},
            "minecraft:copper_bars": {"states": [{"id": 6, "default": True}]},
        }
        return reports
    if version != 769:
        return reports
    # ---- 1.21.4 (protocol 769) delta -----------------------------------------
    play_c2s = reports["packets.json"]["play"]["clientbound"]
    del play_c2s["minecraft:legacy_removed_packet"]
    play_c2s["minecraft:player_chat"]["protocol_id"] = 3          # shifted by insert
    play_c2s["minecraft:new_inserted_packet"] = {"protocol_id": 2}
    play_c2s["minecraft:minecart_along_track"] = {"protocol_id": 40}
    reports["packets.json"]["configuration"]["clientbound"][
        "minecraft:select_known_packs"] = {"protocol_id": 14}

    comp = reports["registries.json"]["minecraft:data_component_type"]["entries"]
    del comp["minecraft:legacy_thing"]
    comp["minecraft:equippable"] = {"protocol_id": 1}              # inserted at 1
    comp["minecraft:custom_name"]["protocol_id"] = 2               # renumbered
    comp["minecraft:enchantments"]["protocol_id"] = 3              # renumbered
    comp["minecraft:tooltip_style"] = {"protocol_id": 4}           # appended

    et = reports["registries.json"]["minecraft:entity_type"]["entries"]
    et["minecraft:creaking"] = {"protocol_id": 129}

    menu = reports["registries.json"]["minecraft:menu"]
    menu["protocol_id"] = 13                                       # registry renumbered
    menu["entries"]["minecraft:crafter_3x3"] = {"protocol_id": 9}

    reports["registries.json"]["minecraft:variant_registry"] = {
        "protocol_id": 99,
        "entries": {"minecraft:variant_a": {"protocol_id": 0}},
    }

    reports["blocks.json"] = {
        "minecraft:air": {"states": [{"id": 0, "default": True}]},
        "minecraft:new_block": {"states": [{"id": 1, "default": True}]},
        "minecraft:stone": {"states": [{"id": 2, "default": True},
                                       {"id": 3, "properties": {"variant": "b"}}]},
        "minecraft:dirt": {"states": [{"id": 4, "default": True},
                                      {"id": 5, "properties": {"moist": "7"}}]},
    }
    return reports


# version.json metadata per fixture protocol. 26.3 uses the modern flat
# `pack_version.{data,resource}_{major,minor}` spelling.
_VERSION_JSON_META = {
    767: {"id": "1.21.1", "name": "1.21.1", "world_version": 3955,
          "pack_version": {"resource": 34, "data": 48}},
    769: {"id": "1.21.4", "name": "1.21.4", "world_version": 4189,
          "pack_version": {"resource": 46, "data": 61}},
    777: {"id": "26.3", "name": "26.3", "world_version": 5023,
          "pack_version": {"resource_major": 97, "resource_minor": 1,
                           "data_major": 121, "data_minor": 0}},
}


def _write_reports(root, version):
    reports_dir = Path(root) / "reports"
    reports_dir.mkdir(parents=True)
    for name, obj in _fixture(version).items():
        (reports_dir / name).write_text(
            json.dumps(obj, indent=2, sort_keys=True), encoding="utf-8")
    meta = _VERSION_JSON_META[version]
    version_json = {
        "id": meta["id"],
        "name": meta["name"],
        "protocol_version": version,
        "world_version": meta["world_version"],
        "pack_version": meta["pack_version"],
        "stable": True,
    }
    (Path(root) / "version.json").write_text(json.dumps(version_json), encoding="utf-8")
    return reports_dir


def _ns(**kw):
    return argparse.Namespace(**kw)


def _expect_fail(fn, label, failures):
    try:
        fn()
    except PackError as error:
        return f"PASS {label} (rejected: {error})"
    except SystemExit as error:
        if error.code not in (0, None):
            return f"PASS {label} (exit {error.code})"
        failures.append(f"{label}: exited 0")
        return None
    failures.append(f"{label}: expected failure, succeeded")
    return None


def self_test():
    checks = []
    failures = []

    def check(label, cond):
        if cond:
            checks.append(f"PASS {label}")
        else:
            failures.append(label)
            checks.append(f"FAIL {label}")

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        rep767, rep769 = _write_reports(tmp / "v767", 767), _write_reports(tmp / "v769", 769)
        pack_a, pack_a2, pack_b = tmp / "pack_767", tmp / "pack_767_b", tmp / "pack_769"

        # ---- 1. deterministic, byte-identical build ---------------------------
        for out in (pack_a, pack_a2):
            cmd_build(_ns(reports=str(rep767), out=str(out),
                          version_json=str(tmp / "v767" / "version.json"),
                          mc_version=None, protocol=None, data_version=None,
                          pack_format=None, jar_sha1=None))
        files = (MANIFEST_JSON, PACKET_IDS_JSON, REGISTRY_IDS_JSON, BLOCK_STATES_JSON)
        check("build: all four pack files written",
              all((pack_a / f).is_file() for f in files))
        check("build: byte-identical across runs",
              all((pack_a / f).read_bytes() == (pack_a2 / f).read_bytes() for f in files))
        manifest = load_json(pack_a / MANIFEST_JSON, MANIFEST_JSON)
        check("build: manifest records mc_version/protocol/data_version/pack_format",
              manifest["mc_version"] == "1.21.1"
              and manifest["protocol_version"] == 767
              and manifest["data_version"] == 3955
              and manifest["pack_format"] == {"data": 48, "resource": 34})
        check("build: manifest format + generated_by",
              manifest["format"] == 1 and manifest["generated_by"] == TOOL)
        check("build: per-file sha256 digests recorded",
              all(manifest["files"][f]["sha256"] == sha256_hex((pack_a / f).read_bytes())
                  for f in files[1:]))

        cmd_build(_ns(reports=str(rep769), out=str(pack_b),
                      version_json=str(tmp / "v769" / "version.json"),
                      mc_version=None, protocol=None, data_version=None,
                      pack_format=None, jar_sha1=None))

        # ---- 2. verify passes on a valid pack ---------------------------------
        m, p, r, b = verify_pack(pack_a)
        check("verify: valid pack passes",
              m["protocol_version"] == 767 and p["play"]["clientbound"]["minecraft:add_entity"] == 1)

        # ---- 3. verify fails closed -------------------------------------------
        # 3a. tampered byte, manifest untouched -> sha mismatch
        tampered = tmp / "pack_tampered"
        tampered.mkdir()
        for f in files:
            (tampered / f).write_bytes((pack_a / f).read_bytes())
        raw = bytearray((tampered / PACKET_IDS_JSON).read_bytes())
        raw[len(raw) // 2] ^= 0xFF
        (tampered / PACKET_IDS_JSON).write_bytes(bytes(raw))
        msg = _expect_fail(lambda: verify_pack(tampered),
                           "verify: tampered byte fails (sha256)", failures)
        if msg: checks.append(msg)

        # 3b. missing file
        missing = tmp / "pack_missing"
        missing.mkdir()
        for f in files:
            (missing / f).write_bytes((pack_a / f).read_bytes())
        (missing / REGISTRY_IDS_JSON).unlink()
        msg = _expect_fail(lambda: verify_pack(missing),
                           "verify: missing registry_ids.json fails", failures)
        if msg: checks.append(msg)

        # 3c. manifest missing a required field
        broken = tmp / "pack_nofmt"
        broken.mkdir()
        for f in files:
            (broken / f).write_bytes((pack_a / f).read_bytes())
        bad_manifest = load_json(broken / MANIFEST_JSON, MANIFEST_JSON)
        del bad_manifest["format"]
        (broken / MANIFEST_JSON).write_bytes(dump_bytes(bad_manifest))
        msg = _expect_fail(lambda: verify_pack(broken),
                           "verify: manifest without 'format' fails", failures)
        if msg: checks.append(msg)

        # 3d/3e/3f. content-level defects with *honest* digests: the manifest
        # integrity check passes, so only content re-validation can catch them.
        def forge(mutate_packets=None, mutate_registries=None):
            forged = tmp / f"pack_forge_{len(list(tmp.glob('pack_forge_*')))}"
            forged.mkdir()
            bodies = {
                PACKET_IDS_JSON: load_json(pack_a / PACKET_IDS_JSON, PACKET_IDS_JSON),
                REGISTRY_IDS_JSON: load_json(pack_a / REGISTRY_IDS_JSON, REGISTRY_IDS_JSON),
                BLOCK_STATES_JSON: load_json(pack_a / BLOCK_STATES_JSON, BLOCK_STATES_JSON),
            }
            if mutate_packets:
                mutate_packets(bodies[PACKET_IDS_JSON])
            if mutate_registries:
                mutate_registries(bodies[REGISTRY_IDS_JSON])
            file_digests = {}
            for name, body in bodies.items():
                data = dump_bytes(body)
                (forged / name).write_bytes(data)
                file_digests[name] = {"sha256": sha256_hex(data), "bytes": len(data)}
            m = dict(load_json(pack_a / MANIFEST_JSON, MANIFEST_JSON))
            m["files"] = file_digests
            (forged / MANIFEST_JSON).write_bytes(dump_bytes(m))
            return forged

        dup_packet = forge(
            mutate_packets=lambda b: b["packets"]["play"]["clientbound"].update(
                {"minecraft:alias_for_1": 1}))
        msg = _expect_fail(lambda: verify_pack(dup_packet),
                           "verify: duplicate packet protocol_id fails", failures)
        if msg: checks.append(msg)

        dup_reg = forge(
            mutate_registries=lambda b: b["registries"]["minecraft:item"]["entries"].update(
                {"minecraft:another_air": 0}))
        msg = _expect_fail(lambda: verify_pack(dup_reg),
                           "verify: duplicate registry protocol_id fails", failures)
        if msg: checks.append(msg)

        missing_core = forge(
            mutate_registries=lambda b: b["registries"].pop("minecraft:menu"))
        msg = _expect_fail(lambda: verify_pack(missing_core),
                           "verify: missing core registry fails", failures)
        if msg: checks.append(msg)

        # 3g/3h. build-time fail-closed on bad reports
        bad_reports = tmp / "bad_dup" / "reports"
        bad_reports.mkdir(parents=True)
        dup_in = _fixture(767)
        dup_in["packets.json"]["play"]["clientbound"]["minecraft:dup"] = {"protocol_id": 1}
        for name, obj in dup_in.items():
            (bad_reports / name).write_text(json.dumps(obj), encoding="utf-8")
        msg = _expect_fail(lambda: cmd_build(_ns(
            reports=str(bad_reports), out=str(tmp / "bad_dup_out"),
            version_json=None, mc_version="9.9.9", protocol=1,
            data_version=None, pack_format=None, jar_sha1=None)),
            "build: duplicate packet id in reports fails", failures)
        if msg: checks.append(msg)

        noreg = tmp / "bad_noreg" / "reports"
        noreg.mkdir(parents=True)
        noreg_in = _fixture(767)
        del noreg_in["registries.json"]["minecraft:data_component_type"]
        for name, obj in noreg_in.items():
            (noreg / name).write_text(json.dumps(obj), encoding="utf-8")
        msg = _expect_fail(lambda: cmd_build(_ns(
            reports=str(noreg), out=str(tmp / "bad_noreg_out"),
            version_json=None, mc_version="9.9.9", protocol=1,
            data_version=None, pack_format=None, jar_sha1=None)),
            "build: missing core registry in reports fails", failures)
        if msg: checks.append(msg)

        # 3i. malformed JSON
        badjson = tmp / "bad_json" / "reports"
        badjson.mkdir(parents=True)
        for name, obj in _fixture(767).items():
            (badjson / name).write_text(json.dumps(obj), encoding="utf-8")
        (badjson / "packets.json").write_text("{not json", encoding="utf-8")
        msg = _expect_fail(lambda: cmd_build(_ns(
            reports=str(badjson), out=str(tmp / "bad_json_out"),
            version_json=None, mc_version="9.9.9", protocol=1,
            data_version=None, pack_format=None, jar_sha1=None)),
            "build: malformed packets.json fails", failures)
        if msg: checks.append(msg)

        # ---- 4. diff 767 -> 769 -------------------------------------------------
        delta = diff_packs(pack_a, pack_b)
        play = delta["packets"]["play/clientbound"]
        check("diff: added play packet detected",
              play["added"] == {"minecraft:new_inserted_packet": 2,
                                "minecraft:minecart_along_track": 40})
        check("diff: removed play packet detected",
              play["removed"] == {"minecraft:legacy_removed_packet": 99})
        check("diff: renumbered play packet detected",
              play["renumbered"] == {"minecraft:player_chat": {"from_id": 2, "to_id": 3}})
        check("diff: unchanged count correct", play["unchanged_count"] == 4)
        check("diff: new configuration packet detected",
              delta["packets"]["configuration/clientbound"]["added"]
              == {"minecraft:select_known_packs": 14})

        comp = delta["registries"]["minecraft:data_component_type"]
        check("diff: data components added",
              comp["added"] == {"minecraft:equippable": 1, "minecraft:tooltip_style": 4})
        check("diff: data components removed",
              comp["removed"] == {"minecraft:legacy_thing": 3})
        check("diff: data components renumbered",
              comp["renumbered"] == {
                  "minecraft:custom_name": {"from_id": 1, "to_id": 2},
                  "minecraft:enchantments": {"from_id": 2, "to_id": 3}})
        et = delta["registries"]["minecraft:entity_type"]
        check("diff: entity added", et["added"] == {"minecraft:creaking": 129})
        menu = delta["registries"]["minecraft:menu"]
        check("diff: menu entry added + registry renumbered",
              menu["added"] == {"minecraft:crafter_3x3": 9}
              and menu["registry_renumbered"] == {"from_id": 12, "to_id": 13})
        check("diff: added registry detected",
              delta["registries"]["minecraft:variant_registry"]["added_registry"] is True)
        check("diff: unchanged registry is all-unchanged",
              delta["registries"]["minecraft:item"]["unchanged_count"] == 3
              and not delta["registries"]["minecraft:item"]["added"])

        bs = delta["block_states"]
        check("diff: block added / renumbered / resized",
              bs["added"] == {"minecraft:new_block": 1}
              and bs["renumbered"] == {
                  "minecraft:stone": {"from_id": 1, "to_id": 2},
                  "minecraft:dirt": {"from_id": 3, "to_id": 4}}
              and bs["resized"] == {"minecraft:dirt": {"from_count": 1, "to_count": 2}})

        remap = delta["id_remap"]["registries"]["minecraft:data_component_type"]
        check("diff: id_remap forward exact",
              remap["forward"] == {"1": 2, "2": 3})
        check("diff: id_remap reverse exact",
              remap["reverse"] == {"2": 1, "3": 2})
        check("diff: id_remap added/removed ids",
              remap["added_ids"] == [1, 4] and remap["removed_ids"] == [3])
        premap = delta["id_remap"]["packets"]["play/clientbound"]
        check("diff: packet remap forward",
              premap["forward"] == {"2": 3} and premap["added_ids"] == [2, 40])

        # reverse direction is the inverse
        back = diff_packs(pack_b, pack_a)
        check("diff: reverse direction inverts",
              back["id_remap"]["registries"]["minecraft:data_component_type"]["forward"]
              == {"2": 1, "3": 2})

        # ---- 5. real-target cross-version: 1.21.1 (767) -> 26.3 (777) ---------
        rep777 = _write_reports(tmp / "v777", 777)
        pack_c = tmp / "pack_777"
        cmd_build(_ns(reports=str(rep777), out=str(pack_c),
                      version_json=str(tmp / "v777" / "version.json"),
                      mc_version=None, protocol=None, data_version=None,
                      pack_format=None, jar_sha1=None))
        check("26.3 build: all four pack files written",
              all((pack_c / f).is_file() for f in files))
        manifest_c = load_json(pack_c / MANIFEST_JSON, MANIFEST_JSON)
        check("26.3 build: manifest records flat pack_version major/minor",
              manifest_c["mc_version"] == "26.3"
              and manifest_c["protocol_version"] == 777
              and manifest_c["data_version"] == 5023
              and manifest_c["pack_format"] == {
                  "data_major": 121, "data_minor": 0,
                  "resource_major": 97, "resource_minor": 1})
        mc, pc, rc, bc = verify_pack(pack_c)
        check("26.3 verify: valid pack passes",
              mc["protocol_version"] == 777
              and pc["play"]["clientbound"]["minecraft:add_entity"] == 1
              and "minecraft:timeline" in rc)

        delta_c = diff_packs(pack_a, pack_c)
        check("26.3 diff: header records both real targets",
              delta_c["from"] == {"mc_version": "1.21.1", "protocol_version": 767}
              and delta_c["to"] == {"mc_version": "26.3", "protocol_version": 777})
        play_c = delta_c["packets"]["play/clientbound"]
        check("26.3 diff: added play packets detected",
              play_c["added"] == {"minecraft:new_inserted_packet": 2,
                                  "minecraft:minecart_along_track": 40,
                                  "minecraft:chunk_batch_finished_v2": 41})
        check("26.3 diff: removed play packet detected",
              play_c["removed"] == {"minecraft:legacy_removed_packet": 99})
        check("26.3 diff: renumbered play packet detected",
              play_c["renumbered"]
              == {"minecraft:player_chat": {"from_id": 2, "to_id": 3}})
        check("26.3 diff: new configuration packets detected",
              delta_c["packets"]["configuration/clientbound"]["added"]
              == {"minecraft:select_known_packs": 14,
                  "minecraft:transfer_v2": 15})
        comp_c = delta_c["registries"]["minecraft:data_component_type"]
        check("26.3 diff: data components added",
              comp_c["added"] == {"minecraft:equippable": 1,
                                  "minecraft:tooltip_style": 4,
                                  "minecraft:kinetic_weapon": 5})
        check("26.3 diff: data components renumbered",
              comp_c["renumbered"] == {
                  "minecraft:custom_name": {"from_id": 1, "to_id": 2},
                  "minecraft:enchantments": {"from_id": 2, "to_id": 3}})
        check("26.3 diff: second added registry detected",
              delta_c["registries"]["minecraft:timeline"]["added_registry"] is True)
        remap_c = delta_c["id_remap"]["registries"]["minecraft:data_component_type"]
        check("26.3 diff: id_remap forward exact",
              remap_c["forward"] == {"1": 2, "2": 3})
        check("26.3 diff: id_remap reverse exact",
              remap_c["reverse"] == {"2": 1, "3": 2})
        check("26.3 diff: id_remap added/removed ids",
              remap_c["added_ids"] == [1, 4, 5]
              and remap_c["removed_ids"] == [3])
        # Forward and reverse directions are true inverses across targets.
        back_c = diff_packs(pack_c, pack_a)
        check("26.3 diff: reverse direction inverts",
              back_c["id_remap"]["registries"]["minecraft:data_component_type"]["forward"]
              == {"2": 1, "3": 2}
              and back_c["packets"]["play/clientbound"]["added"]
              == {"minecraft:legacy_removed_packet": 99})

    for line in checks:
        print(line)
    total = len(checks)
    if failures:
        print(f"self-test: {total - len(failures)}/{total} passed, "
              f"{len(failures)} FAILED: {failures}")
        return 1
    print(f"self-test: all {total} checks passed")
    return 0


# ---- cli -----------------------------------------------------------------------

def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="version_pack.py",
        description="Build, verify and diff megu VersionPacks from vanilla "
                    "DataGenerator --reports output.")
    parser.add_argument("--self-test", action="store_true",
                        help="run the offline self-test suite and exit")
    sub = parser.add_subparsers(dest="command")

    p_build = sub.add_parser("build", help="build a VersionPack from datagen reports")
    p_build.add_argument("--reports", required=True, help="datagen reports dir")
    p_build.add_argument("--out", required=True, help="output pack dir")
    p_build.add_argument("--version-json", default=None,
                         help="server jar version.json (id/protocol_version/world_version/pack_version)")
    p_build.add_argument("--mc-version", default=None)
    p_build.add_argument("--protocol", type=int, default=None)
    p_build.add_argument("--data-version", type=int, default=None)
    p_build.add_argument("--pack-format", type=int, default=None)
    p_build.add_argument("--jar-sha1", default=None)

    p_verify = sub.add_parser("verify", help="fail-closed validation of a pack dir")
    p_verify.add_argument("--pack", required=True, help="pack dir")

    p_diff = sub.add_parser("diff", help="cross-version packet/registry delta")
    p_diff.add_argument("--from", dest="from_dir", required=True, help="older pack dir")
    p_diff.add_argument("--to", dest="to_dir", required=True, help="newer pack dir")
    p_diff.add_argument("--json", action="store_true", help="emit the full delta as JSON")

    sub.add_parser("self-test", help="run the offline self-test suite")

    args = parser.parse_args(argv)
    try:
        if args.self_test or args.command == "self-test":
            return self_test()
        if args.command == "build":
            return cmd_build(args)
        if args.command == "verify":
            return cmd_verify(args)
        if args.command == "diff":
            return cmd_diff(args)
        parser.print_help()
        return 2
    except PackError as error:
        print(f"version_pack: error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
