#!/usr/bin/env python3
"""modloader_parity_audit.py -- megu NeoForge/Fabric 1.21.1 static
source-presence audit.

Schema: "megu-modloader-parity-audit/1".

Mode A (always, zero external files, repo self-audit) is a **Static Source
Presence Coverage** audit, not a behavioural one: it scans non-test Rust code
across src/, mod-adapter-core/src/, mod-offload/src/, create-adapter/src/ and
proto/src/ (masking #[cfg(test)] regions and *_tests.rs files so test-only code
never inflates coverage) and audits 6 concrete loader-parity dimensions:

  A1 Mod metadata & packaging specs        (8 items)
  A2 Loader wire channels & handshake      (8 items)
  A3 Data-driven mod resource directories  (12 items)
  A4 Generic execution & safety primitives (10 items)
  A5 NeoForge 1.21.1 `c:` common tag families (10 items)
  A6 Security & isolation invariants       (5 items)

Each item is data-driven: a list of lowercase literal evidence needles that
must appear in live (comment-blanked, test-masked) source. An item is COVERED
when any needle is found (`all_of: true` items require every needle). The
report emits covered/total and % for NeoForge Parity, Fabric Parity and
Overall Modloader Parity, plus every GAP item.

What those percentages mean: a needle being present in the source is evidence
that a mechanism exists and is wired, nothing more. They do not measure runtime
behaviour, and they are not a modpack compatibility score -- a mod whose JAR is
handled in Mode B, or a whole modpack, is not exercised here. Treat 100% as
"every audited mechanism is present in source", never as "mods run".

Mode B (optional, read-only): audits real mod JARs via --mods-dir <dir> and/or
--jar <file.jar> (repeatable). Every .jar is inspected with bounded ZIP limits
(max entries / per-member bytes / total decompressed bytes) and path-traversal
rejection. Each JAR is classified CLIENT_ONLY_AUTO_SKIP | NATIVE_DATA_DRIVEN
| NATIVE_PRIMITIVE_OR_VM | JVM_BRIDGE_NEEDED | UNSUPPORTED_NATIVE_LIB, and the
report aggregates the top missing generic primitives across the modpack.

Pure Python 3 stdlib. No shell=True. Linux/macOS/Windows portable.

Exit codes: 0 = report produced; 1 = strict gate failed (Mode A static source
presence below --min-overall-pct, or --strict with any Mode B integrity error);
2 = fail-closed input/parse error.
"""

import argparse
import json
import re
import sys
import zipfile
from collections import Counter
from pathlib import Path, PurePosixPath

SCHEMA = "megu-modloader-parity-audit/1"

SCAN_DIRS = [
    "src",
    "mod-adapter-core/src",
    "mod-offload/src",
    "create-adapter/src",
    "proto/src",
]

# ---------------------------------------------------------------------------
# Rust lexing (regex-tokenized; comments blanked, strings preserved for `live`,
# string/char literal spans additionally blanked for `skel`).
# ---------------------------------------------------------------------------

_TOKEN_RE = re.compile(
    r"//[^\n]*"                       # line comment
    r"|/\*"                            # block comment open
    r"|\*/"                            # block comment close (stray)
    r"|(?<![\w'])(?:br|rb|cr)(#*)\"[\s\S]*?\"\1"   # raw string w/ prefix
    r"|(?<![\w'])r(#*)\"[\s\S]*?\"\2"             # raw string r"..." / r#"..."#
    r"|(?<![\w'])[bc]\"(?:\\.|[^\"\\\n])*\""       # byte / C string
    r"|\"(?:\\.|[^\"\\\n])*\""                     # normal string
    r"|'(?:\\.|[^'\\\n])'"                        # char literal
)

_CFG_TEST = re.compile(
    r"#\!?\[cfg\(test\)\]"
    r"|#\!?\[cfg\((?:all|any)\([^]]*\btest\b[^]]*\)\)\]")


def _scan_tokens(text):
    """Return (comment_spans, literal_spans); spans are [start, end)."""
    comments = []
    literals = []
    pos, n = 0, len(text)
    while True:
        m = _TOKEN_RE.search(text, pos)
        if not m:
            break
        tok = m.group(0)
        if tok.startswith("//"):
            comments.append(m.span())
            pos = m.end()
        elif tok.startswith("/*"):
            # nested block comments: raw depth scan (comment content is pure
            # comment text; strings inside cannot escape it)
            depth = 1
            i = m.end()
            while i < n and depth:
                two = text[i:i + 2]
                if two == "/*":
                    depth += 1
                    i += 2
                elif two == "*/":
                    depth -= 1
                    i += 2
                else:
                    i += 1
            comments.append((m.start(), i))
            pos = i
        elif tok == "*/":
            pos = m.end()
        else:
            literals.append(m.span())
            pos = m.end()
    return comments, literals


def _blank_spans(text, spans):
    """Blank [start,end) spans preserving newlines and total length."""
    if not spans:
        return text
    parts = []
    prev = 0
    for a, b in sorted(spans):
        if a < prev:
            a = prev
        if a >= b:
            continue
        parts.append(text[prev:a])
        seg = text[a:b]
        parts.append("".join("\n" if c == "\n" else " " for c in seg))
        prev = b
    parts.append(text[prev:])
    return "".join(parts)


def _match_bracket(skel, open_idx, open_c="{", close_c="}"):
    depth = 0
    for i in range(open_idx, len(skel)):
        c = skel[i]
        if c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return i
    return -1


