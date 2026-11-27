#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Generic, registry-driven Vanilla 1.21.1 gameplay-mechanics coverage audit.

The defect class this gate exists for is "declared, but never applied": a
damage type that only exists in the ``/damage`` argument table (an elytra wall
hit that silently deals nothing), a mob-AI ``Unavailable("AvoidEntityGoal<Cat>")``
placeholder, a ``GameRule`` that ``/gamerule`` stores and ``save_v2`` persists
but no live tick ever reads, a ``MobEffects`` registry entry with no behavioural
handler, or an ``Enchantments`` table entry that never reaches a combat/loot
consumer. Each one still compiles and its unit tests stay green.

Everything here is *discovered* from declarations in ``src/`` — there is no
per-feature list to update when a mechanic is added:

* ``unimplemented_damage_type`` — the damage-type registry is the string-array
  local constant of the ``*_known``/``*_valid`` argument validator. Every
  declared type must have at least one literal reference in live, non-test,
  damage-vocabulary code outside the registry/validation table.
* ``unavailable_mob_ai_goal`` — every ``Unavailable("...")`` goal placeholder in
  live ``src/`` code. The set is ratcheted through the waiver manifest, so a
  new placeholder is an immediate failure and implementing one makes its
  waiver stale.
* ``unconsumed_gamerule`` — every ``(&str, bool, i32)`` gamerule default table
  entry must have a live read of its name (or of the atomic its
  ``gamerule_atomic_value`` arm maps it to) outside the command parser,
  save/load footer and the atomic name table itself.
* ``unapplied_status_effect`` — every ``EFFECT_REGISTRY`` name must reach a
  live behavioural handler: a name reference in live code, or the internal id
  matched by the live effect tick handler (``match e.id { … }``).
* ``unapplied_enchantment`` — every name in the ``enchant_id`` name→id map must
  be consumed by a live ``enchant_*`` call carrying its numeric id (the live
  combat/loot path keys on ids, not names), or by a live name reference.

