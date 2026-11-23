#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Evidence-based parity scorer for the Minecraft 1.21.1 feature CSV.

Reads the external feature CSV (10,436 rows, 17 prefixes) and mechanically
scores every row against the live megu repository.  Scoring is honest by
construction:

* Rust sources are lexed so ``//`` / ``/* */`` comments and string/char
  literal *contents* cannot produce false evidence, and ``#[cfg(test)]``
  items (plus whole files reachable only through ``#[cfg(test)] mod x;``)
  are excluded from production evidence and kept as a separate test corpus.
* ``COVERED_DATA_OR_LIVE`` is only emitted when a dedicated evidence source
  (canonical table, asset JSON, vendored datapack file, or live code path)
  actually contains the row's registry name / command / packet / rule.
* Registry *references* without a dedicated implementation degrade to
  ``PARTIAL``; absent evidence is ``MISSING``; mechanics that need human
  verification are ``NEEDS_VERIFY``; config-file-only subsystems are
  ``MANUAL_MECH``.

Pure stdlib Python 3.  No cargo required.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Statuses
# ---------------------------------------------------------------------------

COVERED = "COVERED_DATA_OR_LIVE"
PARTIAL = "PARTIAL"
MISSING = "MISSING"
NEEDS_VERIFY = "NEEDS_VERIFY"
MANUAL_MECH = "MANUAL_MECH"
ALL_STATUSES = (COVERED, PARTIAL, MISSING, NEEDS_VERIFY, MANUAL_MECH)

DEFAULT_CSV = str(Path.home() / ".cache/megu-audit/minecraft-1.21.1-features.csv")
CSV_ENV = "MC_1211_FEATURES_CSV"

PREFIXES = (
    "MECH", "BLK", "ITEM", "ENT", "REG", "ENCH", "BIOME", "STRUCT", "DMG",
    "DATA", "TAG", "GR", "CMD", "PKT", "ADV", "RCP", "LOOT",
)

_WS = " \t\r"

# ---------------------------------------------------------------------------
# Rust lexer: comments -> spaces (clean), comments + literal bodies -> spaces
# (skel).  Both outputs are exactly the same length as the input so ranges
# computed on one apply to the other.
# ---------------------------------------------------------------------------


def _blank_range(out: list[str], text: str, lo: int, hi: int) -> None:
    for k in range(lo, hi):
        if text[k] != "\n":
            out[k] = " "


def lex_rust(text: str) -> tuple[str, str]:
    """Return (clean, skel): clean keeps literals, skel blanks literal bodies."""
    n = len(text)
    clean = list(text)
    skel = list(text)
    i = 0
    while i < n:
        c = text[i]
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            j = text.find("\n", i)
            if j < 0:
                j = n
            _blank_range(clean, text, i, j)
            _blank_range(skel, text, i, j)
            i = j
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if text[j] == "/" and j + 1 < n and text[j + 1] == "*":
                    depth += 1
                    j += 2
                elif text[j] == "*" and j + 1 < n and text[j + 1] == "/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            _blank_range(clean, text, i, min(j, n))
            _blank_range(skel, text, i, min(j, n))
            i = j
            continue
        # raw string: r".." r#".."# r##".."## br".." br#".."#
        if c == "r" or (c == "b" and i + 1 < n and text[i + 1] == "r"):
            base = i if c == "r" else i + 1
            j = base + 1
            hashes = 0
            while j < n and text[j] == "#":
                hashes += 1
                j += 1
            if j < n and text[j] == '"':
                term = '"' + "#" * hashes
                k = text.find(term, j + 1)
                end = n if k < 0 else k + len(term)
                for p in range(j + 1, min(k if k >= 0 else n, n)):
                    if text[p] != "\n":
                        skel[p] = " "
                i = end
                continue
        if c == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"' or text[j] == "\n":
                    break
                j += 1
            j = min(j + 1, n)
            for p in range(i + 1, min(j - 1, n)):
                if text[p] != "\n":
                    skel[p] = " "
            i = j
            continue
        if c == "'":
            lit_end = -1
            if i + 2 < n and text[i + 1] == "\\":
                k = i + 2
                if k < n and text[k] == "u" and k + 1 < n and text[k + 1] == "{":
                    e = text.find("}", k + 2)
                    if e >= 0 and e + 1 < n and text[e + 1] == "'":
                        lit_end = e + 2
                elif k + 2 < n and text[k] == "x":
                    if k + 3 < n and text[k + 3] == "'":
                        lit_end = k + 4
                elif k + 1 < n and text[k + 1] == "'":
                    lit_end = k + 2
            elif i + 2 < n and text[i + 2] == "'" and text[i + 1] != "\\":
                lit_end = i + 3
            if lit_end > 0:
                for p in range(i + 1, lit_end - 1):
                    if text[p] != "\n":
                        skel[p] = " "
                i = lit_end
                continue
            i += 1
            continue
        i += 1
    return "".join(clean), "".join(skel)


_STR_RE = re.compile(r'"((?:[^"\\\n]|\\.)*)"')
_MC_RE = re.compile(r"minecraft:[A-Za-z0-9_./+-]+")
_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_WORD_RE = re.compile(r"[a-z][a-z0-9_]*")


def _match_delim(skel: str, pos: int, open_ch: str, close_ch: str) -> int:
    """pos points at open_ch; return index just past matching close_ch."""
    depth = 0
    i = pos
    n = len(skel)
    while i < n:
        ch = skel[i]
        if ch == open_ch:
            depth += 1
        elif ch == close_ch:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def _item_end(skel: str, pos: int) -> int:
    """Given item start pos, return end (exclusive) of the item.

    Tracks paren/bracket depth separately from brace depth; the item ends at
    the first `;` outside any delimiter, or just past the first top-level
    `{...}` body once it closes.
    """
    n = len(skel)
    pd = 0
    bd = 0
    seen_brace = False
    i = pos
    while i < n:
        ch = skel[i]
        if ch in "([":
            pd += 1
        elif ch in ")]":
            if pd > 0:
                pd -= 1
        elif ch == "{":
            if pd == 0:
                bd += 1
                seen_brace = True
        elif ch == "}":
            if pd == 0 and bd > 0:
                bd -= 1
                if seen_brace and bd == 0:
                    return i + 1
        elif ch == ";" and pd == 0 and bd == 0:
            return i + 1
        i += 1
    return n


def strip_cfg_test(clean: str, skel: str) -> tuple[str, str, list[str]]:
    """Blank `#[cfg(test)]`-gated items; returns (clean, skel, removed_texts)."""
    c_out = list(clean)
    s_out = list(skel)
    removed: list[str] = []
    n = len(skel)
    pos = 0
    while True:
        m = re.search(r"#\s*\[\s*cfg\s*\(", skel[pos:])
        if not m:
            break
        attr_start = pos + m.start()
        p = pos + m.end() - 1  # at '('
        paren = 1
        q = p + 1
        while q < n and paren:
            if skel[q] == "(":
                paren += 1
            elif skel[q] == ")":
                paren -= 1
            q += 1
        inner = skel[p + 1 : q - 1]
        stripped_inner = re.sub(r"not\s*\([^()]*\)", "", inner)
        needs_test = (
            re.match(r"\s*(?:test\b|all\s*\(\s*test\b|any\s*\(\s*test\b)", inner)
            is not None
            and re.search(r"\btest\b", stripped_inner) is not None
        )
        j = q
        while j < n and skel[j] in _WS:
            j += 1
        if j >= n or skel[j] != "]":
            pos = q
            continue
        j += 1
        # skip additional attributes between cfg(test) and the item
        while True:
            k = j
            while k < n and skel[k] in _WS:
                k += 1
            if k + 1 < n and skel[k] == "#" and skel[k + 1] == "[":
                j = _match_delim(skel, k + 1, "[", "]")
            else:
                j = k
                break
        if not needs_test:
            pos = j
            continue
        end = _item_end(s_out, j)
        removed.append(clean[attr_start:end])
        _blank_range(c_out, clean, attr_start, end)
        _blank_range(s_out, skel, attr_start, end)
        pos = end
    return "".join(c_out), "".join(s_out), removed


class RustFile:
    __slots__ = ("rel", "code", "skel", "literals", "mc", "idents")

    def __init__(self, rel: str, code: str, skel: str):
        self.rel = rel
        self.code = code
        self.skel = skel
        self.literals = {m.group(1) for m in _STR_RE.finditer(code)}
        self.mc = set(_MC_RE.findall(code))
        self.idents = set(_IDENT_RE.findall(skel))


def scan_rust_tree(root: Path):
    """Scan *.rs under root.  Returns (files, test_files, skipped)."""
    files: dict[str, RustFile] = {}
    test_only: dict[str, RustFile] = {}
    skipped: list[str] = []
    rs_paths = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [
            d for d in dirnames if d not in ("target", ".git", "node_modules")
        ]
        for fn in filenames:
            if fn.endswith(".rs"):
                rs_paths.append(Path(dirpath) / fn)
    rs_paths.sort()
    raw: dict[Path, tuple[str, str, list[str]]] = {}
    for p in rs_paths:
        rel = p.relative_to(root).as_posix()
        try:
            text = p.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            skipped.append(f"{rel}: {exc}")
            continue
        clean, skel = lex_rust(text)
        clean, skel, removed = strip_cfg_test(clean, skel)
        raw[p] = (clean, skel, removed)
    # files reachable only via `#[cfg(test)] mod x;` are test-only
    test_mods: set[tuple[Path, str]] = set()
    for p, (_clean, _skel, removed) in raw.items():
        for snippet in removed:
            for mm in re.finditer(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;", snippet):
                test_mods.add((p.parent, mm.group(1)))
    test_file_paths: set[Path] = set()
    for parent, name in test_mods:
        for cand in (parent / f"{name}.rs", parent / name / "mod.rs"):
            if cand in raw:
                test_file_paths.add(cand)
    for p, (clean, skel, _removed) in raw.items():
        rel = p.relative_to(root).as_posix()
        rf = RustFile(rel, clean, skel)
        if p in test_file_paths:
            test_only[rel] = rf
        else:
            files[rel] = rf
    return files, test_only, skipped


# ---------------------------------------------------------------------------
# Small extraction helpers
# ---------------------------------------------------------------------------


def find_file(files: dict[str, RustFile], suffix: str) -> RustFile | None:
    if suffix in files:
        return files[suffix]
    for rel, rf in files.items():
        if rel.endswith("/" + suffix):
            return rf
    return None


def files_matching(files: dict[str, RustFile], *needles: str) -> list[RustFile]:
    out = []
    for rel, rf in files.items():
        low = rel.lower()
        if any(nd.lower() in low for nd in needles):
            out.append(rf)
    return out


def fn_span(rf: RustFile, name: str) -> tuple[int, int] | None:
    """Locate `fn name` and return (start, end) covering attributes + body."""
    m = re.search(r"\bfn\s+" + re.escape(name) + r"\b", rf.skel)
    if not m:
        return None
    start = m.start()
    line_start = rf.skel.rfind("\n", 0, start) + 1
    while line_start > 0:
        prev_end = rf.skel.rfind("\n", 0, line_start - 1) + 1
        prev = rf.skel[prev_end:line_start].strip()
        if prev.startswith("#") or prev == "":
            line_start = prev_end
        else:
            break
    end = _item_end(rf.skel, start)
    return line_start, end


def fn_text(rf: RustFile, name: str) -> str:
    span = fn_span(rf, name)
    if not span:
        return ""
    return rf.code[span[0] : span[1]]


def section_text(rf: RustFile, marker: str, limit: int = 800_000) -> str:
    idx = rf.code.find(marker)
    if idx < 0:
        return ""
    return rf.code[idx : idx + limit]


def array_section(rf: RustFile, marker: str) -> str:
    """Text of a `[...]` literal array following `marker` (a `NAME:` decl)."""
    sec = section_text(rf, marker)
    if not sec:
        return ""
    end = sec.find("\n];")
    if end < 0:
        end = sec.find("];")
    if end < 0:
        end = min(len(sec), 500_000)
    return sec[: end + 2]


def literals_in(text: str) -> list[str]:
    return [m.group(1) for m in _STR_RE.finditer(text)]


def bare(name: str) -> str:
    return name.split(":", 1)[1] if ":" in name else name


# ---------------------------------------------------------------------------
# Evidence collection
# ---------------------------------------------------------------------------


class Evidence:
    def __init__(self, root: Path):
        self.root = root
        self.skipped: list[str] = []
        self.files: dict[str, RustFile] = {}
        self.test_files: dict[str, RustFile] = {}
        self.py_tests: list[str] = []
        self._blob_cache: dict[tuple[str, ...], str] = {}
        self._mech_cache: dict[str, tuple[set[str], int]] = {}
        self._load()
        self._extract()

    # -- loading ---------------------------------------------------------
    def _load(self) -> None:
        self.files, self.test_files, self.skipped = scan_rust_tree(self.root)
        self.test_tokens: set[str] = set()
        for rf in self.test_files.values():
            self.test_tokens.update(_WORD_RE.findall(rf.code.lower()))
            for lit in rf.literals:
                self.test_tokens.update(_WORD_RE.findall(lit.lower()))
        tdir = self.root / "tests"
        if tdir.is_dir():
            for p in sorted(tdir.rglob("*.py")):
                try:
                    txt = p.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    continue
                self.py_tests.append(txt)
                self.test_tokens.update(_WORD_RE.findall(txt.lower()))

    def blob(self, *needles: str) -> str:
        """Cached concatenation of production code for files matching needles."""
        key = tuple(sorted(needles))
        if key not in self._blob_cache:
            self._blob_cache[key] = "\n".join(
                rf.code for rf in files_matching(self.files, *needles)
            )
        return self._blob_cache[key]

    def all_code(self) -> str:
        return self.blob(".rs") if False else self._all_blob()

    def _all_blob(self) -> str:
        if not hasattr(self, "_all_code_cache"):
            self._all_code_cache = "\n".join(rf.code for rf in self.files.values())
        return self._all_code_cache

    # -- datapack / asset helpers ----------------------------------------
    def dp_path(self, *parts: str) -> Path:
        return self.root.joinpath("datapacks", "vanilla", "data", "minecraft", *parts)

    def dp_json_exists(self, *parts: str) -> bool:
        return self.dp_path(*parts).with_suffix(".json").is_file()

    def load_json(self, path: Path):
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None

    def blocks_asset(self) -> dict[str, dict]:
        if hasattr(self, "_blocks_asset"):
            return self._blocks_asset
        data = self.load_json(self.root / "assets" / "blocks_1211.json")
        self._blocks_asset = {}
        if isinstance(data, list):
            for b in data:
                if isinstance(b, dict) and "name" in b:
                    self._blocks_asset[b["name"]] = b
        self._blocks_asset_names = set(self._blocks_asset)
        return self._blocks_asset

    def block_states_names(self) -> set[str]:
        if hasattr(self, "_block_states_names"):
            return self._block_states_names
        data = self.load_json(self.root / "assets" / "block_states_1211.json")
        names: set[str] = set()
        if isinstance(data, dict):
            mc = data.get("minecraft", data)
            if isinstance(mc, dict):
                skip = {"format", "jar_sha1", "generated_by", "about"}
                for k, v in mc.items():
                    if k not in skip and isinstance(v, (dict, list)):
                        names.add(k)
        self._block_states_names = names
        return names

    def worldgen_type_values(self) -> set[str]:
        """All `"type"/"processor_type": "minecraft:X"` values in worldgen JSONs."""
        if hasattr(self, "_wg_types"):
            return self._wg_types
        base = self.dp_path("worldgen")
        self._wg_types = set()
        type_re = re.compile(
            r'"(?:type|processor_type)"\s*:\s*"minecraft:([a-z0-9_/-]+)"'
        )
        if base.is_dir():
            for p in base.rglob("*.json"):
                try:
                    text = p.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    continue
                self._wg_types.update(type_re.findall(text))
        return self._wg_types

    # -- canonical protocol tables ---------------------------------------
    def _v767(self, rf: RustFile):
        self.v767_packets: dict[tuple[str, str, int], str] = {}
        pkt_re = re.compile(
            r"\(\s*PacketPhase::(\w+)\s*,\s*PacketDir::(\w+)\s*,\s*"
            r'"([^"]+)"\s*,\s*(0x[0-9a-fA-F]+|\d+)\s*\)'
        )
        for m in pkt_re.finditer(rf.code):
            phase, direction, name, idraw = m.groups()
            pid = int(idraw, 16) if idraw.lower().startswith("0x") else int(idraw)
            self.v767_packets[(phase.lower(), direction.lower(), pid)] = name
        for const, attr in (
            ("V767_BLOCKS:", "v767_blocks"),
            ("V767_ITEMS:", "v767_items"),
            ("V767_ENTITY_TYPES:", "v767_entities"),
            ("V767_MENUS:", "v767_menus"),
            ("V767_COMPONENTS:", "v767_components"),
        ):
            sec = array_section(rf, const)
            setattr(self, attr, {bare(x) for x in literals_in(sec) if x})

    # -- registry name tables --------------------------------------------
    def _tables(self):
        mi = find_file(self.files, "src/menus_and_items.rs")
        er = find_file(self.files, "src/entity_registry.rs")
        ef = find_file(self.files, "src/effects.rs")
        wm = find_file(self.files, "src/world_mechanisms.rs")
        cc = find_file(self.files, "src/commands_core.rs")
        pm = find_file(self.files, "src/player_mechanisms.rs")
        rs = find_file(self.files, "src/recipes_and_stats.rs")
        ws = find_file(self.files, "src/workstations_and_mounts.rs")
        tc = find_file(self.files, "src/teams_and_commands.rs")
        pp = find_file(self.files, "src/play_protocol.rs")
        be = find_file(self.files, "src/block_entity.rs")
        vr = find_file(self.files, "src/villager_raid.rs")
        ch = find_file(self.files, "src/combat_and_health.rs")
        dp = find_file(self.files, "src/datapack.rs")
        wg = find_file(self.files, "src/worldgen.rs")
        main = find_file(self.files, "src/main.rs")
        sv = find_file(self.files, "src/save_v2.rs")
        an = find_file(self.files, "src/anvil_io.rs")

        # items
        self.item_names: set[str] = set()
        if mi:
            self.item_names = {
                bare(x) for x in literals_in(array_section(mi, "ITEM_NAMES:")) if x
            }
        # entities
        self.entity_names: set[str] = set()
        if er:
            self.entity_names = set(
                re.findall(r'entity_spec_1211!\(\s*\d+\s*,\s*"([^"]+)"', er.code)
            )
        # mob effects
        self.effect_names: set[str] = set()
        if ef:
            self.effect_names = {
                bare(x)
                for x in literals_in(array_section(ef, "EFFECT_REGISTRY:"))
                if x
            }
        # enchantment ids
        self.enchant_names: set[str] = set()
        if mi:
            self.enchant_names = {
                bare(x)
                for x in literals_in(fn_text(mi, "enchant_name"))
                if x.startswith("minecraft:")
            }
        # banner patterns
        self.banner_patterns: set[str] = set()
        if mi:
            self.banner_patterns = {
                bare(x)
                for x in literals_in(array_section(mi, "BANNER_PATTERN_REGISTRY:"))
                if x
            }
        # trim patterns / materials
        self.trim_patterns: set[str] = set()
        self.trim_materials: set[str] = set()
        for rf in (pm, mi):
            if rf is None:
                continue
            for marker, attr in (
                ("TRIM_PATTERNS:", "trim_patterns"),
                ("TRIM_MATERIAL_ITEMS:", "trim_materials"),
            ):
                for x in literals_in(array_section(rf, marker)):
                    if x:
                        getattr(self, attr).add(bare(x))
        # live potions
        self.potion_names: set[str] = set()
        if mi:
            sec = array_section(mi, "LIVE_POTIONS:")
            self.potion_names = {
                m.group(1)
                for m in re.finditer(r'\(\s*"([a-z0-9_]+)"\s*,', sec)
            }
        # advancements
        self.advancements: set[str] = set()
        if wm:
            sec = array_section(wm, "CORE_ADVANCEMENTS:")
            self.advancements = {
                x for x in literals_in(sec) if x.startswith("minecraft:")
            }
        # gamerules: defaults table + atomic-backed live names
        self.gamerule_defaults: set[str] = set()
        self.gamerule_live: set[str] = set()
        for rf in (cc, main, sv, an):
            if rf is None:
                continue
            sec = section_text(rf, "COMMAND_GAMERULE_DEFAULTS:")
            self.gamerule_defaults.update(
                m.group(1)
                for m in re.finditer(r'\(\s*"([A-Za-z_][A-Za-z0-9_]*)"\s*,', sec)
            )
            for fn in ("gamerule_atomic_value", "gamerule_atomic_store", "gamerule_value"):
                for x in literals_in(fn_text(rf, fn)):
                    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", x):
                        self.gamerule_live.add(x)
        self.gamerule_live -= {"true", "false", "none", "name", "value"}
        # damage types
        self.damage_types: set[str] = set()
        if tc:
            body = fn_text(tc, "damage_type_known")
            self.damage_types = {
                x
                for x in literals_in(body)
                if re.fullmatch(r"[a-z0-9_]+", x)
            }
        # attributes
        self.attribute_names: set[str] = set()
        if ch:
            for fn in ("attr_key_name", "attr_key_id"):
                for x in literals_in(fn_text(ch, fn)):
                    if x:
                        self.attribute_names.add(bare(x))
        # particles
        self.particle_names: set[str] = set()
        if pp:
            self.particle_names = {
                m.group(1)
                for m in re.finditer(
                    r'\(\s*"([a-z0-9_]+)"\s*,', fn_text(pp, "particle_id")
                )
            }
        # block entity types
        self.be_type_names: set[str] = set()
        for rf, fns in (
            (mi, ("live_block_entity_id",)),
            (pp, ("live_block_entity_wire_id",)),
        ):
            if rf is None:
                continue
            for fn in fns:
                for x in literals_in(fn_text(rf, fn)):
                    if x.startswith("minecraft:"):
                        self.be_type_names.add(bare(x))
        if pp and "sign_entity_pkt" in pp.code:
            self.be_type_names.add("sign")
        if find_file(self.files, "src/generated_content.rs") and find_file(
            self.files, "src/block_entity.rs"
        ):
            self.be_type_names.add("mob_spawner")
        # stat categories
        self.stat_categories: set[str] = set()
        if rs:
            m = re.search(r"enum\s+StatCategory\s*\{([^}]*)\}", rs.skel)
            if m:
                body = rs.code[m.start(1) : m.end(1)]
                self.stat_categories = {
                    re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", v.strip()).lower()
                    for v in body.split(",")
                    if v.strip() and not v.strip().startswith("//")
                }
        # villager professions
        self.professions: set[str] = set()
        if vr:
            m = re.search(r"enum\s+VillagerProfession\s*\{([^}]*)\}", vr.skel)
            if m:
                body = vr.code[m.start(1) : m.end(1)]
                self.professions = {
                    re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", v.strip()).lower()
                    for v in body.split(",")
                    if v.strip() and not v.strip().startswith("//")
                }
        # recipe serializers
        self.recipe_serializers: set[str] = set()
        if rs:
            self.recipe_serializers = {
                bare(x)
                for x in literals_in(array_section(rs, "RBS_RECIPE_SERIALIZERS:"))
                if x
            }
        # smelt pairs + campfire outputs
        self.smelt_outputs: set[str] = set()
        self.campfire_outputs: set[str] = set()
        if mi:
            for _a, b in re.findall(
                r'\(\s*"([a-z0-9_]+)"\s*,\s*"([a-z0-9_]+)"\s*\)',
                fn_text(mi, "smelt_result"),
            ):
                self.smelt_outputs.add(b)
            for m in re.finditer(
                r'"([a-z0-9_]+)"\s*=>\s*"([a-z0-9_]+)"',
                fn_text(mi, "live_campfire_result"),
            ):
                self.campfire_outputs.add(m.group(2))
        # stonecut outputs (incl. generated copper shapes)
        self.stonecut_outputs: set[str] = set()
        if ws:
            sec = array_section(ws, "WM_STONECUT:")
            for m in re.finditer(r'\(\s*"([a-z0-9_]+)"\s*,\s*\d+\s*\)', sec):
                self.stonecut_outputs.add(m.group(1))
            psec = section_text(ws, "WM_COPPER_PREFIXES:")
            prefixes = re.findall(r'"([a-z0-9_]+)"', psec)
            for p in prefixes:
                for suf in (
                    "chiseled_copper",
                    "cut_copper",
                    "cut_copper_slab",
                    "cut_copper_stairs",
                ):
                    self.stonecut_outputs.add(f"{p}{suf}")
        # craft results from hardcoded tables (+ format! template expansion)
        self.craft_results: set[str] = set()
        self.craft_templates: set[str] = set()
        if mi:
            for fn in ("craft_result", "craft3_result", "live_crafter_recipe"):
                body = fn_text(mi, fn)
                if not body:
                    continue
                self.craft_results.update(
                    re.findall(r'name_to_item\(\s*"([a-z0-9_]+)"', body)
                )
                prefixes = set(
                    re.findall(r'\(\s*"[a-z0-9_]+"\s*,\s*"([a-z0-9_]+)"\s*\)', body)
                )
                for m in re.finditer(r'name_to_item\(\s*&?format!\(\s*"([^"]+)"', body):
                    tpl = m.group(1)
                    if "{}" in tpl:
                        self.craft_templates.add(tpl)
                        for p in prefixes:
                            self.craft_results.add(tpl.replace("{}", p))
                    else:
                        self.craft_results.add(tpl)
        # loot condition/function/entry dispatch names
        self.loot_conditions: set[str] = set()
        self.loot_functions: set[str] = set()
        self.loot_entries: set[str] = set()
        if dp:
            for fn, attr in (
                ("loot_condition", "loot_conditions"),
                ("loot_function", "loot_functions"),
                ("loot_entry_node", "loot_entries"),
            ):
                body = fn_text(dp, fn)
                arm_names: set[str] = set()
                for am in re.finditer(
                    r'((?:"[a-z0-9_]+"\s*(?:\|\s*)?)+)\s*=>', body
                ):
                    arm_names.update(re.findall(r'"([a-z0-9_]+)"', am.group(1)))
                setattr(self, attr, arm_names)
        # command names + token corpus
        self.command_names: set[str] = set()
        self.command_tokens: set[str] = set()
        ce = find_file(self.files, "src/commands_execute.rs")
        cmd_files = files_matching(
            self.files,
            "commands_core",
            "commands_execute",
            "brigadier",
            "teams_and_commands",
            "datapack",
            "operator_ux",
            "server_ops",
        )
        for rf in cmd_files:
            for lit in rf.literals:
                if re.fullmatch(r"[a-zA-Z_][a-zA-Z0-9_-]*", lit):
                    self.command_tokens.add(lit.lower())
            self.command_tokens.update(
                w.lower() for w in rf.idents if w.islower() or "_" in w
            )
        if cc:
            # largest `match cmd {` block = the dispatch table
            best: tuple[int, str] = (0, "")
            for mm in re.finditer(r"match\s+cmd\s*\{", cc.skel):
                end = _match_delim(cc.skel, mm.end() - 1, "{", "}")
                body = cc.code[mm.start() : end]
                arms = len(re.findall(r'"[a-z_]+"\s*=>', body))
                if arms > best[0]:
                    best = (arms, body)
            for am in re.finditer(
                r'((?:"[a-z_]+"\s*(?:\|\s*)?)+)\s*=>', best[1]
            ):
                self.command_names.update(re.findall(r'"([a-z_]+)"', am.group(1)))
            for am in re.finditer(
                r'((?:"[a-z_]+"\s*(?:\|\s*)?)+)\s*=>',
                fn_text(cc, "command_usage"),
            ):
                self.command_names.update(re.findall(r'"([a-z_]+)"', am.group(1)))
            self.command_names.update(
                m.group(1)
                for m in re.finditer(r'(?:literal|done)\(\s*"([a-z_]+)"', cc.code)
            )
        # biomes
        self.biome_names: set[str] = set()
        for rf in (dp, wg):
            if rf is None:
                continue
            for marker in ("VANILLA_BIOMES:", "BIOME64:"):
                self.biome_names.update(
                    bare(x)
                    for x in literals_in(array_section(rf, marker))
                    if x
                )
        # packet handling evidence (computed once)
        self.codec_names: set[str] = set()
        codec = find_file(self.files, "proto/src/codec.rs")
        if codec:
            self.codec_names = {
                x for x in codec.literals if re.fullmatch(r"[a-z0-9_]+", x)
            }
        self.client_pkt_names: set[str] = set()
        client_proto = find_file(self.files, "client-app/src/protocol.rs")
        if client_proto:
            for am in re.finditer(
                r'((?:"[a-z0-9_]+"\s*(?:\|\s*)?)+)\s*=>', client_proto.code
            ):
                self.client_pkt_names.update(
                    re.findall(r'"([a-z0-9_]+)"', am.group(1))
                )
        self.pkt_c2s_ids: set[int] = set()
        self.pkt_s2c_ids: set[int] = set()
        if pp:
            for mm in re.finditer(r"\bmatch\s+pid\b", pp.skel):
                end = _item_end(pp.skel, mm.start())
                body = pp.code[mm.start() : end]
                for am in re.finditer(r"0x([0-9a-fA-F]{2})", body):
                    self.pkt_c2s_ids.add(int(am.group(1), 16))
            for mm in re.finditer(
                r"(?:send_packet|recv_packet|handle_packet)\([^)]*?0x([0-9a-fA-F]{2})",
                pp.code,
            ):
                self.pkt_s2c_ids.add(int(mm.group(1), 16))
        # live loot tables: dedicated loot-generator fns only
        self.live_loot_tables: set[str] = set()
        self.referenced_loot: set[str] = set()
        fn_owner: dict[str, RustFile] = {}
        for rf in self.files.values():
            for m in re.finditer(r"\bfn\s+([a-z_][a-z0-9_]*)\s*\(", rf.skel):
                fn_owner.setdefault(m.group(1), rf)
        loot_top = {
            "chests", "blocks", "entities", "archaeology", "gameplay", "pots",
            "shearing", "spawners", "equipment", "dispensers",
        }
        for rf in self.files.values():
            for x in rf.mc:
                if "/" not in x:
                    continue
                path = x.split(":", 1)[1]
                top = path.split("/", 1)[0]
                if top not in loot_top:
                    continue
                self.referenced_loot.add(path)
                base = path.rsplit("/", 1)[-1]
                for cand in (base, f"{base}_loot", f"loot_{base}"):
                    owner = fn_owner.get(cand)
                    if owner is not None:
                        body = fn_text(owner, cand)
                        if re.search(
                            r"LootEntry|LootDrop|loot_entries|roll_loot|chest_loot",
                            body,
                        ):
                            self.live_loot_tables.add(path)
                        break
        # POI kinds
        self.poi_names: set[str] = {"home", "meeting"} | set(self.professions)
        # fluids
        self.fluid_names: set[str] = set()
        ff = find_file(self.files, "src/flowing_fluid.rs")
        if ff:
            self.fluid_names = {
                x
                for x in literals_in(ff.code)
                if x in {"water", "lava", "empty", "flowing_water", "flowing_lava"}
            }
        self.dimension_names: set[str] = {"overworld", "the_nether", "the_end"}
        self.chat_types: set[str] = {"chat"}
        # jukebox songs: disc item -> song id is `music_disc_` prefix strip
        self.jukebox_songs: set[str] = set()
        if be:
            for x in literals_in(fn_text(be, "jukebox_comparator_output")):
                if x.startswith("music_disc_"):
                    self.jukebox_songs.add(x[len("music_disc_"):])

    # -- extraction driver -------------------------------------------------
    def _extract(self) -> None:
        proto = find_file(self.files, "proto/src/data_v767.rs")
        self.v767_packets = {}
        self.v767_blocks = set()
        self.v767_items = set()
        self.v767_entities = set()
        self.v767_menus = set()
        self.v767_components = set()
        if proto:
            self._v767(proto)
        self._tables()
        self.blocks_asset()


# ---------------------------------------------------------------------------
# Row helpers
# ---------------------------------------------------------------------------


def row_prefix(row: dict) -> str:
    return row["id"].split("-", 1)[0]


def element_name(row: dict) -> str:
    el = row["element"].strip()
    return el.split(" (", 1)[0].strip()


def mc_path(row: dict) -> str:
    return element_name(row)


def bare_name(row: dict) -> str:
    return bare(mc_path(row))


def result_name(row: dict) -> str:
    m = re.search(r"result=(minecraft:[a-z0-9_]+)", row["behavior"])
    if m:
        return bare(m.group(1))
    return bare_name(row)


# ---------------------------------------------------------------------------
# Per-prefix scorers -> (status, [evidence])
# ---------------------------------------------------------------------------


def score_blk(row, ev):
    name = bare_name(row)
    e = []
    if name in getattr(ev, "_blocks_asset_names", set()):
        e.append("blocks_asset")
    if name in ev.block_states_names():
        e.append("block_states")
    if name in ev.v767_blocks:
        e.append("v767_blocks")
    if "blocks_asset" in e and ("block_states" in e or "v767_blocks" in e):
        return COVERED, e
    if e:
        return PARTIAL, e
    return MISSING, ["no_block_evidence"]


def score_item(row, ev):
    name = bare_name(row)
    e = []
    if name in ev.item_names:
        e.append("item_names")
    if name in ev.v767_items:
        e.append("v767_items")
    if "item_names" in e and "v767_items" in e:
        return COVERED, e
    if e:
        return PARTIAL, e
    return MISSING, ["no_item_evidence"]


def score_ent(row, ev):
    name = bare_name(row)
    e = []
    if name in ev.entity_names:
        e.append("entity_registry")
    if name in ev.v767_entities:
        e.append("v767_entities")
    if "entity_registry" in e and "v767_entities" in e:
        return COVERED, e
    if e:
        return PARTIAL, e
    return MISSING, ["no_entity_evidence"]


def score_biome(row, ev):
    name = bare_name(row)
    e = []
    if ev.dp_json_exists("worldgen", "biome", f"{name}.json"):
        e.append("datapack:worldgen/biome")
    if name in ev.biome_names:
        e.append("biome_registry")
    if "datapack:worldgen/biome" in e and "biome_registry" in e:
        return COVERED, e
    if e:
        return PARTIAL, e
    return MISSING, ["no_biome_evidence"]


def score_struct(row, ev):
    name = bare_name(row)
    e = []
    jpath = ev.dp_path("worldgen", "structure", f"{name}.json")
    stype = ""
    m = re.search(r"type=(minecraft:[a-z0-9_]+)", row["behavior"])
    if m:
        stype = bare(m.group(1))
    if jpath.is_file():
        e.append("datapack:worldgen/structure")
        data = ev.load_json(jpath)
        if isinstance(data, dict) and data.get("type"):
            e.append(f"structure_type={bare(str(data['type']))}")
    pipe = ev.blob("structure_pipeline", "jigsaw", "structure", "worldgen")
    if stype and (f'"minecraft:{stype}"' in pipe or f'"{stype}"' in pipe):
        e.append("pipeline_type_ref")
    if ev.dp_path("structure", name).is_dir():
        e.append("structure_templates")
    if "datapack:worldgen/structure" in e and (
        "pipeline_type_ref" in e or "structure_templates" in e
    ):
        return COVERED, e
    if e:
        return PARTIAL, e
    return MISSING, ["no_structure_evidence"]


def score_dmg(row, ev):
    name = bare_name(row)
    e = []
    if name in ev.damage_types:
        e.append("damage_type_known")
    if ev.dp_json_exists("damage_type", f"{name}.json"):
        e.append("datapack:damage_type")
    probe = ev.blob("combat_and_health", "teams_and_commands", "datapack")
    if f'"minecraft:{name}"' in probe or (
        (len(name) >= 5 or "_" in name) and f'"{name}"' in probe
    ):
        e.append("code_ref")
    if "damage_type_known" in e:
        return COVERED, e
    if e:
        return PARTIAL, e
    return MISSING, ["no_damage_type_evidence"]


def score_ench(row, ev):
    name = bare_name(row)
    sub = row["subcategory"]
    e = []
    if sub == "definition":
        if name in ev.enchant_names:
            e.append("enchant_name")
        probe = ev.blob("menus_and_items", "anvil", "enchant", "commands_core")
        if f'"minecraft:{name}"' in probe or f'"{name}"' in probe:
            e.append("code_ref")
        if "enchant_name" in e:
            return COVERED, e
        if e:
            return PARTIAL, e
        return MISSING, ["no_enchantment_evidence"]
    # effect rows: component/effect-type keys listed in behavior
    crit = row["behavior"].split("criteria:")[-1]
    keys = [
        k
        for k in re.findall(r"([a-z_]+)\s*:", row["behavior"])
        if k
        not in {
            "type", "amount", "base", "per_level_above_first", "value",
            "effect", "affected", "damage_type", "source", "target",
        }
    ]
    probe = ev.blob("enchant", "menus_and_items", "combat_and_health", "datapack")
    found = [k for k in set(keys) if f'"{k}"' in probe]
    e.extend(f"effect_key:{k}" for k in found)
    if name not in ev.enchant_names:
        return MISSING, ["enchantment_unknown"] + e
    if keys and len(found) == len(set(keys)):
        return COVERED, e or ["enchant_name"]
    if found:
        return PARTIAL, e
    return PARTIAL, ["enchant_id_only"] + e


def score_adv(row, ev):
    adv_id = mc_path(row)  # minecraft:story/cure_zombie_villager
    e = []
    if adv_id in ev.advancements:
        e.append("core_advancements")
    # criteria section only: `name=trigger, name2=trigger2; frame=...`
    crit = ""
    if "criteria:" in row["behavior"]:
        crit = row["behavior"].split("criteria:", 1)[1].split(";", 1)[0]
    all_trigs = set(re.findall(r"=\s*([a-z0-9_]+)", crit))
    wm_blob = ev.blob("world_mechanisms", "advancement", "trigger", "teams_and_commands")
    known = [
        t
        for t in all_trigs
        if f'"{t}"' in wm_blob or f'"minecraft:{t}"' in wm_blob
    ]
    e.extend(f"trigger:{t}" for t in sorted(known))
    if "core_advancements" not in e:
        probe = ev.blob("world_mechanisms", "advancement")
        if f'"{adv_id}"' in probe:
            e.append("code_ref")
            return PARTIAL, e
        return MISSING, ["advancement_absent"]
    if all_trigs and len(known) == len(all_trigs):
        return COVERED, e
    return PARTIAL, e


def score_data(row, ev):
    name = bare_name(row)
    sub = row["subcategory"]
    e = []
    covered_set = None
    probe_dirs: tuple[str, ...] = ()
    if sub == "banner_pattern":
        covered_set = ev.banner_patterns
        probe_dirs = ("workstations_and_mounts", "menus_and_items")
    elif sub == "jukebox_song":
        covered_set = ev.jukebox_songs
        probe_dirs = ("block_entity", "menus_and_items", "play_protocol")
    elif sub == "painting_variant":
        covered_set = set()
        probe_dirs = ("entity", "painting", "variant", "play_protocol")
    elif sub == "trim_material":
        covered_set = ev.trim_materials
        probe_dirs = ("player_mechanisms", "menus_and_items")
    elif sub == "trim_pattern":
        covered_set = ev.trim_patterns
        probe_dirs = ("player_mechanisms", "menus_and_items")
    elif sub == "wolf_variant":
        covered_set = set()
        probe_dirs = ("entity", "wolf", "variant", "natural_spawner")
    if covered_set is not None and name in covered_set:
        e.append(f"{sub}_registry")
        return COVERED, e
    blob = ev.blob(*probe_dirs)
    if f'"minecraft:{name}"' in blob or f'"{name}"' in blob:
        e.append("code_ref")
        return PARTIAL, e
    return MISSING, [f"no_{sub}_evidence"]


def score_tag(row, ev):
    sub = row["subcategory"]  # tag/<dir>
    tag_dir = sub[4:] if sub.startswith("tag/") else sub
    el = element_name(row)
    rel = el[1:] if el.startswith("#") else el
    e = []
    jfile = ev.dp_path("tags", tag_dir, *rel.split("/")).with_suffix(".json")
    if jfile.is_file():
        e.append("datapack:tag")
        consumers = {
            "block": ("block_registry", "block_state"),
            "item": ("menus_and_items", "craft", "recipe", "tag_"),
            "entity_type": ("entity", "spawner", "natural_spawner"),
            "worldgen/biome": ("worldgen", "biome", "datapack"),
            "enchantment": ("enchant", "anvil"),
            "damage_type": ("damage", "combat"),
            "fluid": ("fluid",),
            "cat_variant": ("cat", "variant"),
            "painting_variant": ("painting", "variant"),
            "instrument": ("instrument", "note_block"),
            "banner_pattern": ("banner", "pattern"),
            "game_event": ("game_event", "vibration", "sculk"),
            "point_of_interest_type": ("poi", "villager", "point_of_interest"),
        }
        needles = consumers.get(tag_dir)
        if needles:
            blob = ev.blob(*needles)
            if blob:
                e.append("tag_consumer")
                return COVERED, e
        return PARTIAL, e
    probe = ev.blob("datapack", "block_registry", "menus_and_items")
    if f'"{el}"' in probe or f'"{rel}"' in probe:
        e.append("code_ref")
        return PARTIAL, e
    return MISSING, ["no_tag_file"]


def score_gr(row, ev):
    name = element_name(row)
    e = []
    if name in ev.gamerule_live:
        e.append("gamerule_atomic")
        return COVERED, e
    if name in ev.gamerule_defaults:
        e.append("gamerule_default_only")
        return PARTIAL, e
    probe = ev.blob("commands_core", "main", "save_v2", "anvil_io")
    if f'"{name}"' in probe:
        e.append("code_ref")
        return PARTIAL, e
    return MISSING, ["unknown_gamerule"]


_CMD_TOKEN_STOP = {
    "true", "false", "null", "none", "all", "any", "the", "a", "an",
}


def score_cmd(row, ev):
    sub = row["subcategory"]  # /root
    root = sub.lstrip("/")
    el = element_name(row)
    body = el[len(sub) :].strip() if el.startswith(sub) else el
    body = re.sub(r"<[^>]*>", " ", body).replace("→", " ")
    tokens = [
        t.lower()
        for t in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", body)
        if t.lower() not in _CMD_TOKEN_STOP and len(t) >= 2
    ]
    e = []
    if root not in ev.command_names:
        probe = ev.blob("commands_core", "commands_execute")
        if f'"{root}"' in probe:
            e.append("root_literal_only")
            return PARTIAL, e
        return MISSING, [f"root_{root}_absent"]
    e.append("root_implemented")
    if not tokens:
        return COVERED, e
    missing = sorted({t for t in tokens if t not in ev.command_tokens})
    if not missing:
        e.append("subcommands_verified")
        return COVERED, e
    e.append("missing_tokens:" + ",".join(missing[:6]))
    return PARTIAL, e


_PKG_PHASE = {
    "handshaking": "handshake",
    "status": "status",
    "login": "login",
    "configuration": "configuration",
    "play": "play",
}


def score_pkt(row, ev):
    sub = row["subcategory"]  # play/S→C
    phase_s, _, dir_s = sub.partition("/")
    phase = _PKG_PHASE.get(phase_s.strip(), phase_s.strip())
    direction = "serverbound" if dir_s.strip() == "C→S" else "clientbound"
    el = element_name(row)
    m = re.match(r"(0x[0-9a-fA-F]+|\d+)\s+([a-z0-9_]+)", el)
    if not m:
        return MISSING, ["unparseable_packet_row"]
    pid = (
        int(m.group(1), 16)
        if m.group(1).lower().startswith("0x")
        else int(m.group(1))
    )
    csv_name = m.group(2)
    canon = ev.v767_packets.get((phase, direction, pid))
    e = []
    if canon is None:
        return MISSING, ["not_in_v767_table"]
    e.append("v767_table")
    live_names = ev.codec_names | ev.client_pkt_names
    if canon in live_names or csv_name in live_names:
        e.append("codec_or_client")
        return COVERED, e
    if direction == "serverbound":
        if pid in ev.pkt_c2s_ids:
            e.append("server_c2s_handler")
            return COVERED, e
    else:
        if pid in ev.pkt_s2c_ids:
            e.append("server_s2c_send")
            return COVERED, e
    return PARTIAL, e


def score_reg(row, ev):
    name = bare_name(row)
    sub = row["subcategory"]
    e = []

    def hit(names, tag):
        if name in names:
            e.append(tag)
            return (COVERED, e)
        return None

    r = None
    if sub == "mob_effect":
        r = hit(ev.effect_names, "effect_registry")
    elif sub == "potion":
        r = hit(ev.potion_names, "live_potions")
    elif sub == "particle_type":
        r = hit(ev.particle_names, "particle_id")
    elif sub == "menu":
        r = hit(ev.v767_menus, "v767_menus")
    elif sub == "data_component_type":
        r = hit(ev.v767_components, "v767_components")
    elif sub == "stat_type":
        r = hit(ev.stat_categories, "stat_category")
    elif sub == "custom_stat":
        probe = ev.blob("recipes_and_stats", "scoreboard", "statistics")
        if f'"minecraft:{name}"' in probe or f'"{name}"' in probe:
            e.append("code_ref")
            return PARTIAL, e
        return MISSING, ["custom_stat_absent"]
    elif sub == "recipe_serializer":
        r = hit(ev.recipe_serializers, "rbs_serializers")
    elif sub == "recipe_type":
        probe = ev.blob("recipes_and_stats", "menus_and_items", "datapack")
        if f'"{name}"' in probe:
            e.append("code_ref")
            return COVERED, e
        return MISSING, ["recipe_type_absent"]
    elif sub == "attribute":
        r = hit(ev.attribute_names, "attr_keys")
    elif sub == "block_entity_type":
        r = hit(ev.be_type_names, "live_block_entities")
    elif sub == "villager_profession":
        r = hit(ev.professions, "villager_profession")
    elif sub == "villager_type":
        probe = ev.blob("villager_raid", "natural_spawner", "entity_registry")
        if f'"minecraft:{name}"' in probe or f'"{name}"' in probe:
            e.append("code_ref")
            return COVERED, e
        return MISSING, ["villager_type_absent"]
    elif sub == "point_of_interest_type":
        r = hit(ev.poi_names, "poi_kinds")
    elif sub == "dimension":
        r = hit(ev.dimension_names, "dimensions")
    elif sub == "dimension_type":
        if ev.dp_json_exists("dimension_type", f"{name}.json") or (
            name in ev.dimension_names
        ):
            e.append("dimension_type_data")
            return COVERED, e
        probe = ev.blob("play_protocol", "worldgen", "datapack")
        if f'"{name}"' in probe:
            e.append("code_ref")
            return PARTIAL, e
        return MISSING, ["dimension_type_absent"]
    elif sub == "fluid":
        r = hit(ev.fluid_names, "flowing_fluid")
    elif sub == "chat_type":
        r = hit(ev.chat_types, "chat_type")
    elif sub == "decorated_pot_pattern":
        if f"{name}_pottery_sherd" in ev.item_names or name in ev.item_names:
            e.append("sherd_item")
            return COVERED, e
        probe = ev.blob("block_entity", "menus_and_items")
        if f'"{name}"' in probe:
            e.append("code_ref")
            return PARTIAL, e
        return MISSING, ["pot_pattern_absent"]
    elif sub == "armor_material":
        probe = ev.blob("combat_and_health", "menus_and_items", "enchant", "trim")
        if f'"minecraft:{name}"' in probe or f'"{name}"' in probe:
            e.append("code_ref")
            return PARTIAL, e
        return MISSING, ["armor_material_absent"]
    elif sub in (
        "enchantment_effect_component_type",
        "enchantment_entity_effect_type",
        "enchantment_location_based_effect_type",
        "enchantment_value_effect_type",
        "enchantment_provider",
        "entity_sub_predicate_type",
        "item_sub_predicate_type",
        "instrument",
        "cat_variant",
        "frog_variant",
        "game_event",
        "map_decoration_type",
        "creative_mode_tab",
        "chunk_status",
        "command_argument_type",
        "activity",
        "memory_module_type",
        "sensor_type",
        "schedule",
    ):
        probe_dirs = {
            "enchantment_effect_component_type": ("enchant", "component"),
            "enchantment_entity_effect_type": ("enchant", "component"),
            "enchantment_location_based_effect_type": ("enchant", "component"),
            "enchantment_value_effect_type": ("enchant", "component"),
            "enchantment_provider": ("enchant", "loot", "raid"),
            "entity_sub_predicate_type": ("enchant", "predicate", "selector"),
            "item_sub_predicate_type": ("enchant", "predicate", "item"),
            "instrument": ("goat", "horn", "instrument"),
            "cat_variant": ("cat", "variant"),
            "frog_variant": ("frog", "variant"),
            "game_event": ("game_event", "vibration", "sculk", "warden"),
            "map_decoration_type": ("map", "decoration", "banner"),
            "creative_mode_tab": ("creative", "menu", "inventory"),
            "chunk_status": ("chunk", "status", "worldgen"),
            "command_argument_type": ("brigadier", "argument", "commands"),
            "activity": ("mob_ai", "behavior", "activity", "brain"),
            "memory_module_type": ("mob_ai", "behavior", "memory", "brain"),
            "sensor_type": ("mob_ai", "behavior", "sensor", "brain"),
            "schedule": ("mob_ai", "behavior", "schedule", "brain"),
        }[sub]
        blob = ev.blob(*probe_dirs)
        if f'"minecraft:{name}"' in blob:
            e.append("code_ref")
            return COVERED, e
        if (len(name) >= 5 or "_" in name) and f'"{name}"' in blob:
            e.append("code_ref_bare")
            return PARTIAL, e
        return MISSING, [f"{sub}_absent"]
    elif sub in ("loot_condition_type", "loot_function_type", "loot_pool_entry_type"):
        table = {
            "loot_condition_type": ev.loot_conditions,
            "loot_function_type": ev.loot_functions,
            "loot_pool_entry_type": ev.loot_entries,
        }[sub]
        if name in table:
            e.append(f"{sub}_dispatch")
            return COVERED, e
        blob = ev.blob("datapack", "chest_loot")
        if f'"{name}"' in blob:
            e.append("code_ref")
            return PARTIAL, e
        return MISSING, [f"{sub}_absent"]
    elif sub == "trigger_type":
        blob = ev.blob("world_mechanisms", "advancement", "trigger")
        if f'"minecraft:{name}"' in blob or f'"{name}"' in blob:
            e.append("trigger_ref")
            return COVERED, e
        return MISSING, ["trigger_absent"]
    elif sub.startswith("worldgen/"):
        kind = sub.split("/", 1)[1]
        file_map = {
            "configured_carver": "configured_carver",
            "feature": "configured_feature",
            "flat_level_generator_preset": "flat_level_generator_preset",
            "noise_settings": "noise_settings",
            "placed_feature": "placed_feature",
            "structure_set": "structure_set",
            "world_preset": "world_preset",
        }
        if kind in file_map:
            if ev.dp_json_exists("worldgen", file_map[kind], f"{name}.json"):
                e.append(f"datapack:worldgen/{file_map[kind]}")
                return COVERED, e
            return MISSING, ["worldgen_file_absent"]
        # *_type registries: JSON "type" values + code parser refs
        in_json = name in ev.worldgen_type_values()
        blob = ev.blob("worldgen", "feature", "datapack", "structure_pipeline", "jigsaw")
        in_code = f'"minecraft:{name}"' in blob or (
            (len(name) >= 5 or "_" in name) and f'"{name}"' in blob
        )
        if in_json and in_code:
            e.append("worldgen_type_json+code")
            return COVERED, e
        if in_json or in_code:
            e.append("worldgen_type_partial")
            return PARTIAL, e
        return MISSING, ["worldgen_type_absent"]
    if r is not None:
        return r
    blob = ev._all_blob()
    if f'"minecraft:{name}"' in blob:
        e.append("code_ref_mc")
        return PARTIAL, e
    if (len(name) >= 6 or "_" in name) and f'"{name}"' in blob:
        e.append("code_ref_bare")
        return PARTIAL, e
    return MISSING, [f"{sub}_absent"]


def score_loot(row, ev):
    path = mc_path(row)
    sub = row["subcategory"]
    name_path = path.split(":", 1)[1] if ":" in path else path
    e = []
    if sub == "blocks":
        block = name_path.split("/", 1)[1] if "/" in name_path else name_path
        if block in getattr(ev, "_blocks_asset_names", set()):
            e.append("block_asset_drops")
            return COVERED, e
        return MISSING, ["block_unknown"]
    if name_path in ev.live_loot_tables:
        e.append("live_loot_generator")
        return COVERED, e
    if name_path in ev.referenced_loot:
        e.append("code_ref")
        return PARTIAL, e
    if sub == "archaeology":
        blob = ev.blob("block_entity", "brush", "suspicious", "archaeolog")
        base = name_path.rsplit("/", 1)[-1]
        if base and base in blob:
            e.append("archaeology_ref")
            return PARTIAL, e
    if sub == "entities":
        ent = name_path.split("/", 1)[1] if "/" in name_path else name_path
        if ent in ev.entity_names:
            blob = ev.blob("mob", "drop", "loot", "entity", "natural_spawner")
            if f'"{ent}"' in blob:
                e.append("entity_ref")
                return PARTIAL, e
    return MISSING, ["loot_table_absent"]


def score_rcp(row, ev):
    sub = row["subcategory"]
    name = result_name(row)
    el_name = bare_name(row)
    e = []

    def item_fallback():
        if name in ev.item_names or el_name in ev.item_names:
            e.append("item_exists_only")
            return PARTIAL, e
        return MISSING, ["recipe_result_unknown"]

    if sub in ("crafting_shaped", "crafting_shapeless"):
        if name in ev.craft_results or el_name in ev.craft_results:
            e.append("craft_table")
            return COVERED, e
        if name in ev.craft_templates or el_name in ev.craft_templates:
            e.append("craft_template_unexpanded")
            return PARTIAL, e
        return item_fallback()
    if sub == "stonecutting":
        if name in ev.stonecut_outputs or el_name in ev.stonecut_outputs:
            e.append("wm_stonecut")
            return COVERED, e
        return item_fallback()
    if sub in ("smelting", "blasting", "smoking"):
        if name in ev.smelt_outputs:
            e.append("smelt_table")
            return COVERED, e
        return item_fallback()
    if sub == "campfire_cooking":
        if name in ev.campfire_outputs:
            e.append("campfire_table")
            return COVERED, e
        if name in ev.smelt_outputs:
            e.append("smelt_table_only")
            return PARTIAL, e
        return item_fallback()
    if sub == "smithing_transform":
        blob = ev.blob("player_mechanisms", "menus_and_items")
        if "netherite_upgrade_smithing_template" in blob and name.startswith(
            "netherite_"
        ):
            diamond = "diamond_" + name[len("netherite_"):]
            if diamond in ev.item_names:
                e.append("smithing_transform")
                return COVERED, e
        return item_fallback()
    if sub == "smithing_trim":
        base = el_name
        for suf in ("_smithing_trim", "_armor_trim_smithing_template_smithing_trim", "_smithing_template_smithing_trim"):
            if base.endswith(suf):
                base = base[: -len(suf)]
                break
        pattern = base.replace("_armor_trim_smithing_template", "").replace(
            "_smithing_template", ""
        )
        blob = ev.blob("player_mechanisms", "menus_and_items")
        if "_armor_trim_smithing_template" in blob and (
            pattern in ev.trim_patterns or pattern in blob
        ):
            e.append("smithing_trim")
            return COVERED, e
        return MISSING, ["smithing_trim_absent"]
    if sub.startswith("crafting_special_"):
        special = sub[len("crafting_special_"):]
        blob = ev.blob("craft", "menus_and_items", "datapack", "recipes_and_stats")
        probe_names = {el_name, name, special.replace("_", "")}
        for pn in probe_names:
            if pn and (f'"{pn}"' in blob or f'"minecraft:{pn}"' in blob):
                e.append("special_ref")
                return PARTIAL, e
        return MISSING, [f"special_{special}_absent"]
    if sub == "crafting_decorated_pot":
        blob = ev.blob("craft", "menus_and_items", "datapack")
        if '"decorated_pot"' in blob:
            e.append("decorated_pot_ref")
            return PARTIAL, e
        return MISSING, ["decorated_pot_recipe_absent"]
    return MISSING, [f"rcp_{sub}_unhandled"]


MECH_ANCHORS: dict[str, tuple[str, ...]] = {
    "移動 Movement": ("player_mechanisms", "anticheat", "movement", "swim", "elytra", "sneak", "sprint", "collision"),
    "レッドストーン基礎 Redstone basics": ("redstone",),
    "道具 Tools": ("item_registry", "menus_and_items", "durability"),
    "作物と成長 Crops & growth": ("feature", "crop", "growth", "bone_meal", "fertiliz"),
    "体力 Health": ("combat_and_health", "effects", "health", "damage", "armor", "regenerat"),
    "飛び道具 Projectiles": ("combat_and_health", "projectile", "arrow", "trident", "bow", "crossbow"),
    "染料 Dye": ("dye", "banner", "shulker", "wool", "terracotta", "concrete"),
    "骨粉 Bone meal": ("bone_meal", "feature", "growth", "worldgen", "crop"),
    "醸造レシピ Brewing recipes": ("brewing", "potion", "player_mechanisms"),
    "生成 World generation": ("worldgen", "feature", "generation", "structure", "jigsaw", "noise", "biome", "carver", "template", "ore"),
    "流体 Fluids": ("flowing_fluid", "fluid", "water", "lava"),
    "村人 Villagers": ("villager", "poi", "gossip", "trade", "workstation", "raid", "profession"),
    "近接 Melee": ("combat_and_health", "melee", "attack", "knockback", "shield", "sweep"),
    "空腹 Hunger": ("player_mechanisms", "food", "hunger", "eat", "saturation"),
    "時間と天候 Time & weather": ("weather", "time", "day", "night", "thunder"),
    "メニュー Menus": ("menus_and_items", "menu", "container", "window", "anvil", "enchant", "grindstone", "loom", "cartography", "fletching", "smithing", "stonecut"),
    "インベントリ Inventory": ("menus_and_items", "container", "inventory", "slot", "stack"),
    "火 Fire": ("fire", "flint", "fireball", "magma", "blaze", "campfire", "torch"),
    "Trial chambers": ("trial", "chamber", "vault", "spawner", "ominous", "breeze", "wind_charge", "crawler"),
    "統計と進捗 Statistics & Advancements": ("recipes_and_stats", "world_mechanisms", "advancement", "stat", "recipe_book", "trigger"),
    "経験値 Experience": ("player_mechanisms", "experience", "xp", "enchant", "orb"),
    "襲撃 Raids": ("villager_raid", "raid", "pillager", "vindicator", "evoker", "illusioner", "ravager", "omen"),
    "睡眠 Sleeping": ("player_mechanisms", "sleep", "bed", "phantom", "spawn"),
    "ディメンション Dimensions": ("dimension", "play_protocol", "worldgen", "nether", "portal", "the_end"),
    "ゲームモード Game modes": ("gamemode", "player_mechanisms", "adventure", "spectator", "creative", "survival"),
    "Chat": ("chat", "tellraw", "team", "msg", "sign", "whisper"),
    "盾 Shield": ("combat_and_health", "shield", "blocking"),
    "Fishing loot": ("fishing", "fish", "loot", "bobber", "hook"),
    "Archaeology": ("archaeolog", "brush", "suspicious", "pottery", "sherd"),
    "Piglin bartering": ("piglin", "barter", "gold", "bastion", "hoglin"),
    "Lighting": ("light", "lighting", "glow", "lightning", "sea_pickle", "lantern", "redstone_lamp"),
    "効果ごとの挙動 Effect behaviors": ("effects", "effect", "potion", "status", "particle"),
    "友好 Passive mobs": ("entity", "mob", "natural_spawner", "villager", "animal", "breed", "tame", "feed", "ai", "behavior", "spawn", "allay", "axolotl", "frog", "goat", "llama", "nautilus", "sniffer", "camel", "armadillo", "bee"),
    "敵対 Hostile mobs": ("entity", "mob", "natural_spawner", "hostile", "monster", "zombie", "skeleton", "creeper", "spider", "witch", "slime", "magma", "ghast", "blaze", "wither", "ender", "piglin", "hoglin", "zoglin", "warden", "breeze", "bogged", "ai", "behavior", "spawn", "despawn", "combat"),
    "共通 Common mob mechanics": ("entity", "mob", "natural_spawner", "ai", "behavior", "spawn", "despawn", "burn", "drown", "freeze", "target", "aggress"),
    "非生物 Non-mob entities": ("entity", "item", "arrow", "boat", "minecart", "armor_stand", "painting", "item_frame", "falling", "tnt", "xp_orb", "area_effect", "marker", "display", "interaction", "leash", "chest_boat", "fishing", "hook", "eye_of_ender", "fireball", "llama_spit", "wind_charge", "breeze_wind", "shulker_bullet", "dragon_fireball", "wither_skull", "spectral", "trident", "snowball", "egg", "pearl", "experience"),
    "Data pack": ("datapack", "tag", "recipe", "loot", "advancement", "function", "predicate", "item_modifier"),
}

_MECH_STOP = {
    "the", "and", "for", "with", "per", "when", "see", "via", "not", "but",
    "can", "has", "have", "its", "each", "only", "also", "used", "use",
    "true", "false", "none", "all", "any", "minecraft", "block", "blocks",
    "entity", "entities", "item", "items", "player", "players", "mob",
    "mobs", "data", "type", "vanilla", "present", "behavior", "effect",
    "base", "level", "levels", "value", "values", "name", "from", "into",
    "over", "under", "than", "then", "that", "this", "are", "was", "were",
    "will", "would", "should", "could", "may", "might", "must", "shall",
    "one", "two", "first", "second", "new", "old", "same", "other", "more",
    "most", "some", "such", "like", "get", "set", "put", "add", "remove",
}


def score_mech(row, ev):
    sub = row["subcategory"]
    cat = row["category"]
    label = f"{cat}|{sub}"
    if "server.properties" in label or "Operator files" in label:
        return MANUAL_MECH, ["config_file_semantics"]
    key = sub if sub in MECH_ANCHORS else cat
    if key not in ev._mech_cache:
        anchors = MECH_ANCHORS.get(key) or MECH_ANCHORS.get(sub) or MECH_ANCHORS.get(cat) or ()
        matched = (
            files_matching(ev.files, *anchors) if anchors else list(ev.files.values())
        )
        toks: set[str] = set()
        for rf in matched:
            toks.update(_WORD_RE.findall(rf.code.lower()))
            for lit in rf.literals:
                toks.update(_WORD_RE.findall(lit.lower()))
        ev._mech_cache[key] = (toks, len(matched))
    blob_tokens, nfiles = ev._mech_cache[key]

    text = f"{row['element']} {row['behavior']}"
    raw_tokens = re.findall(r"[A-Za-z][A-Za-z0-9_]*", text.lower())
    name_sets = (
        ev.entity_names
        | ev.item_names
        | getattr(ev, "_blocks_asset_names", set())
        | ev.effect_names
        | {bare(x) for x in ev.enchant_names}
        | ev.biome_names
        | ev.damage_types
    )
    specific: list[str] = []
    for t in dict.fromkeys(raw_tokens):
        if t in _MECH_STOP or len(t) < 3:
            continue
        if (
            t in name_sets
            or len(t) >= 6
            or "_" in t
            or any(ch.isdigit() for ch in t)
        ):
            specific.append(t)
        if len(specific) >= 8:
            break
    hits = []
    for t in specific:
        if t in blob_tokens:
            hits.append(t)
        elif t in ev.test_tokens:
            hits.append(f"test:{t}")
    if hits:
        return NEEDS_VERIFY, [
            "anchor:" + ",".join(sorted(set(hits))[:5]),
            f"files:{nfiles}",
        ]
    reg_hits = [t for t in specific if t in name_sets]
    if reg_hits:
        return NEEDS_VERIFY, ["registry_name_only:" + ",".join(reg_hits[:4])]
    return MISSING, ["no_mechanic_anchor"]


SCORERS = {
    "BLK": score_blk,
    "ITEM": score_item,
    "ENT": score_ent,
    "BIOME": score_biome,
    "STRUCT": score_struct,
    "DMG": score_dmg,
    "ENCH": score_ench,
    "ADV": score_adv,
    "DATA": score_data,
    "TAG": score_tag,
    "GR": score_gr,
    "CMD": score_cmd,
    "PKT": score_pkt,
    "REG": score_reg,
    "RCP": score_rcp,
    "LOOT": score_loot,
    "MECH": score_mech,
}


# ---------------------------------------------------------------------------
# Driver / reporting
# ---------------------------------------------------------------------------


def score_rows(rows: list[dict], ev: Evidence) -> list[dict]:
    out = []
    for row in rows:
        p = row_prefix(row)
        fn = SCORERS.get(p)
        if fn is None:
            status, evidence = MISSING, ["no_scorer"]
        else:
            try:
                status, evidence = fn(row, ev)
            except Exception as exc:  # never crash the scoreboard
                status, evidence = MISSING, [f"scorer_error:{type(exc).__name__}"]
        out.append(
            {
                "id": row["id"],
                "prefix": p,
                "subcategory": row.get("subcategory", ""),
                "element": row.get("element", ""),
                "status": status,
                "evidence": evidence,
            }
        )
    return out


def summarize(results: list[dict]) -> dict:
    per: dict[str, dict[str, int]] = {p: {s: 0 for s in ALL_STATUSES} for p in PREFIXES}
    per_total: dict[str, int] = {p: 0 for p in PREFIXES}
    for r in results:
        p = r["prefix"]
        if p not in per:
            continue
        per[p][r["status"]] += 1
        per_total[p] += 1
    summary = {}
    for p in PREFIXES:
        counts = per[p]
        total = per_total[p]
        summary[p] = {
            "covered": counts[COVERED],
            "partial": counts[PARTIAL],
            "missing": counts[MISSING],
            "needs_verify": counts[NEEDS_VERIFY],
            "manual_mech": counts[MANUAL_MECH],
            "total": total,
            "pct": round(100.0 * counts[COVERED] / total, 1) if total else 0.0,
        }
    return summary


def top_gaps(results: list[dict], n: int) -> list[dict]:
    clusters: dict[tuple[str, str], dict] = {}
    for r in results:
        if r["status"] not in (MISSING, NEEDS_VERIFY):
            continue
        key = (r["prefix"], r["subcategory"])
        c = clusters.setdefault(
            key,
            {
                "prefix": r["prefix"],
                "subcategory": r["subcategory"],
                "count": 0,
                "samples": [],
            },
        )
        c["count"] += 1
        if len(c["samples"]) < 5:
            c["samples"].append(r["element"][:80])
    ordered = sorted(
        clusters.values(),
        key=lambda c: (-c["count"], c["prefix"], c["subcategory"]),
    )
    return ordered[:n]


def print_table(summary: dict) -> None:
    hdr = (
        f"{'Prefix':<7} {'Covered':>8} {'Partial':>8} {'Missing':>8} "
        f"{'NVerify':>8} {'Manual':>7} {'Total':>7} {'%':>7}"
    )
    print(hdr)
    print("-" * len(hdr))
    tot_cov = tot_all = 0
    for p in PREFIXES:
        s = summary[p]
        tot_cov += s["covered"]
        tot_all += s["total"]
        print(
            f"{p:<7} {s['covered']:>8} {s['partial']:>8} {s['missing']:>8} "
            f"{s['needs_verify']:>8} {s['manual_mech']:>7} {s['total']:>7} "
            f"{s['pct']:>6.1f}%"
        )
    print("-" * len(hdr))
    pct = 100.0 * tot_cov / tot_all if tot_all else 0.0
    print(f"{'TOTAL':<7} {tot_cov:>8} {'':>8} {'':>8} {'':>8} {'':>7} {tot_all:>7} {pct:>6.1f}%")


def print_gaps(gaps: list[dict]) -> None:
    print("\nTop missing / needs-verify clusters:")
    for i, g in enumerate(gaps, 1):
        print(f"  {i:>2}. {g['prefix']:<5} {g['subcategory']:<38} x{g['count']}")
        for s in g["samples"][:3]:
            print(f"        e.g. {s}")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--csv",
        default=os.environ.get(CSV_ENV, DEFAULT_CSV),
        help=f"feature CSV path (env {CSV_ENV})",
    )
    ap.add_argument("--repo", default=str(Path(__file__).resolve().parent.parent))
    ap.add_argument("--top-gaps", type=int, default=25)
    ap.add_argument("--json-out", default=None)
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args(argv)

    csv_path = Path(args.csv)
    if not csv_path.is_file():
        print(f"error: CSV not found: {csv_path}", file=sys.stderr)
        return 2
    repo = Path(args.repo)

    rows = []
    with csv_path.open(newline="", encoding="utf-8") as fh:
        for r in csv.DictReader(fh):
            rows.append(r)

    ev = Evidence(repo)
    results = score_rows(rows, ev)
    summary = summarize(results)
    gaps = top_gaps(results, args.top_gaps)

    if not args.quiet:
        print(f"CSV: {csv_path} ({len(rows)} rows)")
        print(f"Repo: {repo}")
        if ev.skipped:
            print(f"skipped files: {len(ev.skipped)}")
            for s in ev.skipped[:10]:
                print(f"  ! {s}")
        print()
        print_table(summary)
        print_gaps(gaps)

    if args.json_out:
        out_path = Path(args.json_out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "tool": "score_1211_features.py",
            "csv": str(csv_path),
            "repo": str(repo),
            "rows_total": len(results),
            "skipped_files": ev.skipped,
            "summary": summary,
            "top_gaps": gaps,
            "rows": results,
        }
        out_path.write_text(json.dumps(payload, indent=1, ensure_ascii=False))
        if not args.quiet:
            print(f"\nJSON: {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