def mask_test_regions(live, skel):
    """Blank #[cfg(test)]-annotated items in `live`; return masked text.

    `live` has comments blanked but string contents preserved; `skel` is the
    same text with string/char literal spans also blanked (same length, so
    offsets align). Only the masked live text is returned.
    """
    n = len(skel)
    spans = []
    for m in _CFG_TEST.finditer(skel):
        k = m.end()
        while k < n:
            while k < n and skel[k] in " \t\n":
                k += 1
            if k < n and skel[k] == "#":
                close = skel.find("]", k)
                if close < 0:
                    break
                k = close + 1
                continue
            break
        if k >= n:
            continue
        par = brk = 0
        i = k
        end = -1
        while i < n:
            c = skel[i]
            if c == "(":
                par += 1
            elif c == ")":
                par -= 1
            elif c == "[":
                brk += 1
            elif c == "]":
                brk -= 1
            elif c == "{" and par == 0 and brk == 0:
                close = _match_bracket(skel, i)
                end = close + 1 if close > 0 else n
                break
            elif c == ";" and par == 0 and brk == 0:
                end = i + 1
                break
            i += 1
        if end < 0:
            end = n
        spans.append((m.start(), end))
    if not spans:
        return live
    return _blank_spans(live, spans)


def live_source(text):
    """One-shot: raw Rust source -> (live_masked, skeleton).

    live_masked: comments blanked, #[cfg(test)] items removed, strings kept.
    skeleton:   comments + literals blanked (structure only).
    """
    comments, literals = _scan_tokens(text)
    live = _blank_spans(text, comments)
    has_cfg_test = ("cfg(test)" in text or "cfg(all" in text
                    or "cfg(any" in text or "cfg(not" in text)
    if not has_cfg_test:
        return live, None
    skel = _blank_spans(text, comments + literals)
    return mask_test_regions(live, skel), skel


# ---------------------------------------------------------------------------
# Mode A: loader-parity dimensions (data-driven items)
# ---------------------------------------------------------------------------

# Each item: {"id", "title", "loaders": subset of {"neoforge","fabric"},
#             "needles": [lowercase literals], "all_of": bool}
# A needle is a plain substring matched against lowercased live source.
# `all_of: true` requires every needle to be present (e.g. dual-loader proof).