Source is masked before any analysis (comments, string/char literals and
``#[cfg(test)]`` items are blanked with offsets preserved), so a call site
that only exists in a test, a comment or a doc example can never satisfy a
check. A second pass keeps string-literal contents so registry names can be
located while brace accounting still cannot drift.

``audit/vanilla_mechanics_waivers.json`` baselines the currently-known gaps
with a reason and owning lane. A waiver whose mechanic was implemented or
removed is *stale*: reported as a warning by default and a hard failure under
``--fail-on-stale`` so the baseline can be pruned.

Usage:
    tools/vanilla_mechanics_coverage_audit.py
    tools/vanilla_mechanics_coverage_audit.py --json
    tools/vanilla_mechanics_coverage_audit.py --fail-on-stale
    tools/vanilla_mechanics_coverage_audit.py --root DIR   # hermetic fixture
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
WAIVERS_REL = "audit/vanilla_mechanics_waivers.json"

CATEGORIES = (
    "unimplemented_damage_type",
    "unavailable_mob_ai_goal",
    "unconsumed_gamerule",
    "unapplied_status_effect",
    "unapplied_enchantment",
)

# ---------------------------------------------------------------------------
# Rust source masking. Mirrors tools/wiring_connectivity_audit.py: comments,
# literals and cfg(test) items are blanked while offsets are preserved.
# ---------------------------------------------------------------------------

RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
CHARACTER = re.compile(r"'(?:[^'\\\n]|\\(?:.|u\{[A-Fa-f0-9_]+\}))'")
CFG_TEST = re.compile(r"\bcfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)")

def _balanced_end(code, start, opening, closing):
    depth = 0
    for pos in range(start, len(code)):
        if code[pos] == opening:
            depth += 1
        elif code[pos] == closing:
            depth -= 1
            if depth == 0:
                return pos + 1
    return len(code)


def _mask(code, start, end):
    for idx in range(start, min(end, len(code))):
        code[idx] = " "


def _skip_space(code, pos):
    while pos < len(code) and code[pos].isspace():
        pos += 1
    return pos


def production_code(text, keep_strings=False):
    """Mask Rust comments/literals and ``#[cfg(test)]`` items, keep offsets."""
    code = list(text)
    pos = 0
    while pos < len(text):
        start = pos
        string_span = None
        if text.startswith("//", pos):
            end = text.find("\n", pos)
            pos = len(text) if end < 0 else end
        elif text.startswith("/*", pos):
            pos += 2
            depth = 1
            while pos < len(text) and depth:
                if text.startswith("/*", pos):
                    depth += 1
                    pos += 2
                elif text.startswith("*/", pos):
                    depth -= 1
                    pos += 2
                else:
                    pos += 1
        elif match := RAW_STRING.match(text, pos):
            terminator = '"' + match.group(1)
            end = text.find(terminator, match.end())
            pos = len(text) if end < 0 else end + len(terminator)
            string_span = (start, pos)
        elif text[pos] == '"':
            pos += 1
            while pos < len(text):
                if text[pos] == "\\":
                    pos += 2
                elif text[pos] == '"':
                    pos += 1
                    break
                else:
                    pos += 1
            string_span = (start, pos)
        elif match := CHARACTER.match(text, pos):
            pos = match.end()
        else:
            pos += 1
            continue
        if keep_strings and string_span is not None:
            # Keep the literal's value for discovery, neutralise brackets so
            # the cfg(test) item walk below cannot be thrown off balance.
            s, e = string_span
            for idx in range(s, min(e, len(code))):
                if code[idx] in "(){}[]":
                    code[idx] = " "
        else:
            _mask(code, start, min(pos, len(text)))

    clean = "".join(code)
    for match in re.finditer(r"#\s*\[", clean):
        start = match.start()
        if code[start] == " ":
            continue
        attr_end = _balanced_end(clean, clean.index("[", start), "[", "]")
        if not CFG_TEST.search(clean[start:attr_end]):
            continue
        item_start = attr_end
        while True:
            item_start = _skip_space(clean, item_start)
            if not clean.startswith("#[", item_start):
                break
            item_start = _balanced_end(clean, item_start + 1, "[", "]")
        paren = bracket = 0
        item_end = len(clean)
        for idx in range(item_start, len(clean)):
            char = clean[idx]
            if char == "(":
                paren += 1
            elif char == ")":
                paren -= 1
            elif char == "[":
                bracket += 1
            elif char == "]":
                bracket -= 1
            elif not paren and not bracket and char == "{":
                item_end = _balanced_end(clean, idx, "{", "}")
                break
            elif not paren and not bracket and char == ";":
                item_end = idx + 1
                break
        _mask(code, start, item_end)
    return "".join(code)


# ---------------------------------------------------------------------------
# Rust item discovery on masked code.
# ---------------------------------------------------------------------------

FN_HEAD = re.compile(
    r"\b(pub(?:\s*\([^)]*\))?)?\s*"
    r"(?:async\s+|unsafe\s+|const\s+|extern\s+\"[A-Za-z]+\"\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)

CONST_HEAD = re.compile(
    r"\b(?:pub(?:\s*\([^)]*\))?\s+)?(?:const|static)\s+([A-Za-z_][A-Za-z0-9_]*)"
)

TOKEN = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\b")


class Fn:
    __slots__ = ("name", "start", "body", "end", "dead")

    def __init__(self, name, start, body, end, dead):
        self.name = name
        self.start = start
        self.body = body
        self.end = end
        self.dead = dead


class Item:
    """A ``const``/``static`` declaration and the string literals it holds."""

    __slots__ = ("name", "start", "end", "strings")

    def __init__(self, name, start, end, strings):
        self.name = name
        self.start = start
        self.end = end
        self.strings = strings


def _skip_ws_back(text, pos):
    while pos > 0 and text[pos - 1].isspace():
        pos -= 1
    return pos


def attributes_before(text, pos):
    """Attributes immediately preceding ``pos`` in masked source."""
    attrs = []
    cursor = pos
    while True:
        end = _skip_ws_back(text, cursor)
        if end == 0 or text[end - 1] != "]":
            break
        depth = 0
        start = end - 1
        while start >= 0:
            if text[start] == "]":
                depth += 1
            elif text[start] == "[":
                depth -= 1
                if depth == 0:
                    break
            start -= 1
        hash_pos = _skip_ws_back(text, start)
        if hash_pos == 0 or text[hash_pos - 1] != "#":
            break
        attrs.append(text[hash_pos - 1:end])
        cursor = hash_pos - 1
    return attrs


def fn_ranges(code):
    fns = []
    for match in FN_HEAD.finditer(code):
        depth = 0
        brace = -1
        for pos in range(match.end(), len(code)):
            char = code[pos]
            if char in "([":
                depth += 1
            elif char in ")]":
                depth -= 1
            elif char == "{" and depth == 0:
                brace = pos
                break
            elif char == ";" and depth == 0:
                break
        if brace < 0:
            continue
        end = _balanced_end(code, brace, "{", "}")
        attrs = " ".join(attributes_before(code, match.start()))
        dead = "allow(dead_code)" in attrs.replace(" ", "")
        fns.append(Fn(match.group(2), match.start(), brace, end, dead))
    return fns


def const_ranges(code):
    items = []
    for match in CONST_HEAD.finditer(code):
        depth = 0
        assign = -1
        end = -1
        for pos in range(match.end(), len(code)):
            char = code[pos]
            if char in "([{":
                depth += 1
            elif char in ")]}":
                depth -= 1
            elif char == "=" and depth == 0 and assign < 0:
                assign = pos
            elif char == ";" and depth == 0:
                end = pos + 1
                break
        if assign < 0 or end < 0:
            continue
        items.append(Item(match.group(1), match.start(), end, []))
    return items


def literal_spans(text):
    """``(start, end, value)`` for every string literal in masked-keep text."""
    spans = []
    pos = 0
    while pos < len(text):
        match = RAW_STRING.match(text, pos)
        if match:
            terminator = '"' + match.group(1)
            value_start = match.end()
            end = text.find(terminator, value_start)
            if end < 0:
                end = len(text)
            spans.append((match.start(), end + len(terminator),
                          text[value_start:end]))
            pos = end + len(terminator)
            continue
        if text[pos] == '"':
            value_start = pos + 1
            cursor = value_start
            while cursor < len(text):
                if text[cursor] == "\\":
                    cursor += 2
                elif text[cursor] == '"':
                    break
                else:
                    cursor += 1
            spans.append((pos, min(cursor + 1, len(text)),
                          text[value_start:cursor]))
            pos = cursor + 1
            continue
        pos += 1
    return spans


def enclosing(ranges, offset):
    found = None
    for item in ranges:
        if item.start <= offset < item.end:
            if found is None or item.start >= found.start:
                found = item
    return found


def strings_in(code_keep, start, end):
    return [value for s, e, value in literal_spans(code_keep)
            if start <= s and e <= end]


class Source:
    __slots__ = ("path", "rel", "raw", "code", "keep", "values", "fns",
                 "items", "lits")

    def __init__(self, path, rel, raw):
        self.path = path
        self.rel = rel
        self.raw = raw
        self.code = production_code(raw)
        self.keep = production_code(raw, keep_strings=True)
        # `keep` has brackets inside literals neutralised so the cfg(test)
        # walk cannot drift; restore the true bytes at those spans for value
        # extraction (goal labels like `AvoidEntityGoal<Armadillo> (unscared)`
        # must keep their punctuation).
        values = list(self.keep)
        for start, end, _ in literal_spans(self.keep):
            for idx in range(start, end):
                values[idx] = self.raw[idx]
        self.values = "".join(values)
        self.fns = fn_ranges(self.code)
        self.items = const_ranges(self.code)
        self.lits = literal_spans(self.values)
        for item in self.items:
            item.strings = [value for s, e, value in self.lits
                            if item.start <= s and e <= item.end]


def load_sources(root):
    sources = []
    src = root / "src"
    if not src.is_dir():
        return sources
    for path in sorted(src.rglob("*.rs")):
        if path.name.endswith("_tests.rs"):
            continue
        try:
            raw = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        sources.append(Source(path, str(path.relative_to(root)), raw))
    return sources


# ---------------------------------------------------------------------------
# Registry discovery (no feature names, only declaration shapes).
# ---------------------------------------------------------------------------

SNAKE = re.compile(r"^[a-z][a-z0-9_]*$")


def damage_type_registry(sources):
    """The string table of the ``*_known``/``*_valid`` damage-type validator."""
    for source in sources:
        for fn in source.fns:
            body = source.code[fn.start:fn.end]
            # A registry validator must be named like one. A generic
            # `*_type_*` predicate is not enough: damage-type codec validators
            # also contain small enum tables (`DamageScaling`), and treating
            # those as the registry makes the audit audit the wrong domain.
            looks_like_validator = (
                re.search(r"(?i)(?:known|valid|exists)$", fn.name) is not None
            )
            if not looks_like_validator or ".contains(" not in body:
                continue
            for item in source.items:
                if not (fn.start <= item.start < fn.end):
                    continue
                names = [value for value in item.strings if SNAKE.match(value)]
                if len(names) >= 3:
                    return sorted(set(names))
            # The real registry is usually a file-level constant referenced by
            # the validator (`VANILLA_DAMAGE_TYPES.contains(...)`), not an item
            # nested inside the function. Resolve that exact identifier instead
            # of guessing any damage/type-shaped table in the workspace.
            for match in re.finditer(
                    r"\b([A-Z][A-Z0-9_]*)\s*\.\s*contains\s*\(", body):
                table_name = match.group(1)
                for item in source.items:
                    if item.name != table_name:
                        continue
                    names = [value for value in item.strings if SNAKE.match(value)]
                    if len(names) >= 3:
                        return sorted(set(names))
    # Fallback: a damage-type-named string array.
    for source in sources:
        for item in source.items:
            if re.search(r"(?i)damage.*type|type.*damage", item.name):
                names = [value for value in item.strings if SNAKE.match(value)]
                if len(names) >= 3:
                    return sorted(set(names))
    return []


def gamerule_registry(sources):
    """The ``(&str, bool, i32)`` gamerule default table (first field names)."""
    for source in sources:
        for item in source.items:
            head = source.code[item.start:item.start + 400]
            if item.name.upper().startswith("COMMAND_GAMERULE") or (
                re.search(r"\(\s*&str\s*,\s*bool\s*,\s*i32\s*\)", head)
                and re.search(r"(?i)gamerule|rule", item.name)
            ):
                names = [value for value in item.strings
                         if value and ":" not in value and " " not in value]
                if names:
                    return sorted(set(names))
    return []


def effect_registry(sources):
    """The ``MobEffects`` declaration-order string array."""
    for source in sources:
        for item in source.items:
            if not re.search(r"(?i)effect", item.name):
                continue
            head = source.code[item.start:item.start + 300]
            if not re.search(r"\[\s*&str\s*;|&\s*\[\s*&str\s*\]", head):
                continue
            names = [value for value in item.strings if SNAKE.match(value)]
            if len(names) >= 5:
                return names
    return []


def enchantment_registry(sources):
    """``name -> id`` arms of the enchantment id map function."""
    for source in sources:
        for fn in source.fns:
            if not re.search(r"(?i)^enchant(?:ment)?_?id$", fn.name):
                continue
            body = source.values[fn.start:fn.end]
            mapping = {}
            for match in re.finditer(r'"([^"\\]+)"\s*=>\s*(-?\d+)', body):
                mapping[match.group(1).split(":")[-1]] = int(match.group(2))
            if len(mapping) >= 5:
                return mapping
    return {}


# ---------------------------------------------------------------------------
# Live-code analysis: tests are already masked; a function carrying
# ``allow(dead_code)`` is live only when a live function references it.
# ---------------------------------------------------------------------------

class Scanner:
    def __init__(self, sources):
        self.sources = sources
        self.fn_index = []
        for source in sources:
            for fn in source.fns:
                self.fn_index.append((source, fn))
        self._live = None
        self._tokens = {}
        self._table_cache = {}

    def _token_map(self, source):
        cached = self._tokens.get(source.rel)
        if cached is None:
            cached = list(TOKEN.finditer(source.code))
            self._tokens[source.rel] = cached
        return cached

    def _refs(self):
        """callee-name -> enclosing function indexes that reference it."""
        if getattr(self, "_ref_cache", None) is not None:
            return self._ref_cache
        by_name = {}
        for idx, (_, fn) in enumerate(self.fn_index):
            by_name.setdefault(fn.name, []).append(idx)
        refs = {name: set() for name in by_name}
        for source in self.sources:
            fns = [(i, f) for i, (s, f) in enumerate(self.fn_index)
                   if s is source]
            if not fns:
                continue
            starts = sorted((f.start, i) for i, f in fns)
            for token in self._token_map(source):
                name = token.group(1)
                if name not in by_name:
                    continue
                holder = None
                for start, idx in starts:
                    if start > token.start():
                        break
                    holder = idx
                if holder is None:
                    continue
                if self.fn_index[holder][1].start <= token.start() < \
                        self.fn_index[holder][1].end:
                    refs[name].add(holder)
        self._ref_cache = refs
        return refs

    def live(self):
        if self._live is not None:
            return self._live
        refs = self._refs()
        live = {i for i, (_, fn) in enumerate(self.fn_index) if not fn.dead}
        changed = True
        while changed:
            changed = False
            for i, (_, fn) in enumerate(self.fn_index):
                if i in live or not fn.dead:
                    continue
                if any(caller in live for caller in refs.get(fn.name, ())):
                    live.add(i)
                    changed = True
        self._live = live
        return live

    def fn_is_live(self, source, fn):
        for idx, (_src, candidate) in enumerate(self.fn_index):
            if candidate is fn:
                return idx in self.live()
        return not fn.dead

    def names_in(self, source, start, end, names):
        """Distinct registry names referenced as string literals in a span."""
        found = set()
        for s, e, value in source.lits:
            if not (start <= s and e <= end):
                continue
            bare = value.split(":")[-1]
            if bare in names:
                found.add(bare)
        return found

    def table_fn(self, source, fn, names):
        """A name/id/registry mapping table rather than a behavioural user."""
        key = (source.rel, fn.start)
        cached = self._table_cache.get(key)
        if cached is not None:
            return cached
        hits = self.names_in(source, fn.start, fn.end, names)
        table_name = re.search(
            r"(?i)(?:^|_)(?:id|ids|name|names|known|valid|lookup|table|registry|"
            r"default|defaults|max|level|cost|rule|rules|wire|pkt|packet|encode|"
            r"decode|serialize|deserialize|read|write|nbt|persist|save|load|"
            r"restore|migrate|footer|args?|parse|usage)(?:_|$)", fn.name)
        result = False
        if hits:
            # A single-name reference inside a getter is a consumer, not a
            # mapping table; tables map at least two registry entries.
            if table_name and len(hits) >= 2:
                result = True
            elif len(names) >= 8 and len(hits) >= len(names) // 2:
                # A function naming half of a large registry is a table, not
                # a behavioural handler.
                result = True
        self._table_cache[key] = result
        return result

    def table_item(self, source, item, names):
        hits = self.names_in(source, item.start, item.end, names)
        if not hits:
            return False
        if len(hits) >= 2:
            return True
        return bool(re.search(
            r"(?i)(?:registry|table|defaults?|names?|types?|rules?)$", item.name))


EFFECT_CONTEXT = re.compile(
    r"(?i)\beffects?\b|\beffs?\b|\beffect_|effect_registry")

ENCHANT_CONTEXT = re.compile(r"(?i)\benchant")

DAMAGE_CONTEXT = re.compile(
    r"\b(?:damage_type|damage_source|damage_types|DamageSource|DamageType|"
    r"apply_damage|deal_damage|damage_player|damage_entity|damage_mob|"
    r"take_damage|hurt|death_cause|killed_by)\b"
)

GAMERULE_PLUMBING = re.compile(
    r"(?i)(?:save|load|restore|persist|serial|footer|atomic|value_store|sync)"
)


def literal_occurrences(source, names):
    """name -> list of (offset, literal value)."""
    index = {}
    for start, _end, value in source.lits:
        bare = value.split(":")[-1]
        if bare in names:
            index.setdefault(bare, []).append((start, value))
    return index


def live_reference(source, scanner, offset, names, fn_live_check,
                   context=None):
    """Whether a literal at ``offset`` is a live, non-table reference."""
    fn = enclosing(source.fns, offset)
    if fn is not None:
        if scanner.table_fn(source, fn, names):
            return False
        if not fn_live_check(fn):
            return False
        if context is not None and not context.search(
                source.code[fn.start:fn.end]):
            return False
        return True
    item = enclosing(source.items, offset)
    if item is not None:
        return not scanner.table_item(source, item, names)
    return False


def check_damage_types(sources, scanner):
    names = set(damage_type_registry(sources))
    live = set()
    context_ok = {}
    for source in sources:
        occurrences = literal_occurrences(source, names)
        for name, positions in occurrences.items():
            for offset, _ in positions:
                fn = enclosing(source.fns, offset)
                if fn is None:
                    continue
                body = source.code[fn.start:fn.end]
                key = (source.rel, fn.start)
                if key not in context_ok:
                    context_ok[key] = bool(DAMAGE_CONTEXT.search(body))
                if not context_ok[key]:
                    continue
                if live_reference(source, scanner, offset, names,
                                  lambda f, s=source: scanner.fn_is_live(s, f)):
                    live.add(name)
                    break
    return sorted(names), live


def check_mob_ai_goals(sources, scanner):
    found = set()
    for source in sources:
        for match in re.finditer(
                r'\bUnavailable\s*\(\s*"((?:[^"\\]|\\.)*)"', source.values):
            if enclosing(source.fns, match.start()) is None:
                continue
            found.add(match.group(1))
    # A placeholder is by definition unimplemented; nothing is "live".
    return sorted(found), set()


def gamerule_atomic_map(sources):
    """rule -> atomic constant from ``gamerule_atomic_value``-shaped fns."""
    mapping = {}
    for source in sources:
        for fn in source.fns:
            body = source.code[fn.start:fn.end]
            if ".load(" not in body and ".store(" not in body:
                continue
            if not re.search(r"(?i)gamerule", fn.name):
                continue
            for match in re.finditer(
                    r'"([A-Za-z][A-Za-z0-9_]*)"\s*=>\s*\{?\s*'
                    r"(?:[A-Za-z_][A-Za-z0-9_]*::)*"
                    r"([A-Z_][A-Z0-9_]*)\s*\.\s*(?:load|store)\s*\(",
                    source.values[fn.start:fn.end]):
                mapping[match.group(1)] = match.group(2)
    return mapping


def check_gamerules(sources, scanner):
    names = set(gamerule_registry(sources))
    mapping = gamerule_atomic_map(sources)
    live = set()
    for source in sources:
        occurrences = literal_occurrences(source, names)
        for name, positions in occurrences.items():
            for offset, _ in positions:
                fn = enclosing(source.fns, offset)
                if fn is not None and GAMERULE_PLUMBING.search(fn.name):
                    continue
                if live_reference(source, scanner, offset, names,
                                  lambda f, s=source: scanner.fn_is_live(s, f)):
                    live.add(name)
                    break
        # The atomic a rule maps to is an equally valid live read.
        for name, constant in mapping.items():
            if name not in names or name in live:
                continue
            pattern = re.compile(r"\b" + re.escape(constant) + r"\b")
            for fn in source.fns:
                if not pattern.search(source.code[fn.start:fn.end]):
                    continue
                if scanner.table_fn(source, fn, names):
                    continue
                if GAMERULE_PLUMBING.search(fn.name):
                    continue
                if scanner.fn_is_live(source, fn):
                    live.add(name)
                    break
    return sorted(names), live


def effect_numeric_handlers(sources, scanner, names):
    """Internal effect ids matched by a live effect-tick handler."""
    by_index = list(names)
    live = set()
    for source in sources:
        for fn in source.fns:
            if "effect" not in fn.name.lower():
                continue
            if scanner.table_fn(source, fn, set(names)):
                continue
            if not scanner.fn_is_live(source, fn):
                continue
            body = source.code[fn.start:fn.end]
            ids = set()
            for match in re.finditer(
                    r"^\s*((?:\d{1,2}\s*\|\s*)*\d{1,2})\s*=>", body, re.M):
                ids.update(int(part) for part in
                           re.findall(r"\d{1,2}", match.group(1)))
            for match in re.finditer(
                    r"(?:\.\s*id|\bid)\s*==\s*(\d{1,2})\b", body):
                ids.add(int(match.group(1)))
            for value in ids:
                if 0 <= value < len(by_index):
                    live.add(by_index[value])
    # A registry that remaps a few names to sentinel internal ids exposes the
    # remap arm in an ``*internal*id`` helper; a live read of that sentinel
    # constant is the behavioural hook for the remapped name.
    sentinels = {}
    for source in sources:
        for fn in source.fns:
            if not re.search(r"(?i)effect.*(?:internal|wire|protocol).*id",
                             fn.name):
                continue
            for match in re.finditer(
                    r'name\s*==\s*"([^"\\]+)"\s*\{\s*'
                    r"([A-Z_][A-Z0-9_]*)", source.values[fn.start:fn.end]):
                sentinels[match.group(1)] = match.group(2)
    for name, constant in sentinels.items():
        if name not in names:
            continue
        pattern = re.compile(r"\b" + re.escape(constant) + r"\b")
        for source in sources:
            for fn in source.fns:
                if not scanner.fn_is_live(source, fn):
                    continue
                if pattern.search(source.code[fn.start:fn.end]):
                    live.add(name)
                    break
    return live


def check_status_effects(sources, scanner):
    ordered = effect_registry(sources)
    names = set(ordered)
    live = set()
    for source in sources:
        occurrences = literal_occurrences(source, names)
        for name, positions in occurrences.items():
            for offset, _ in positions:
                if live_reference(source, scanner, offset, names,
                                  lambda f, s=source: scanner.fn_is_live(s, f),
                                  EFFECT_CONTEXT):
                    live.add(name)
                    break
    if ordered:
        live |= effect_numeric_handlers(sources, scanner, ordered)
    return sorted(names), live


def enchant_numeric_handlers(sources, scanner, id_to_name):
    live = set()
    for source in sources:
        for fn in source.fns:
            if not scanner.fn_is_live(source, fn):
                continue
            body = source.code[fn.start:fn.end]
            if "enchant" not in body.lower():
                continue
            for match in re.finditer(r"\benchant_\w+\s*\(", body):
                start = match.end() - 1
                end = _balanced_end(body, start, "(", ")")
                inner = body[start + 1:end - 1]
                for argument in split_arguments(inner):
                    argument = argument.strip()
                    if re.fullmatch(r"-?\d+", argument):
                        value = int(argument)
                        if value in id_to_name:
                            live.add(id_to_name[value])
    return live


def split_arguments(text):
    parts = []
    depth = 0
    current = []
    for char in text:
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(char)
    parts.append("".join(current))
    return parts


def check_enchantments(sources, scanner):
    mapping = enchantment_registry(sources)
    names = set(mapping)
    id_to_name = {value: key for key, value in mapping.items()}
    live = set()
    for source in sources:
        occurrences = literal_occurrences(source, names)
        for name, positions in occurrences.items():
            for offset, _ in positions:
                if live_reference(source, scanner, offset, names,
                                  lambda f, s=source: scanner.fn_is_live(s, f),
                                  ENCHANT_CONTEXT):
                    live.add(name)
                    break
    if mapping:
        live |= enchant_numeric_handlers(sources, scanner, id_to_name)
    return sorted(names), live


# ---------------------------------------------------------------------------
# Waivers + report.
# ---------------------------------------------------------------------------

def load_waivers(root):
    path = root / WAIVERS_REL
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"vanilla-mechanics: unreadable {path}: {error}") from error
    if not isinstance(data, dict):
        raise SystemExit(f"vanilla-mechanics: {path} must hold a JSON object")
    return data


def waiver_names(waivers, category):
    entries = waivers.get(category, {})
    if isinstance(entries, dict):
        return set(entries)
    if isinstance(entries, list):
        names = set()
        for entry in entries:
            if isinstance(entry, str):
                names.add(entry)
            elif isinstance(entry, dict) and "id" in entry:
                names.add(entry["id"])
        return names
    return set()


def audit(root):
    sources = load_sources(root)
    scanner = Scanner(sources)
    checks = {
        "unimplemented_damage_type": check_damage_types,
        "unavailable_mob_ai_goal": check_mob_ai_goals,
        "unconsumed_gamerule": check_gamerules,
        "unapplied_status_effect": check_status_effects,
        "unapplied_enchantment": check_enchantments,
    }
    waivers = load_waivers(root)
    report = {}
    for category in CATEGORIES:
        declared, live = checks[category](sources, scanner)
        declared_set = set(declared)
        waived_all = waiver_names(waivers, category)
        gaps = declared_set - live
        waived = gaps & waived_all
        unwaived = gaps - waived_all
        stale = waived_all - gaps
        report[category] = {
            "scanned": len(declared_set),
            "live_wired": len(live & declared_set),
            "waived": len(waived),
            "unwaived": len(unwaived),
            "stale": len(stale),
            "live": sorted(live & declared_set),
            "waived_ids": sorted(waived),
            "unwaived_ids": sorted(unwaived),
            "stale_ids": sorted(stale),
        }
    return report


def print_report(report, root):
    print(f"Vanilla 1.21.1 mechanics coverage audit (root: {root})")
    print(f"{'category':34}{'scanned':>9}{'live_wired':>12}"
          f"{'waived':>8}{'unwaived':>10}{'stale':>7}")
    for category in CATEGORIES:
        entry = report[category]
        print(f"{category:34}{entry['scanned']:>9}{entry['live_wired']:>12}"
              f"{entry['waived']:>8}{entry['unwaived']:>10}"
              f"{entry['stale']:>7}")
    for category in CATEGORIES:
        entry = report[category]
        if entry["unwaived_ids"]:
            print(f"\nUNWAIVED {category}:")
            for name in entry["unwaived_ids"]:
                print(f"  - {name}")
        if entry["stale_ids"]:
            print(f"\nSTALE {category}:")
            for name in entry["stale_ids"]:
                print(f"  - {name}")
    for category in CATEGORIES:
        entry = report[category]
        if entry["waived_ids"]:
            print(f"\nwaived {category} ({entry['waived']}):")
            print("  " + ", ".join(entry["waived_ids"]))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=str(REPO_ROOT),
                        help="repository root to audit (default: this tree)")
    parser.add_argument("--json", action="store_true",
                        help="machine-readable report")
    parser.add_argument("--fail-on-stale", action="store_true",
                        help="exit 1 when a waiver no longer matches a gap")
    options = parser.parse_args(argv)
    root = Path(options.root).resolve()
    report = audit(root)
    unwaived = sum(entry["unwaived"] for entry in report.values())
    stale = sum(entry["stale"] for entry in report.values())
    ok = unwaived == 0 and (stale == 0 or not options.fail_on_stale)
    if options.json:
        print(json.dumps({
            "root": str(root),
            "ok": ok,
            "unwaived": unwaived,
            "stale": stale,
            "categories": report,
        }, indent=2, sort_keys=True))
    else:
        print_report(report, root)
        print()
        if unwaived:
            print(f"FAIL: {unwaived} unwaived mechanic gap(s)")
        elif stale and options.fail_on_stale:
            print(f"FAIL: {stale} stale waiver(s)")
        else:
            detail = f", {stale} stale waiver(s)" if stale else ""
            print(f"PASS: every declared vanilla mechanic is wired or "
                  f"waived ({detail.strip(', ') if detail else 'no stale waivers'})")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