_DIMENSIONS = [
    {
        "id": "A1",
        "name": "Mod Metadata & Packaging Specs",
        "items": [
            {"id": "neoforge.mods.toml", "loaders": ["neoforge"],
             "needles": ["neoforge.mods.toml", "net.neoforged",
                         "neoforged.fml", "javafml"]},
            {"id": "mods.toml", "loaders": ["neoforge"],
             "needles": ["/mods.toml", '"mods.toml"', "net.minecraftforge",
                         "fmlcommon", "modlauncher"]},
            {"id": "fabric.mod.json", "loaders": ["fabric"],
             "needles": ["fabric.mod.json", "fabricmc", "net.fabricmc",
                         "fabricloader"]},
            {"id": "quilt.mod.json", "loaders": ["fabric"],
             "needles": ["quilt.mod.json", "quilt_loader", "org.quiltmc"]},
            {"id": "pack.mcmeta", "loaders": ["neoforge", "fabric"],
             "needles": ["pack.mcmeta"]},
            {"id": "jarjar_metadata", "loaders": ["neoforge", "fabric"],
             "needles": ["jarjar", "jar-in-jar", "jar_in_jar",
                         "jarjar/metadata.json"]},
            {"id": "accesstransformer", "loaders": ["neoforge", "fabric"],
             "needles": ["accesstransformer", "access_transformer",
                         "accesswidener", "access_widener"]},
            {"id": "mixins_json", "loaders": ["neoforge", "fabric"],
             "needles": [".mixins.json", "mixins.json", "org.spongepowered",
                         "mixinextras"]},
        ],
    },
    {
        "id": "A2",
        "name": "Loader Wire Channels & Handshake",
        "items": [
            {"id": "minecraft:register", "loaders": ["neoforge", "fabric"],
             "needles": ["minecraft:register"]},
            {"id": "minecraft:unregister", "loaders": ["neoforge", "fabric"],
             "needles": ["minecraft:unregister"]},
            {"id": "neoforge:register", "loaders": ["neoforge"],
             "needles": ["neoforge:register", "neoforged:register",
                         "neoforge:frozen_registry", "frozen_registry_sync"]},
            {"id": "registry_snapshot_or_c_register",
             "loaders": ["neoforge", "fabric"],
             "needles": ["registry_snapshot", "registrysnapshot", "c:register",
                         "registry/sync"]},
            {"id": "fabric:registry/sync/direct", "loaders": ["fabric"],
             "needles": ["fabric:registry/sync/direct",
                         "fabric:registry/sync", "registry/sync/direct"]},
            {"id": "megu:mod_sync_v1", "loaders": ["neoforge", "fabric"],
             "needles": ["megu:mod_sync_v1", "mod_sync_v1"]},
            {"id": "appleskin", "loaders": ["neoforge", "fabric"],
             "needles": ["appleskin"]},
            {"id": "custom_payload", "loaders": ["neoforge", "fabric"],
             "needles": ["custom_payload", "custompacketpayload",
                         "serverboundcustompayload", "clientboundcustompayload",
                         "packetpayload"]},
        ],
    },
    {
        "id": "A3",
        "name": "Data-Driven Mod Resource Directories",
        "items": [
            {"id": "recipe/recipes", "loaders": ["neoforge", "fabric"],
             "needles": ["recipe", "recipes"]},
            {"id": "tags/block", "loaders": ["neoforge", "fabric"],
             "needles": ["tags/block", "block tags", "block_tags"]},
            {"id": "tags/item", "loaders": ["neoforge", "fabric"],
             "needles": ["tags/item", "item tags", "item_tags"]},
            {"id": "tags/entity_type", "loaders": ["neoforge", "fabric"],
             "needles": ["tags/entity_type", "entity_type"]},
            {"id": "tags/fluid", "loaders": ["neoforge", "fabric"],
             "needles": ["tags/fluid", "fluid_tags", "fluid tag"]},
            {"id": "loot_table/loot_tables", "loaders": ["neoforge", "fabric"],
             "needles": ["loot_table", "loot_tables", "loottable"]},
            {"id": "advancement", "loaders": ["neoforge", "fabric"],
             "needles": ["advancement"]},
            {"id": "worldgen/biome", "loaders": ["neoforge", "fabric"],
             "needles": ["worldgen/biome", "biome_source", "climate"]},
            {"id": "worldgen/structure", "loaders": ["neoforge", "fabric"],
             "needles": ["worldgen/structure", "structure_set",
                         "structure_piece"]},
            {"id": "dimension", "loaders": ["neoforge", "fabric"],
             "needles": ["dimension_type", "dimension"]},
            {"id": "neoforge/data_maps", "loaders": ["neoforge", "fabric"],
             "needles": ["data_maps", "data map", "datamap"]},
            {"id": "neoforge/biome_modifier", "loaders": ["neoforge", "fabric"],
             "needles": ["biome_modifier", "biomemodifier"]},
        ],
    },
    {
        "id": "A4",
        "name": "Generic Execution & Safety Primitives",
        "items": [
            {"id": "bounded_class_vm", "loaders": ["neoforge", "fabric"],
             "needles": ["bytecode_subset_vm", "class_vm", "parse_class_methods",
                         "verify_subset_method", "bounded class"]},
            {"id": "jvm_bridge_or_jlink_jre", "loaders": ["neoforge", "fabric"],
             "needles": ["megu-jvm-bridge", "jvm-bridge", "jlink",
                         "jvm bridge", "minimal jre", "boundedjvmshim",
                         "jvm shim", "jvm"]},
            {"id": "client_only_auto_skip", "loaders": ["neoforge", "fabric"],
             "needles": ["client_only_auto_skip", "clientonly", "client-only",
                         "client_only"]},
            {"id": "dual_loader_scanner", "loaders": ["neoforge", "fabric"],
             "all_of": True,
             "needles": ["fabric", "neoforge"]},
            {"id": "modrinth_dep_resolver", "loaders": ["neoforge", "fabric"],
             "needles": ["modrinth", "dep_resolver", "dependency resolver",
                         "missing dep"]},
            {"id": "sha256_content_addressed_mod_sync",
             "loaders": ["neoforge", "fabric"],
             "needles": ["sha256", "sha-256", "contentaddressed",
                         "content-addressed", "mod_sync"]},
            {"id": "hot_reload_safety_classifier",
             "loaders": ["neoforge", "fabric"],
             "needles": ["hot_reload", "hot-reload", "assess_hot_reload",
                         "reload classifier"]},
            {"id": "generic_recipe_processing_graph",
             "loaders": ["neoforge", "fabric"],
             "needles": ["recipe_processor", "recipe graph", "processing graph",
                         "recipe_processor"]},
            {"id": "generic_rail_contraption_kinetic",
             "loaders": ["neoforge", "fabric"],
             "needles": ["contraption", "kinetic", "rail_network"]},
            {"id": "generic_collector_accessory_claim",
             "loaders": ["neoforge", "fabric"],
             "needles": ["collector", "accessory", "claim"]},
        ],
    },
    {
        "id": "A5",
        "name": "NeoForge 1.21.1 `c:` Common Tag Families",
        "items": [
            {"id": "c:ores", "loaders": ["neoforge", "fabric"],
             "needles": ["c:ores"]},
            {"id": "c:ingots", "loaders": ["neoforge", "fabric"],
             "needles": ["c:ingots"]},
            {"id": "c:raw_materials", "loaders": ["neoforge", "fabric"],
             "needles": ["c:raw_materials", "c:raw_material"]},
            {"id": "c:crops", "loaders": ["neoforge", "fabric"],
             "needles": ["c:crops"]},
            {"id": "c:foods", "loaders": ["neoforge", "fabric"],
             "needles": ["c:foods"]},
            {"id": "c:tools", "loaders": ["neoforge", "fabric"],
             "needles": ["c:tools"]},
            {"id": "c:stones", "loaders": ["neoforge", "fabric"],
             "needles": ["c:stones"]},
            {"id": "c:dyes", "loaders": ["neoforge", "fabric"],
             "needles": ["c:dyes"]},
            {"id": "c:gems", "loaders": ["neoforge", "fabric"],
             "needles": ["c:gems"]},
            {"id": "c:storage_blocks", "loaders": ["neoforge", "fabric"],
             "needles": ["c:storage_blocks", "c:storage_block"]},
        ],
    },
    {
        "id": "A6",
        "name": "Security & Isolation Invariants",
        "items": [
            {"id": "zip_path_traversal_rejection",
             "loaders": ["neoforge", "fabric"],
             "needles": ["path traversal", "traversal", "zip slip",
                         "directory traversal", "path_traversal"]},
            {"id": "jar_in_jar_nested_bounds",
             "loaders": ["neoforge", "fabric"],
             "needles": ["jarjar", "jar-in-jar", "nested jar", "nested archive",
                         "zip bomb", "decompression bomb"]},
            {"id": "mixin_fail_closed",
             "loaders": ["neoforge", "fabric"],
             "needles": ["mixin", "fail-closed", "fail_closed", "fail closed",
                         "restartrequired", "restart required"]},
            {"id": "native_lib_fail_closed",
             "loaders": ["neoforge", "fabric"],
             "needles": [".dll", ".dylib", "dlopen", "loadlibrary",
                         "native library", "native lib"]},
            {"id": "bounded_vm_fuel_opcode_allowlist",
             "loaders": ["neoforge", "fabric"],
             "needles": ["fuel", "opcode", "allowlist", "allow_list",
                         "instruction limit", "step limit"]},
        ],
    },
]


def _scan_corpus(repo):
    """Return ({rel: live_lower}, corpus_lower, files, bytes)."""
    files = {}
    parts = []
    nbytes = 0
    for d in SCAN_DIRS:
        root = repo / d
        if not root.is_dir():
            continue
        for path in sorted(root.rglob("*.rs")):
            if path.name.endswith("_tests.rs") or path.name == "test_support.rs":
                continue
            raw = path.read_text(errors="replace")
            live, _skel = live_source(raw)
            rel = path.relative_to(repo).as_posix()
            low = live.lower()
            files[rel] = low
            parts.append(low)
            nbytes += len(raw)
    corpus = "\n".join(parts)
    return files, corpus, len(files), nbytes


def mode_a(repo):
    files, corpus, nfiles, nbytes = _scan_corpus(repo)
    dims = []
    gaps = []
    loader_stats = {
        "neoforge": {"covered": 0, "total": 0},
        "fabric": {"covered": 0, "total": 0},
    }
    total_items = 0
    covered_items = 0
    for dim in _DIMENSIONS:
        items = []
        d_cov = d_tot = 0
        for it in dim["items"]:
            needles = [n.lower() for n in it["needles"]]
            if it.get("all_of"):
                covered = all(n in corpus for n in needles)
                matched = [n for n in needles if n in corpus]
            else:
                matched = [n for n in needles if n in corpus]
                covered = bool(matched)
            evidence = []
            if covered:
                for rel, txt in files.items():
                    if any(n in txt for n in needles):
                        evidence.append(rel)
                    if len(evidence) >= 8:
                        break
            status = "COVERED" if covered else "GAP"
            entry = {
                "id": it["id"],
                "loaders": it["loaders"],
                "status": status,
                "matched_needles": matched[:6],
                "evidence": evidence,
            }
            items.append(entry)
            d_tot += 1
            total_items += 1
            for loader in it["loaders"]:
                loader_stats[loader]["total"] += 1
                if covered:
                    loader_stats[loader]["covered"] += 1
            if covered:
                d_cov += 1
                covered_items += 1
            else:
                gaps.append({
                    "dimension": dim["id"], "id": it["id"],
                    "loaders": it["loaders"],
                })
        dims.append({
            "id": dim["id"],
            "name": dim["name"],
            "covered": d_cov,
            "total": d_tot,
            "items": items,
        })

    def _pct(c, t):
        return round(100.0 * c / max(1, t), 1)

    parity = {
        "neoforge": {
            "covered": loader_stats["neoforge"]["covered"],
            "total": loader_stats["neoforge"]["total"],
            "pct": _pct(loader_stats["neoforge"]["covered"],
                        loader_stats["neoforge"]["total"]),
        },
        "fabric": {
            "covered": loader_stats["fabric"]["covered"],
            "total": loader_stats["fabric"]["total"],
            "pct": _pct(loader_stats["fabric"]["covered"],
                        loader_stats["fabric"]["total"]),
        },
        "overall": {
            "covered": covered_items,
            "total": total_items,
            "pct": _pct(covered_items, total_items),
        },
        "gaps": gaps,
    }
    return {
        "mode_a": {
            "schema": SCHEMA,
            # Mode A measures literal source presence only. Kept next to the
            # numbers so a consumer cannot quote "100%" as runtime parity.
            "coverage_kind": "static_source_presence",
            "coverage_note": "items covered by literal evidence needles found "
                             "in live source; runtime behaviour and modpack "
                             "compatibility are not measured",
            "scanned_dirs": SCAN_DIRS,
            "files_scanned": nfiles,
            "bytes_scanned": nbytes,
            "dimensions": dims,
            "parity": parity,
        }
    }


# ---------------------------------------------------------------------------
# Mode B: real mod JAR audit (bounded, read-only, traversal-safe)
# ---------------------------------------------------------------------------

MAX_JAR_BYTES = 512 * 1024 * 1024
MAX_JAR_ENTRIES = 60000
MAX_MEMBER_BYTES = 16 * 1024 * 1024
MAX_TOTAL_READ = 256 * 1024 * 1024
MAX_JARS = 2048
MAX_RECIPE_READS = 50000
MAX_MANIFESTS_READ = 64

_NATIVE_EXTS = (".so", ".dll", ".dylib")
_MANIFESTS = (
    "META-INF/neoforge.mods.toml",
    "META-INF/mods.toml",
    "fabric.mod.json",
    "quilt.mod.json",
)

# Vanilla 1.21.1 recipe "type" values (namespace stripped before compare).
KNOWN_RECIPE_TYPES = {
    "crafting_shaped", "crafting_shapeless", "smelting", "blasting",
    "smoking", "campfire_cooking", "stonecutting", "smithing_transform",
    "smithing_trim",
}


class AuditError(Exception):
    """Fail-closed input error -> exit 2."""


class JarIntegrityError(Exception):
    """Corrupt/traversal JAR -> recorded as integrity error (exit 1 strict)."""


def _zip_name_safe(name):
    """Reject absolute paths, `..` segments, backslashes, drive letters, NUL."""
    if not name or "\x00" in name or "\\" in name:
        return False
    if name.startswith("/") or name.startswith("~"):
        return False
    if len(name) >= 2 and name[1] == ":":
        return False
    parts = PurePosixPath(name).parts
    return all(p not in ("..", "") for p in parts)


def _read_member(zf, info, budget):
    """Read one member bounded by per-member and total budgets."""
    if info.file_size > MAX_MEMBER_BYTES:
        return None
    if budget["total"] + info.file_size > MAX_TOTAL_READ:
        return None
    data = zf.read(info)
    budget["total"] += len(data)
    return data


def _json_dict(data):
    try:
        v = json.loads(data.decode("utf-8", errors="replace"))
    except Exception:
        return None
    return v if isinstance(v, dict) else None


def _strip_ns(t):
    return t.rsplit(":", 1)[-1] if ":" in t else t


_DATA_RE = re.compile(r"^data/[^/]+/(.+)$")


def _data_kind(rest):
    """Classify the path under data/<ns>/ into a content bucket."""
    parts = rest.split("/")
    top = parts[0]
    if top in ("recipe", "recipes"):
        return "recipe"
    if top in ("loot_table", "loot_tables"):
        return "loot_table"
    if top in ("advancement", "advancements"):
        return "advancement"
    if top == "tags" and len(parts) > 1:
        return "tag:" + parts[1]
    if top == "worldgen" and len(parts) > 1:
        return "worldgen:" + parts[1]
    if top in ("dimension", "dimensions", "dimension_type"):
        return "dimension"
    if top == "neoforge":
        if len(parts) > 1 and parts[1] in (
                "data_maps", "biome_modifier", "structure_modifier",
                "loot_modifiers", "global_loot_modifiers"):
            return "neoforge:" + parts[1]
        if len(parts) > 2 and parts[1] == "data_maps":
            return "neoforge:data_maps"
    return None


def audit_jar(path):
    """Audit one JAR; returns a report dict. Raises JarIntegrityError /
    AuditError on corrupt or hostile archives."""
    path = Path(path)
    if not path.is_file():
        raise AuditError(f"jar not found: {path}")
    if path.stat().st_size > MAX_JAR_BYTES:
        raise JarIntegrityError(f"{path.name}: exceeds {MAX_JAR_BYTES} byte cap")
    try:
        zf = zipfile.ZipFile(path)
    except zipfile.BadZipFile as exc:
        raise JarIntegrityError(f"{path.name}: corrupt ZIP ({exc})") from exc
    except Exception as exc:  # noqa: BLE001 - fail closed on any zip error
        raise JarIntegrityError(f"{path.name}: unreadable ZIP ({exc})") from exc

    with zf:
        infos = zf.infolist()
        if len(infos) > MAX_JAR_ENTRIES:
            raise JarIntegrityError(
                f"{path.name}: {len(infos)} entries exceeds "
                f"{MAX_JAR_ENTRIES} cap")
        unsafe = [i.filename for i in infos if not _zip_name_safe(i.filename)]
        if unsafe:
            raise JarIntegrityError(
                f"{path.name}: path-traversal/unsafe entry names "
                f"({unsafe[0]!r}{' ...' if len(unsafe) > 1 else ''})")

        names = [i.filename for i in infos]
        name_set = set(names)

        # ---- loader + environment from manifests (bounded reads) ----
        budget = {"total": 0}
        manifests = {}
        for mname in _MANIFESTS:
            if mname in name_set:
                info = zf.getinfo(mname)
                data = _read_member(zf, info, budget)
                if data is not None:
                    manifests[mname] = data

        loader_kinds = set()
        if "META-INF/neoforge.mods.toml" in manifests:
            loader_kinds.add("neoforge")
        if "META-INF/mods.toml" in manifests:
            text = manifests["META-INF/mods.toml"].decode(
                "utf-8", errors="replace").lower()
            loader_kinds.add("neoforge" if "neoforged" in text else "forge")
        if "fabric.mod.json" in manifests:
            loader_kinds.add("fabric")
            doc = _json_dict(manifests["fabric.mod.json"])
            if doc is not None:
                env = str(doc.get("environment", "*")).lower()
                manifests["_fabric_env"] = env
        if "quilt.mod.json" in manifests:
            loader_kinds.add("fabric")
            doc = _json_dict(manifests["quilt.mod.json"])
            if doc is not None:
                manifests["_quilt_env"] = str(
                    doc.get("environment", "*")).lower()

        if len(loader_kinds) > 1:
            loader = "dual"
        elif loader_kinds:
            loader = sorted(loader_kinds)[0]
        else:
            loader = "unknown"

        env = manifests.get("_fabric_env") or manifests.get("_quilt_env") or "*"
        client_only = env == "client"

        # ---- structural counts (names only, no decompression) ----
        class_count = sum(1 for n in names if n.endswith(".class"))
        native_libs = sorted(n for n in names
                             if n.lower().endswith(_NATIVE_EXTS))[:64]
        mixin_configs = [n for n in names if n.endswith(".mixins.json")]
        at_entries = [n for n in names
                      if n.rsplit("/", 1)[-1].lower()
                      in ("accesstransformer.cfg", "accesswidener")
                      or n.lower().endswith(".accesswidener")]
        jarjar_meta = "META-INF/jarjar/metadata.json" in name_set
        nested_jars = [n for n in names
                       if n.startswith("META-INF/jarjar/")
                       and n.endswith(".jar")]
        has_pack_mcmeta = "pack.mcmeta" in name_set

        # ---- data-driven content buckets ----
        buckets = Counter()
        recipe_types = Counter()
        recipe_reads = 0
        recipe_jsons = []
        for info in infos:
            m = _DATA_RE.match(info.filename)
            if not m:
                continue
            kind = _data_kind(m.group(1))
            if kind is None:
                continue
            buckets[kind] += 1
            if kind == "recipe" and recipe_reads < MAX_RECIPE_READS:
                data = _read_member(zf, info, budget)
                recipe_reads += 1
                if data is not None and len(data) <= MAX_MEMBER_BYTES:
                    recipe_jsons.append(data)

        for data in recipe_jsons:
            doc = _json_dict(data)
            if doc is None:
                continue
            rtype = doc.get("type")
            if isinstance(rtype, str) and rtype:
                recipe_types[_strip_ns(rtype)] += 1

        # ---- mixin configs: exact target class list ----
        mixin_targets = []
        mixin_config_count = 0
        for cfg_name in mixin_configs[:MAX_MANIFESTS_READ]:
            info = zf.getinfo(cfg_name)
            data = _read_member(zf, info, budget)
            if data is None:
                continue
            doc = _json_dict(data)
            if doc is None:
                continue
            mixin_config_count += 1
            pkg = str(doc.get("package", ""))
            for section in ("mixins", "client", "server"):
                arr = doc.get(section)
                if isinstance(arr, list):
                    for cls in arr:
                        if isinstance(cls, str):
                            mixin_targets.append(
                                f"{pkg}.{cls}" if pkg else cls)

        # ---- access transformers ----
        at_lines = 0
        for at_name in at_entries[:MAX_MANIFESTS_READ]:
            info = zf.getinfo(at_name)
            data = _read_member(zf, info, budget)
            if data is None:
                continue
            for line in data.decode("utf-8", errors="replace").splitlines():
                s = line.strip()
                if s and not s.startswith("#"):
                    at_lines += 1

        # ---- Jar-in-Jar ----
        jarjar_count = 0
        if jarjar_meta:
            info = zf.getinfo("META-INF/jarjar/metadata.json")
            data = _read_member(zf, info, budget)
            doc = _json_dict(data) if data is not None else None
            if doc is not None and isinstance(doc.get("jars"), list):
                jarjar_count = len(doc["jars"])
        jarjar_count = max(jarjar_count, len(nested_jars))

        data_total = sum(buckets.values())

        # ---- classification (fail-closed ordering) ----
        reasons = []
        if client_only:
            classification = "CLIENT_ONLY_AUTO_SKIP"
            reasons.append(f"environment={env}")
        elif native_libs:
            classification = "UNSUPPORTED_NATIVE_LIB"
            reasons.append(f"{len(native_libs)} native librar"
                           f"{'y' if len(native_libs) == 1 else 'ies'}")
        elif class_count > 0 or jarjar_count > 0:
            needs_bridge = bool(mixin_targets) or at_lines > 0 or jarjar_count > 0
            if needs_bridge:
                classification = "JVM_BRIDGE_NEEDED"
                if mixin_targets:
                    reasons.append(f"{len(mixin_targets)} mixin target(s)")
                if at_lines:
                    reasons.append(f"{at_lines} access-transformer line(s)")
                if jarjar_count:
                    reasons.append(f"{jarjar_count} Jar-in-Jar archive(s)")
            else:
                classification = "NATIVE_PRIMITIVE_OR_VM"
                reasons.append(f"{class_count} .class file(s), no mixin/AT/JarJar")
        elif data_total > 0 or has_pack_mcmeta:
            classification = "NATIVE_DATA_DRIVEN"
            reasons.append(f"{data_total} data-driven member(s)")
        else:
            classification = "UNKNOWN"
            reasons.append("no loader manifest, classes or data content")

        # ---- missing generic primitives contributed by this JAR ----
        primitives = set()
        for rtype in recipe_types:
            if rtype not in KNOWN_RECIPE_TYPES:
                primitives.add(f"recipe_type:{rtype}")
        if buckets.get("neoforge:data_maps"):
            primitives.add("neoforge/data_maps")
        if buckets.get("neoforge:biome_modifier"):
            primitives.add("neoforge/biome_modifier")
        if buckets.get("neoforge:structure_modifier"):
            primitives.add("neoforge/structure_modifier")
        if buckets.get("neoforge:loot_modifiers") \
                or buckets.get("neoforge:global_loot_modifiers"):
            primitives.add("neoforge/loot_modifiers")
        if mixin_targets:
            primitives.add("mixin_targets")
        if at_lines:
            primitives.add("accesstransformer")
        if jarjar_count:
            primitives.add("jar_in_jar")
        if native_libs:
            primitives.add("native_lib")

        counts = {
            "entries": len(names),
            "class_files": class_count,
            "mixin_configs": mixin_config_count,
            "mixin_targets": len(mixin_targets),
            "accesstransformer_lines": at_lines,
            "jarjar_archives": jarjar_count,
            "native_libs": len(native_libs),
            "pack_mcmeta": has_pack_mcmeta,
            "data_members": data_total,
            "recipes": sum(recipe_types.values()),
            "recipe_types": dict(sorted(recipe_types.items())),
            "tags": sum(v for k, v in buckets.items() if k.startswith("tag:")),
            "loot_tables": buckets.get("loot_table", 0),
            "advancements": buckets.get("advancement", 0),
            "worldgen": sum(v for k, v in buckets.items()
                            if k.startswith("worldgen:")),
            "dimensions": buckets.get("dimension", 0),
            "neoforge_data_maps": buckets.get("neoforge:data_maps", 0),
            "neoforge_biome_modifiers":
                buckets.get("neoforge:biome_modifier", 0),
            "neoforge_structure_modifiers":
                buckets.get("neoforge:structure_modifier", 0),
        }

        return {
            "file": path.name,
            "path": str(path),
            "loader": loader,
            "environment": env,
            "client_only": client_only,
            "classification": classification,
            "reasons": reasons,
            "counts": counts,
            "recipe_types": dict(sorted(recipe_types.items())),
            "mixin_target_sample": mixin_targets[:16],
            "native_lib_sample": native_libs[:16],
            "missing_primitives": sorted(primitives),
        }


def collect_jars(args):
    jars = []
    if args.jar:
        for j in args.jar:
            jars.append(Path(j))
    if args.mods_dir:
        mdir = Path(args.mods_dir)
        if not mdir.is_dir():
            raise AuditError(f"--mods-dir {mdir} is not a directory")
        found = sorted(mdir.rglob("*.jar"))
        if len(found) > MAX_JARS:
            raise AuditError(
                f"--mods-dir {mdir}: {len(found)} jars exceeds "
                f"{MAX_JARS} cap")
        jars.extend(found)
    # de-dup preserving order
    seen = set()
    out = []
    for j in jars:
        key = str(j)
        if key not in seen:
            seen.add(key)
            out.append(j)
    return out


def mode_b(jar_paths):
    """Audit a list of JAR paths -> (mode_b_report, integrity_errors)."""
    jars = []
    integrity_errors = []
    for jp in jar_paths:
        try:
            jars.append(audit_jar(jp))
        except JarIntegrityError as exc:
            integrity_errors.append(str(exc))
            jars.append({
                "file": Path(jp).name, "path": str(jp),
                "loader": "unknown", "environment": "*",
                "client_only": False,
                "classification": "REJECTED",
                "reasons": [str(exc)],
                "counts": {}, "recipe_types": {},
                "mixin_target_sample": [], "native_lib_sample": [],
                "missing_primitives": [],
            })
    by_class = Counter(j["classification"] for j in jars)
    by_loader = Counter(j["loader"] for j in jars)

    prim_counter = Counter()
    prim_files = {}
    for j in jars:
        for p in j.get("missing_primitives", []):
            prim_counter[p] += 1
            prim_files.setdefault(p, []).append(j["file"])
    top_missing = [
        {"primitive": p, "jars": c,
         "files": sorted(prim_files[p])[:8]}
        for p, c in sorted(prim_counter.items(),
                           key=lambda kv: (-kv[1], kv[0]))[:25]
    ]

    return {
        "mode_b": {
            "jars_total": len(jars),
            "by_classification": dict(sorted(by_class.items())),
            "by_loader": dict(sorted(by_loader.items())),
            "client_only_skipped": by_class.get("CLIENT_ONLY_AUTO_SKIP", 0),
            "integrity_errors": integrity_errors,
            "top_missing_primitives": top_missing,
            "jars": jars,
        }
    }, integrity_errors


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

_CLASS_LABELS = {
    "CLIENT_ONLY_AUTO_SKIP": "client-only (auto-skip)",
    "NATIVE_DATA_DRIVEN": "data-driven",
    "NATIVE_PRIMITIVE_OR_VM": "native primitive / bounded VM",
    "JVM_BRIDGE_NEEDED": "JVM bridge needed",
    "UNSUPPORTED_NATIVE_LIB": "unsupported native lib",
    "UNKNOWN": "unknown",
    "REJECTED": "rejected (integrity)",
}


def render_text(report):
    lines = []
    ma = report["mode_a"]
    p = ma["parity"]
    lines.append("megu modloader parity audit "
                 f"({SCHEMA})")
    lines.append("=" * 64)
    lines.append("Mode A - Static Source Presence Coverage "
                 f"({ma['files_scanned']} files, {ma['bytes_scanned']} bytes)")
    lines.append("  counts literal evidence needles present in live source;"
                 " runtime behaviour and modpack compatibility are not measured")
    for dim in ma["dimensions"]:
        lines.append(f"  {dim['id']} {dim['name']}: "
                     f"{dim['covered']}/{dim['total']}")
    lines.append("")
    lines.append(f"  NeoForge Parity (static source presence): "
                 f"{p['neoforge']['covered']}/"
                 f"{p['neoforge']['total']} ({p['neoforge']['pct']}%)")
    lines.append(f"  Fabric Parity   (static source presence): "
                 f"{p['fabric']['covered']}/"
                 f"{p['fabric']['total']} ({p['fabric']['pct']}%)")
    lines.append(f"  Overall Modloader Parity (static source presence): "
                 f"{p['overall']['covered']}/"
                 f"{p['overall']['total']} ({p['overall']['pct']}%)")
    if p["gaps"]:
        lines.append("")
        lines.append(f"  GAP items ({len(p['gaps'])}):")
        cur = None
        for g in p["gaps"]:
            if g["dimension"] != cur:
                cur = g["dimension"]
                lines.append(f"    [{cur}]")
            lines.append(f"      - {g['id']}  "
                         f"(loaders: {','.join(g['loaders'])})")
    if "mode_b" in report:
        mb = report["mode_b"]
        lines.append("")
        lines.append("Mode B - static mod JAR inspection "
                     f"({mb['jars_total']} jar(s))")
        lines.append(f"  by_classification: {mb['by_classification']}")
        lines.append(f"  by_loader: {mb['by_loader']}")
        if mb["integrity_errors"]:
            lines.append(f"  INTEGRITY ERRORS ({len(mb['integrity_errors'])}):")
            for e in mb["integrity_errors"]:
                lines.append(f"    ! {e}")
        if mb["top_missing_primitives"]:
            lines.append("  Top missing generic primitives:")
            for t in mb["top_missing_primitives"]:
                lines.append(f"    {t['jars']:>3}x {t['primitive']}")
        lines.append("  Per-JAR:")
        for j in mb["jars"]:
            c = j["counts"]
            lines.append(
                f"    {j['file']}: loader={j['loader']} "
                f"env={j['environment']} -> {j['classification']} "
                f"({_CLASS_LABELS.get(j['classification'], j['classification'])})"
                + (f" [{'; '.join(j['reasons'][:3])}]"
                   if j.get("reasons") else ""))
            if c:
                lines.append(
                    f"        classes={c.get('class_files', 0)} "
                    f"mixin_targets={c.get('mixin_targets', 0)} "
                    f"at_lines={c.get('accesstransformer_lines', 0)} "
                    f"jarjar={c.get('jarjar_archives', 0)} "
                    f"native={c.get('native_libs', 0)} "
                    f"recipes={c.get('recipes', 0)} "
                    f"tags={c.get('tags', 0)} "
                    f"loot={c.get('loot_tables', 0)} "
                    f"worldgen={c.get('worldgen', 0)} "
                    f"nf_data_maps={c.get('neoforge_data_maps', 0)} "
                    f"nf_biome_mod={c.get('neoforge_biome_modifiers', 0)}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def build_parser():
    ap = argparse.ArgumentParser(
        description="megu NeoForge/Fabric 1.21.1 static source-presence "
                    "auditor "
                    f"({SCHEMA}). Mode A always runs offline and measures "
                    "literal source presence, not runtime or modpack "
                    "compatibility; Mode B inspects real mod JARs read-only.")
    ap.add_argument("--repo", default=".", help="repo root (default: .)")
    ap.add_argument("--mods-dir", default=None,
                    help="directory of mod .jar files to audit")
    ap.add_argument("--jar", action="append", default=None,
                    help="path to a mod .jar (repeatable)")
    ap.add_argument("--format", choices=["text", "json"], default="text")
    ap.add_argument("--strict", action="store_true",
                    help="strict gate: equivalent to --min-overall-pct 100; "
                         "any Mode B integrity error exits 1")
    ap.add_argument("--min-overall-pct", type=float, default=None,
                    help="exit 1 when Mode A static source presence (%%) is "
                         "below N")
    ap.add_argument("--out", default=None,
                    help="also write the report to this path")
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    repo = Path(args.repo).resolve()
    strict_mode = bool(args.strict) or args.min_overall_pct is not None
    if args.min_overall_pct is not None:
        threshold = args.min_overall_pct
    elif args.strict:
        threshold = 100.0
    else:
        threshold = 0.0
    try:
        # Validate Mode B inputs before the Mode A scan (fail fast).
        jar_paths = collect_jars(args)
        report = {"schema": SCHEMA, "repo": str(repo)}
        report.update(mode_a(repo))
        integrity_errors = []
        if jar_paths:
            mb, integrity_errors = mode_b(jar_paths)
            report.update(mb)
        pct = report["mode_a"]["parity"]["overall"]["pct"]
        if args.format == "json":
            rendered = json.dumps(report, indent=2, sort_keys=False) + "\n"
        else:
            rendered = render_text(report) + "\n"
    except AuditError as exc:
        print(f"modloader_parity_audit: error: {exc}", file=sys.stderr)
        return 2
    except Exception as exc:  # noqa: BLE001 - never crash silently
        print(f"modloader_parity_audit: internal error: {exc}",
              file=sys.stderr)
        return 2
    sys.stdout.write(rendered)
    if args.out:
        Path(args.out).write_text(rendered)

    if integrity_errors and not strict_mode:
        return 2
    if integrity_errors or pct < threshold:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
