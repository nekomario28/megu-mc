#!/usr/bin/env python3
"""vanilla_parity_audit.py -- mc-spine vs Vanilla 1.21.1 (protocol 767) parity gap detector.

Schema: "megu-parity-audit/1".

Mode A (always, zero external files): static live-wiring audit of the repo.
  * Protocol 767 C2S Play packet coverage: every Serverbound Play id in
    proto/src/data_v767.rs vs the hex arms of the live `match pid` dispatch in
    src/play_protocol.rs.
  * Gamerule live-usage audit (CV3 dead-code detector): every gamerule in
    COMMAND_GAMERULE_DEFAULTS (src/commands_core.rs) and every `GR_*` atomic
    (src/main.rs) is checked for non-test live call sites across src/.
  * Command root live coverage: Brigadier literals registered by
    native_command_dispatcher vs the top-level arms of `match cmd` in
    run_command_ctx.
  * Block/mob/mechanism evaluator wiring: top-level `fn` definitions in the
    mechanism files classified live-wired / test-only / dead.

Mode B (optional): differential audit against a local Vanilla 1.21.1
server.jar (`--vanilla-jar`, run via `java -DbundlerMainClass=net.minecraft.data.Main
-jar <jar> --reports --output <tmp>`) or a pre-generated reports dir
(`--reports-dir` containing packets.json / commands.json / registries.json).

Pure Python 3 stdlib. No shell=True. Linux/macOS/Windows portable.

Exit codes: 0 = report produced (or strict gate passed); 1 = strict gate
violation (--strict/--min-*/--require-zero-proto-drift); 2 = fail-closed
input/parse error.
"""

import argparse
import bisect
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCHEMA = "megu-parity-audit/1"
PROTOCOL = 767

SRC_FILES = [
    "proto/src/data_v767.rs",
    "src/play_protocol.rs",
    "src/main.rs",
    "src/commands_core.rs",
    "src/teams_and_commands.rs",
    "src/menus_and_items.rs",
    "src/mob_behaviors.rs",
]

MECHANISM_FILES = [
    "src/commands_core.rs",
    "src/mob_behaviors.rs",
    "src/menus_and_items.rs",
    "src/teams_and_commands.rs",
    "src/block_registry.rs",
    "src/world_mechanisms.rs",
]

VANILLA_REGISTRIES = {
    # vanilla registries.json key -> list of (label, {protocol_id: "minecraft:name"})
    "minecraft:entity_type": "entity",
    "minecraft:data_component_type": "components",
    "minecraft:menu": "menus",
    "minecraft:item": "items",
    "minecraft:block": "blocks",
}


class AuditError(Exception):
    """Fail-closed input error -> exit 2."""


# ---------------------------------------------------------------------------
# Rust source lexing: comment/string-aware skeleton + live text
# ---------------------------------------------------------------------------

# Regex-tokenized lexer: line comments, (nested) block comments, raw strings
# (r"...", r#"..."#, br#/cr#/rb# prefixes), cooked strings ("...", b"...",
# c"...") and char literals. Produces the same (comment_spans, literal_spans)
# contract as a per-char scanner at a fraction of the cost on multi-MB files.
_LEX_RE = re.compile(
    r'//[^\n]*'
    r'|/\*'
    r'|(?:br|cr|rb|r)(#*)"[\s\S]*?"\1'
    r'|(?:b|c)"(?:\\[\s\S]|[^"\\\n])*"'
    r'|"(?:\\[\s\S]|[^"\\\n])*"'
    r"|'(?:\\[\s\S]|[^'\\\n])'"
)
_BLOCK_TOKEN = re.compile(r'/\*|\*/')
_NONL_RE = re.compile(r'[^\n]')
_NONL_KEEP_RE = re.compile(r'[^\n"\'#]')


def _block_comment_end(text, start):
    """End offset (exclusive) of the nested block comment opened at `start`."""
    depth = 1
    for m in _BLOCK_TOKEN.finditer(text, start + 2):
        if m.group(0) == "/*":
            depth += 1
        else:
            depth -= 1
            if depth == 0:
                return m.end()
    return len(text)


def _lex_spans(text):
    """Return (comment_spans, literal_spans); spans are [start, end) char ranges."""
    comments = []
    literals = []
    pos = 0
    n = len(text)
    while pos < n:
        m = _LEX_RE.search(text, pos)
        if not m:
            break
        s, e = m.span()
        tok = text[s:e]
        if tok.startswith("//"):
            comments.append((s, e))
        elif tok.startswith("/*"):
            e = _block_comment_end(text, s)
            comments.append((s, e))
        else:
            literals.append((s, e))
        pos = e
    return comments, literals


def _splice_blank(text, spans, pattern):
    """Blank every [a,b) span's chars (except '\\n') -> same-length text."""
    if not spans:
        return text
    parts = []
    prev = 0
    sub = pattern.sub
    for a, b in spans:
        if a > prev:
            parts.append(text[prev:a])
        parts.append(sub(" ", text[a:b]))
        prev = b
    if prev < len(text):
        parts.append(text[prev:])
    return "".join(parts)


def _skel_from_spans(text, comments, literals):
    """code_skeleton() from precomputed spans: comments blanked fully, literal
    contents blanked except the delimiter characters ``"`` ``'`` ``#``."""
    parts = []
    prev = 0
    i = j = 0
    nc, nl = len(comments), len(literals)
    while i < nc or j < nl:
        if j >= nl or (i < nc and comments[i][0] < literals[j][0]):
            a, b = comments[i]
            i += 1
            pat = _NONL_RE
        else:
            a, b = literals[j]
            j += 1
            pat = _NONL_KEEP_RE
        if a > prev:
            parts.append(text[prev:a])
        parts.append(pat.sub(" ", text[a:b]))
        prev = b
    if prev < len(text):
        parts.append(text[prev:])
    return "".join(parts)


def live_text(text):
    """Source with comments blanked but string contents preserved."""
    comments, _ = _lex_spans(text)
    return _splice_blank(text, comments, _NONL_RE)


def code_skeleton(text):
    """Source with comments blanked and string/char literal *contents* blanked.

    Same length as `text`, so offsets align between skeleton and live_text.
    Delimiter characters (`"`, `'`, `#`) are preserved inside literals so that
    whitespace-skipping still stops at a string boundary; no brace, paren,
    bracket, `.`, `=`, `>` or identifier character survives inside a literal,
    so structure matching stays correct.
    """
    comments, literals = _lex_spans(text)
    return _skel_from_spans(text, comments, literals)


# ---------------------------------------------------------------------------
# Small bracket-matching helpers (operate on skeleton: no braces in strings)
# ---------------------------------------------------------------------------

def match_bracket(skel, open_idx, open_c="{", close_c="}"):
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


def fn_body_span(skel, name):
    """(start, end) char span of a top-level `fn name(...) { ... }` incl. body."""
    pat = re.compile(
        r'(?m)^(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?fn\s+'
        + re.escape(name) + r'\b')
    m = pat.search(skel)
    if not m:
        return None
    brace = skel.find("{", m.end())
    if brace < 0:
        return None
    close = match_bracket(skel, brace)
    if close < 0:
        return None
    return (m.start(), close + 1)


def region_after(skel, marker, open_c="[", close_c="]"):
    """Span of the bracketed literal following `marker`'s `= ` (static array)."""
    idx = skel.find(marker)
    if idx < 0:
        return None
    eq = skel.find("=", idx + len(marker))
    if eq < 0:
        return None
    op = skel.find(open_c, eq)
    if op < 0:
        return None
    cl = match_bracket(skel, op, open_c, close_c)
    if cl < 0:
        return None
    return (op, cl + 1)


_CFG_TEST = re.compile(r"#\[cfg\(test\)\]|#\[cfg\((?:all|any)\([^]]*\btest\b[^]]*\)\)\]")


def _mask_test_spans(skel):
    """Char spans of every #[cfg(test)]-annotated item on a skeleton."""
    n = len(skel)
    spans = []
    for m in _CFG_TEST.finditer(skel):
        # skip whitespace and further #[...] attributes to the item start
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
        # find the item end: first top-level ';' (unit item) or '{' (block)
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
                close = match_bracket(skel, i)
                end = close + 1 if close > 0 else n
                break
            elif c == ";" and par == 0 and brk == 0:
                end = i + 1
                break
            i += 1
        if end < 0:
            end = n
        spans.append((m.start(), end))
    # nested #[cfg(test)] items (e.g. a test fn inside a `mod tests`) yield
    # overlapping spans; merge so splicing stays length-preserving.
    spans.sort()
    merged = []
    for a, b in spans:
        if merged and a < merged[-1][1]:
            if b > merged[-1][1]:
                merged[-1] = (merged[-1][0], b)
        else:
            merged.append((a, b))
    return merged


def mask_test_regions(text, skel=None):
    """Blank out #[cfg(test)]-annotated items; return (masked, test_text).

    `text` must be live_text() output (comments blanked, strings intact).
    Structure is computed on code_skeleton(text) so braces inside string
    literals cannot confuse the matcher. The masked output preserves length
    and line numbers: test code becomes whitespace, string contents of live
    code are untouched.
    """
    if skel is None:
        skel = code_skeleton(text)
    spans = _mask_test_spans(skel)
    if not spans:
        return text, ""
    masked = _splice_blank(text, spans, _NONL_RE)
    test_parts = [text[a:b] for a, b in spans]
    return masked, "\n".join(test_parts)


# ---------------------------------------------------------------------------
# Mode A parsers
# ---------------------------------------------------------------------------

def parse_v767_packets(text):
    """V767_PACKETS -> [{phase, direction, name, id}] (phase/dir lowercased)."""
    out = []
    for m in re.finditer(
            r'\(\s*PacketPhase::(\w+)\s*,\s*PacketDir::(\w+)\s*,\s*"([^"]+)"\s*,\s*(0x[0-9a-fA-F]+|\d+)\s*\)',
            text):
        out.append({
            "phase": m.group(1).lower(),
            "direction": m.group(2).lower(),
            "name": m.group(3),
            "id": int(m.group(4), 0),
        })
    return out


def parse_v767_name_table(text, static_name):
    """V767_* [&str; N] static -> ordered list of names (index = protocol id)."""
    skel = code_skeleton(text)
    span = region_after(skel, f"static {static_name}")
    if not span:
        return None
    body = text[span[0]:span[1]]
    return re.findall(r'"((?:minecraft:)?[^"]+)"', body)


def parse_entity_specs(text):
    """ENTITY_REGISTRY_1211 -> {protocol_id: 'minecraft:name'}."""
    out = {}
    for m in re.finditer(r'entity_spec_1211!\(\s*(\d+)\s*,\s*"([^"]+)"', text):
        out[int(m.group(1))] = "minecraft:" + m.group(2)
    return out


def parse_int_consts(text):
    """`pub const NAME: ty = 0x..|123;` -> {NAME: int} (for packet-id guards)."""
    out = {}
    for m in re.finditer(
            r'\b(?:pub(?:\([^)]*\))?\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*[\w:<>,\s\[\]&\'()*]+?=\s*(0x[0-9a-fA-F]+|\d+)\s*;',
            text):
        try:
            out[m.group(1)] = int(m.group(2), 0)
        except ValueError:
            pass
    return out


def extract_match_arms(skel, match_pos):
    """All top-level arm pattern spans of the `match` whose `{` follows match_pos.

    Returns list of (pattern_start, pattern_end, eq_pos). Operates on skeleton.
    """
    brace = skel.find("{", match_pos)
    if brace < 0:
        return []
    arms = []
    i = brace + 1
    depth = 1
    arm_start = i
    n = len(skel)

    def skip_ws(k):
        while k < n and skel[k] in " \t\n":
            k += 1
        return k

    while i < n and depth >= 1:
        c = skel[i]
        if c == "{":
            depth += 1
            i += 1
            continue
        if c == "}":
            depth -= 1
            i += 1
            if depth == 0:
                break
            continue
        if depth == 1 and skel.startswith("=>", i):
            pat_start, pat_end = arm_start, i
            arms.append((pat_start, pat_end, i))
            j = skip_ws(i + 2)
            if j < n and skel[j] == "{":
                close = match_bracket(skel, j)
                if close < 0:
                    break
                k = skip_ws(close + 1)
                if k < n and skel[k] == ",":
                    k += 1
                arm_start = i = k
                continue
            # expression-bodied arm: consume until top-level ',' or '=>'
            # (block-like exprs without a comma end right before the next '=>').
            # brc starts at 1: the match's own '{' is the unclosed outer brace.
            par = brk = 0
            brc = 1
            last_block_end = -1
            k = j
            stop = -1
            while k < n:
                ch = skel[k]
                if ch == "(":
                    par += 1
                elif ch == ")":
                    par -= 1
                elif ch == "[":
                    brk += 1
                elif ch == "]":
                    brk -= 1
                elif ch == "{":
                    brc += 1
                elif ch == "}":
                    brc -= 1
                    if brc == 0:
                        stop = k
                        break
                    if brc == 1:
                        last_block_end = k
                elif ch == "," and par == 0 and brk == 0 and brc == 1:
                    stop = k
                    break
                elif skel.startswith("=>", k) and par == 0 and brk == 0 and brc == 1:
                    stop = k
                    break
                k += 1
            if stop < 0:
                break
            if skel[stop] == ",":
                arm_start = i = stop + 1
            elif skel.startswith("=>", stop):
                # no-comma block-like arm: pattern for the NEXT arm starts after
                # the last '}' that closed back to depth 1
                if last_block_end >= 0:
                    arm_start = last_block_end + 1
                else:
                    arm_start = stop
                i = stop
            else:  # '}' closing the match
                break
            continue
        i += 1
    return arms


def hex_ids_in_pattern(pat):
    lhs = pat.split(" if ", 1)[0]
    ids = [int(x, 16) for x in re.findall(r"0x[0-9a-fA-F]+", lhs)]
    return ids


def const_refs_in_pattern(pat):
    return re.findall(r"::([A-Z][A-Z0-9_]{3,})\b", pat)


# ---------------------------------------------------------------------------
# Mode A audit sections
# ---------------------------------------------------------------------------

def audit_packets(repo):
    text = (repo / "proto/src/data_v767.rs").read_text()
    packets = parse_v767_packets(text)
    play_c2s = [p for p in packets
                if p["phase"] == "play" and p["direction"] == "serverbound"]

    pp = (repo / "src/play_protocol.rs").read_text()
    skel = code_skeleton(pp)
    handled = set()
    opaque = []
    match_positions = [m.start() for m in re.finditer(r"\bmatch\s+pid\s*\{", skel)]
    consts = parse_int_consts((repo / "src/brigadier_wire.rs").read_text())
    consts.update(parse_int_consts(pp))
    for mp in match_positions:
        for a, b, eq in extract_match_arms(skel, mp):
            pat = skel[a:b].strip()
            if pat == "_" or not pat:
                continue
            ids = set(hex_ids_in_pattern(pat))
            for name in const_refs_in_pattern(pat):
                if name in consts:
                    ids.add(consts[name])
            # bare decimal/ident guard arms we cannot resolve -> opaque
            lhs = pat.split(" if ", 1)[0].strip()
            if not ids and not re.fullmatch(r"[\w\s|]+", lhs):
                opaque.append(pat[:80])
            elif not ids and "::" not in pat:
                # e.g. `some_var` pattern without guard constants
                if not re.fullmatch(r"(?:\w+\s*(?:\|\s*\w+\s*)*)", lhs):
                    opaque.append(pat[:80])
            handled |= ids

    by_id = {p["id"]: p for p in play_c2s}
    handled_list = sorted(
        ({"name": by_id[i]["name"], "id": i} for i in handled if i in by_id),
        key=lambda x: x["id"])
    unhandled = sorted(
        ({"name": p["name"], "id": p["id"]} for p in play_c2s if p["id"] not in handled),
        key=lambda x: x["id"])
    stray = sorted(i for i in handled if i not in by_id)
    return {
        "c2s_play_packets": {
            "total": len(play_c2s),
            "handled_count": len(handled_list),
            "handled_pct": round(100.0 * len(handled_list) / max(1, len(play_c2s)), 1),
            "handled": handled_list,
            "unhandled": unhandled,
            "handled_ids_not_in_proto": stray,
            "opaque_arms": opaque,
            "dispatch_matches_found": len(match_positions),
        },
        "_proto_packets": packets,
    }


_TEXTS_CACHE = {}


def _live_src_texts(repo):
    """{relpath: (live, test, skel)} for every src/**/*.rs file.

    live: comments blanked, test-only items removed (string contents intact).
    test: the removed #[cfg(test)] regions plus whole *_tests.rs files.
    skel: code_skeleton aligned with `live` (None for whole-test files).
    One lexer pass per file feeds all three views. Results are memoized per
    repo keyed on (path, mtime_ns, size) so repeated audits in one process
    (e.g. the test suite's shared Mode A scan plus per-test main() calls)
    don't rescan unchanged sources.
    """
    repo = Path(repo)
    src_dir = repo / "src"
    paths = sorted(src_dir.rglob("*.rs"))
    sig = tuple((p.relative_to(repo).as_posix(), p.stat().st_mtime_ns,
                 p.stat().st_size) for p in paths)
    cached = _TEXTS_CACHE.get(repo)
    if cached is not None and cached[0] == sig:
        return cached[1]
    out = {}
    for path in paths:
        rel = path.relative_to(repo).as_posix()
        raw = path.read_text(errors="replace")
        comments, literals = _lex_spans(raw)
        live_full = _splice_blank(raw, comments, _NONL_RE)
        skel_full = _skel_from_spans(raw, comments, literals)
        is_test_file = path.name.endswith("_tests.rs") or path.name == "test_support.rs"
        if is_test_file:
            out[rel] = ("", live_full, None)
            continue
        spans = _mask_test_spans(skel_full)
        if not spans:
            out[rel] = (live_full, "", skel_full)
            continue
        masked = _splice_blank(live_full, spans, _NONL_RE)
        skel = _splice_blank(skel_full, spans, _NONL_RE)
        test = "\n".join(live_full[a:b] for a, b in spans)
        out[rel] = (masked, test, skel)
    _TEXTS_CACHE[repo] = (sig, out)
    return out


def audit_gamerules(repo, texts):
    cc = (repo / "src/commands_core.rs").read_text()
    cc_live = live_text(cc)
    cc_skel = code_skeleton(cc)

    # COMMAND_GAMERULE_DEFAULTS = [("name", is_bool, default), ...]
    names = []
    m = re.search(r"COMMAND_GAMERULE_DEFAULTS[^=]*=\s*&?\s*\[", cc_skel)
    if m:
        op = cc_skel.find("[", m.end() - 1)
        cl = match_bracket(cc_skel, op, "[", "]")
        if cl > 0:
            body = cc_live[op:cl + 1]
            names = re.findall(r'\(\s*"([A-Za-z][A-Za-z0-9]*)"\s*,\s*(?:true|false)', body)

    # GR_* atomic definitions across all live src files
    gr_defs = {}  # GR_NAME -> relpath
    for rel, (live, _t, _s) in texts.items():
        for gm in re.finditer(r'\bstatic\s+(GR_[A-Z0-9_]+)\s*:', live):
            gr_defs.setdefault(gm.group(1), rel)

    # gamerule name -> GR atomic, via gamerule_atomic_value/store match arms
    atomic_for_name = {}
    main_raw = (repo / "src/main.rs").read_text()
    main_sk_full = code_skeleton(main_raw)
    main_lv_full = live_text(main_raw)
    for fname in ("gamerule_atomic_value", "gamerule_atomic_store"):
        span = fn_body_span(main_sk_full, fname)
        if not span:
            continue
        body = main_lv_full[span[0]:span[1]]
        # "name" => GR_X, including block-bodied arms ("name" => { GR_X ... })
        for am in re.finditer(r'"([A-Za-z][A-Za-z0-9]*)"\s*=>', body):
            window = body[am.end():am.end() + 300]
            gm = re.search(r"\b(GR_[A-Z0-9_]+)\b", window)
            if gm:
                atomic_for_name.setdefault(am.group(1), gm.group(1))

    # Per-file exclusion spans (aligned with each file's stripped live text):
    #  - `static GR_*` definition lines (the definition is not a call site)
    #  - the gamerule_atomic_value/store plumbing bodies in main.rs
    #  - COMMAND_GAMERULE_DEFAULTS tuple lines in commands_core.rs
    exclude = {}  # rel -> list[(start,end)]
    for rel, (live, _t, skel) in texts.items():
        spans = []
        sk = skel if skel is not None else ""
        for gm in re.finditer(r'\bstatic\s+GR_[A-Z0-9_]+\s*:', sk):
            ls = sk.rfind("\n", 0, gm.start()) + 1
            le = sk.find("\n", gm.end())
            spans.append((ls, le if le > 0 else len(sk)))
        if rel == "src/main.rs":
            for fname in ("gamerule_atomic_value", "gamerule_atomic_store"):
                span = fn_body_span(sk, fname)
                if span:
                    spans.append(span)
        if rel == "src/commands_core.rs":
            # defaults table: find it in this file's stripped skeleton
            dm = re.search(r"COMMAND_GAMERULE_DEFAULTS[^=]*=\s*&?\s*\[", sk)
            if dm:
                op = sk.find("[", dm.end() - 1)
                cl = match_bracket(sk, op, "[", "]")
                if cl > 0:
                    spans.append((op, cl + 1))
        if spans:
            exclude[rel] = spans

    # One pass per file counts every GR_* token and every "name" literal that
    # survives the per-file exclusion spans — equivalent to the old per-token
    # count_in_live scans at a fraction of the cost.
    count_re = re.compile(r'\b(GR_[A-Z0-9_]+)\b|"([A-Za-z][A-Za-z0-9]*)"')
    gr_counts = {}
    gr_files = {}
    name_counts = {}
    name_files = {}
    for rel, (live, _t, _s) in texts.items():
        spans = exclude.get(rel, [])
        for m in count_re.finditer(live):
            pos = m.start()
            if any(a <= pos < b for a, b in spans):
                continue
            if m.group(1):
                tok = m.group(1)
                gr_counts[tok] = gr_counts.get(tok, 0) + 1
                gr_files.setdefault(tok, set()).add(rel)
            else:
                tok = m.group(2)
                name_counts[tok] = name_counts.get(tok, 0) + 1
                name_files.setdefault(tok, set()).add(rel)

    rules = []
    rule_names = sorted(set(names) | set(atomic_for_name))
    for name in rule_names:
        gr = atomic_for_name.get(name)
        if gr:
            live_sites = gr_counts.get(gr, 0)
            site_files = sorted(gr_files.get(gr, ()))
        else:
            # hash-map-backed rule: count "name" literal uses in live code
            live_sites = name_counts.get(name, 0)
            site_files = sorted(name_files.get(name, ()))
        rules.append({
            "name": name, "atomic": gr, "in_defaults": name in names,
            "live_sites": live_sites, "site_files": site_files,
            "status": "WIRED" if live_sites > 0 else "DEFINED_NOT_WIRED",
        })
    # GR_* statics not referenced by any gamerule name still get reported
    for gr in sorted(set(gr_defs) - set(atomic_for_name.values())):
        live_sites = gr_counts.get(gr, 0)
        rules.append({
            "name": gr, "atomic": gr, "in_defaults": False,
            "live_sites": live_sites,
            "site_files": sorted(gr_files.get(gr, ())),
            "status": "WIRED" if live_sites > 0 else "DEFINED_NOT_WIRED",
        })

    wired = [r for r in rules if r["status"] == "WIRED"]
    dead = [r for r in rules if r["status"] == "DEFINED_NOT_WIRED"]
    return {
        "gamerules": {
            "total": len(rules),
            "wired_count": len(wired),
            "defined_not_wired_count": len(dead),
            "rules": rules,
        }
    }


def _top_level_then_spans(skel, start, end):
    """All `.then(...)` call spans inside [start,end) of skeleton."""
    spans = []
    for m in re.finditer(r"\.then\s*\(", skel[start:end]):
        op = start + m.end() - 1
        cl = match_bracket(skel, op, "(", ")")
        if cl > 0:
            spans.append((m.start() + start, cl + 1))
    return spans


def audit_commands(repo):
    raw = (repo / "src/commands_core.rs").read_text()
    skel = code_skeleton(raw)
    live = live_text(raw)

    span = fn_body_span(skel, "native_command_dispatcher")
    if not span:
        raise AuditError("native_command_dispatcher not found in src/commands_core.rs")
    s, e = span
    region_skel = skel[s:e]
    region_live = live[s:e]
    then_spans = _top_level_then_spans(skel, s, e)

    def in_then(pos):
        return any(a <= pos < b for a, b in then_spans)

    registered = set()

    # literal("x") / done("x") occurring outside any .then(...) subtree
    for m in re.finditer(r'\b(?:literal|done)\(\s*"([^"]+)"', region_live):
        pos = s + m.start()
        # confirm the call itself is code, not inside a blanked string/comment
        if (skel[pos:pos + 7] == "literal" or skel[pos:pos + 4] == "done") \
                and not in_then(pos):
            registered.add(m.group(1))

    # for VAR in [ "a", "b" ] { ... literal(VAR)/done(VAR)/register_at_level(VAR ... }
    for fm in re.finditer(r"\bfor\s+(\w+)\s+in\s*\[", region_skel):
        var = fm.group(1)
        op = s + fm.end() - 1
        cl = match_bracket(skel, op, "[", "]")
        if cl < 0:
            continue
        # loop body: the '{' after the array
        bstart = skel.find("{", cl + 1, min(cl + 200, e))
        if bstart < 0 or bstart > e:
            continue
        bend = match_bracket(skel, bstart)
        if bend < 0 or bend > e:
            continue
        body = skel[bstart:bend]
        confirmed = False
        for cm in re.finditer(
                r"\b(?:literal|done)\(\s*" + re.escape(var) + r"\s*\)|"
                r"\bregister_at_level\(\s*" + re.escape(var) + r"\b|"
                r"\bregister\(\s*" + re.escape(var) + r"\b", body):
            if not in_then(bstart + cm.start()):
                confirmed = True
                break
        if confirmed:
            arr_live = live[op:cl + 1]
            for name in re.findall(r'"([^"]+)"', arr_live):
                registered.add(name)

    # dispatch arms of `match cmd` — lives in run_command_ctx_level (the
    # permission-pinned variant run_command_ctx delegates to)
    fn_span = None
    for fname in ("run_command_ctx_level", "run_command_ctx"):
        fn_span = fn_body_span(skel, fname)
        if fn_span:
            mm = re.search(r"\bmatch\s+cmd\s*\{", skel[fn_span[0]:fn_span[1]])
            if mm:
                break
            fn_span = None
    if not fn_span:
        raise AuditError("match cmd dispatch not found in run_command_ctx*")
    fs, _fe = fn_span
    match_pos = fs + mm.start()
    dispatched = set()
    for a, b, _eq in extract_match_arms(skel, match_pos):
        pat_live = live[a:b]
        for name in re.findall(r'"([^"]+)"', pat_live):
            if name:
                dispatched.add(name)

    registered_only = sorted(registered - dispatched)
    dispatched_only = sorted(dispatched - registered)
    both = sorted(registered & dispatched)
    return {
        "commands": {
            "registered_count": len(registered),
            "dispatched_count": len(dispatched),
            "covered_count": len(both),
            "coverage_pct": round(100.0 * len(both) / max(1, len(registered)), 1),
            "registered": sorted(registered),
            "dispatched": sorted(dispatched),
            "registered_not_dispatched": registered_only,
            "dispatched_not_registered": dispatched_only,
        }
    }


_TOP_FN = re.compile(
    r"(?m)^(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?fn\s+([A-Za-z_]\w*)\s*\(")


def audit_mechanisms(repo, texts):
    """Classify top-level fns in mechanism files: live_wired / test_only / dead."""
    # gather all live+test text once; call sites counted in two regex passes
    # instead of one findall per function (the old per-fn scan was the Mode A
    # bottleneck: ~500 fns x ~12 MB of source).
    all_live = "\n".join(t[0] for t in texts.values() if t[0])
    all_test = "\n".join(t[1] for t in texts.values() if t[1])
    call_re = re.compile(r"\b([A-Za-z_]\w*)\s*\(")
    live_calls = {}
    for m in call_re.finditer(all_live):
        live_calls[m.group(1)] = live_calls.get(m.group(1), 0) + 1
    test_calls = {}
    for m in call_re.finditer(all_test):
        test_calls[m.group(1)] = test_calls.get(m.group(1), 0) + 1

    per_file = {}
    totals = {"functions": 0, "live_wired": 0, "test_only": 0, "dead": 0}
    for rel in MECHANISM_FILES:
        entry = texts.get(rel)
        if entry is None or not entry[0]:
            continue
        live_code = entry[0]
        fns = []
        for m in _TOP_FN.finditer(live_code):
            name = m.group(1)
            # definition site occurrence of `name(`
            fns.append((name, m.start()))
        wired = only_test = dead = 0
        details = []
        for name, def_pos in fns:
            # `name(` occurrences in live code minus this definition site
            lc = live_calls.get(name, 0) - 1
            tc = test_calls.get(name, 0)
            if lc > 0:
                wired += 1
                status = "live_wired"
            elif tc > 0:
                only_test += 1
                status = "test_only"
            else:
                dead += 1
                status = "dead"
            details.append({"name": name, "status": status})
        per_file[rel] = {
            "functions": len(fns),
            "live_wired": wired,
            "test_only": only_test,
            "dead": dead,
            "fns": details,
        }
        totals["functions"] += len(fns)
        totals["live_wired"] += wired
        totals["test_only"] += only_test
        totals["dead"] += dead
    return {"mechanisms": {"files": per_file, "totals": totals}}


# ---------------------------------------------------------------------------
# Mode A: S2C Play packet send coverage
# ---------------------------------------------------------------------------

# send helpers -> index of the packet-id argument
_SEND_IDXS = {
    "send_packet": 1,
    "send_broadcast_frame": 2,
    "SharedBroadcastFrame::new": 0,
}
_SEND_RE = re.compile(r"\b(send_packet|send_broadcast_frame|SharedBroadcastFrame::new)\s*\(")
# `(ID,` tuple producers: mk closures `|c| (0x42, pkt)`, `=> (ID, ..)`,
# `return (ID, ..)`, `let x = (ID, ..)` and line-leading tail tuples.
_TUPLE_RE = re.compile(r"\(\s*(0x[0-9a-fA-F]+|[A-Za-z_][A-Za-z0-9_]*)\s*,")
_WRITE_VARINT_RE = re.compile(r"\bwrite_varint\s*\(")
_S2C_CONST_RE = re.compile(
    r"\b[A-Za-z_][A-Za-z0-9_]*S2C[A-Za-z0-9_]*\b|\bCB_[A-Za-z0-9_]+\b")
_LET_RE = re.compile(
    r"\blet\s+(?:mut\s+)?([a-zA-Z_]\w*)\s*=\s*"
    r"(0x[0-9a-fA-F]+|[0-9]+|[A-Z][A-Z0-9_]{2,})\s*;")
_FN_DEF_RE = re.compile(
    r"\b(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?"
    r"fn\s+([A-Za-z_]\w*)\s*\(")
_STATIC_LIT_RE = re.compile(
    r"\bstatic\s+([A-Z][A-Z0-9_]*)\s*:[^=;]+?=\s*(0x[0-9a-fA-F]+|[0-9]+)\s*;")
_ARG_TOKEN_RE = re.compile(r"(0x[0-9a-fA-F]+|[0-9]+|[A-Za-z_][A-Za-z0-9_]*)")

# Functions whose send_packet calls carry login/configuration/status-phase
# ids, not Play S2C ids (send_packet is phase-agnostic). Signals inside these
# are excluded from the S2C coverage audit.
_NON_PLAY_FNS = frozenset({
    "negotiate_compression",      # login set_compression (0x03)
    "login_disconnect",           # login disconnect (0x00)
    "login_disconnect_text",      # login disconnect (0x00)
    "velocity_login",             # login plugin request (0x04)
    "online_login",               # login encryption request (0x01)
    "status_version_mismatch",    # status response/pong (0x00/0x01)
})
# Identifier prefixes that name non-Play-phase packet ids.
_NON_PLAY_PREFIXES = ("CONFIG_", "LOGIN_")


def _call_args(skel, open_idx):
    """Top-level argument spans of the call whose '(' is at open_idx."""
    args = []
    start = open_idx + 1
    i = start
    n = len(skel)
    par = brk = brace = 0
    while i < n:
        c = skel[i]
        if c == "(":
            par += 1
        elif c == ")":
            if par == 0 and brk == 0 and brace == 0:
                args.append((start, i))
                return args
            par -= 1
        elif c == "[":
            brk += 1
        elif c == "]":
            brk -= 1
        elif c == "{":
            brace += 1
        elif c == "}":
            brace -= 1
        elif c == "," and par == 0 and brk == 0 and brace == 0:
            args.append((start, i))
            start = i + 1
        i += 1
    return []


def _arg_id_token(skel, span):
    """First token of a call argument, or None."""
    a, b = span
    m = _ARG_TOKEN_RE.match(skel[a:b].lstrip())
    if not m:
        return None
    return m.group(1)


def _resolve_packet_id(tok, consts, letmap, allow_dec=True):
    """Resolve a hex/dec/ident token to an int packet id, or None."""
    if tok is None:
        return None
    tok = tok.strip()
    if tok.startswith(_NON_PLAY_PREFIXES):
        return None
    if re.fullmatch(r"0x[0-9a-fA-F]+", tok):
        return int(tok, 16)
    if re.fullmatch(r"[0-9]+", tok):
        return int(tok) if allow_dec else None
    if tok in letmap:
        return letmap[tok]
    return consts.get(tok)


def _tuple_context_ok(skel, pos):
    """Is the '(' at pos a packet-tuple producer rather than a pattern/arg?

    Accepts closure returns `|..| (ID,`, match arms `=> (ID,`, `return (ID,`,
    `= (ID,` bindings and line-leading tail expressions `(ID, ..)`.
    """
    # closure PARAM list `|(i, v)|` has '|' right after its ')' — reject those
    # up front (closure RETURN `|args| (ID, payload)` ends in ')' ';'/',' etc.)
    depth = 0
    k = pos
    while k < len(skel):
        c = skel[k]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                break
        k += 1
    if k < len(skel):
        j = k + 1
        while j < len(skel) and skel[j] in " \t\n":
            j += 1
        if j < len(skel) and skel[j] == "|":
            return False
    i = pos - 1
    while i >= 0 and skel[i] in " \t\n":
        i -= 1
    if i < 0:
        return True
    c = skel[i]
    if c in "|>=":
        return True
    tail = skel[max(0, pos - 40):pos]
    if re.search(r"\breturn\s*$", tail):
        return True
    # line-leading expression: first non-ws char on its line, preceded by a
    # block boundary ('{' ';' '}') — the shape of a fn/match-arm tail tuple.
    ls = skel.rfind("\n", 0, pos) + 1
    if not skel[ls:pos].strip():
        j = ls - 1
        while j >= 0 and skel[j] in " \t\n":
            j -= 1
        if j < 0 or skel[j] in "{;}":
            return True
    return False


def audit_s2c(repo, texts):
    """S2C Play packet send coverage: which clientbound Play ids have a live
    emission site in non-test src/ code."""
    d767 = (repo / "proto/src/data_v767.rs").read_text()
    packets = parse_v767_packets(d767)
    s2c = [p for p in packets
           if p["phase"] == "play" and p["direction"] == "clientbound"]
    s2c_ids = {p["id"] for p in s2c}

    # packet-id constants across all live files (consts + literal statics)
    consts = {}
    for rel, (live, _t, _s) in texts.items():
        if not live:
            continue
        consts.update(parse_int_consts(live))
        for m in _STATIC_LIT_RE.finditer(live):
            consts.setdefault(m.group(1), int(m.group(2), 0))

    all_live = "\n".join(t[0] for t in texts.values() if t[0])
    call_re = re.compile(r"\b([A-Za-z_]\w*)\s*\(")
    call_counts = {}
    for m in call_re.finditer(all_live):
        call_counts[m.group(1)] = call_counts.get(m.group(1), 0) + 1

    def_counts = {}
    pub_fns = set()
    per_file = {}
    for rel, (live, _t, sk) in texts.items():
        if sk is None:
            continue
        letmap = {}
        letmap_hex = {}  # decimal `let` bindings are too ambiguous for tuples
        for m in _LET_RE.finditer(sk):
            rhs = m.group(2)
            letmap[m.group(1)] = _resolve_packet_id(rhs, consts, {})
            letmap_hex[m.group(1)] = _resolve_packet_id(rhs, consts, {},
                                                      allow_dec=False)
        fns = []  # (pos, name, is_pub) sorted by pos
        for m in _FN_DEF_RE.finditer(sk):
            name = m.group(1)
            is_pub = m.group(0).lstrip().startswith("pub")
            fns.append((m.start(), name, is_pub))
            def_counts[name] = def_counts.get(name, 0) + 1
            if is_pub:
                pub_fns.add(name)
        per_file[rel] = (sk, live, letmap, letmap_hex, fns)

    def fn_live(name):
        # live-wired = has non-def `name(` occurrences in live code, or is pub
        return call_counts.get(name, 0) - def_counts.get(name, 0) > 0 \
            or name in pub_fns

    # handle_inner's login/config handshake ends at the `// ---- play ----`
    # marker; send sites before it carry login/config ids, not Play ids.
    nonplay_spans = {}  # rel -> [(start,end)]
    pp_raw = (repo / "src/play_protocol.rs").read_text(errors="replace")
    marker = pp_raw.find("---- play ----")
    if marker > 0:
        pp_skel = per_file.get("src/play_protocol.rs", (None,))[0]
        if pp_skel is not None:
            hs = fn_body_span(pp_skel, "handle_inner")
            if hs and hs[0] < marker:
                nonplay_spans["src/play_protocol.rs"] = [(hs[0], marker)]

    emitted = {}   # proto play clientbound id -> set(signal kinds)
    unresolved_sites = 0
    excluded_sites = 0

    def note(packet_id, via):
        if packet_id in s2c_ids:
            emitted.setdefault(packet_id, set()).add(via)

    for rel, (sk, live, letmap, letmap_hex, fns) in per_file.items():
        positions = [p for p, _n, _u in fns]

        def enclosing(pos):
            i = bisect.bisect_right(positions, pos) - 1
            return fns[i] if i >= 0 else None

        def in_nonplay(pos):
            if any(a <= pos < b for a, b in nonplay_spans.get(rel, ())):
                return True
            fn = enclosing(pos)
            return fn is not None and fn[1] in _NON_PLAY_FNS

        # 1) direct send-helper call sites: id argument position
        for m in _SEND_RE.finditer(sk):
            pos = m.start()
            if in_nonplay(pos):
                excluded_sites += 1
                continue
            idx = _SEND_IDXS[m.group(1)]
            args = _call_args(sk, m.end() - 1)
            if len(args) <= idx:
                unresolved_sites += 1
                continue
            tok = _arg_id_token(sk, args[idx])
            pid = _resolve_packet_id(tok, consts, letmap)
            if pid is None:
                unresolved_sites += 1
            else:
                note(pid, "send:" + m.group(1))

        # 2) S2C/CB packet-id constants used anywhere in live code
        for m in _S2C_CONST_RE.finditer(live):
            name = m.group(0)
            if "S2C" not in name and not name.startswith("CB_"):
                continue
            if name.startswith(_NON_PLAY_PREFIXES):
                continue
            pid = consts.get(name)
            if pid is not None:
                note(pid, "const:" + name)

        # 3) packet-tuple producers `(ID, payload)` in live-wired fns
        for m in _TUPLE_RE.finditer(sk):
            pos = m.start()
            if in_nonplay(pos) or not _tuple_context_ok(sk, pos):
                continue
            fn = enclosing(pos)
            if fn is not None and not fn_live(fn[1]):
                continue
            tok = m.group(1)
            if re.fullmatch(r"[0-9]+", tok):
                continue  # bare decimals are too ambiguous in tuple position
            pid = _resolve_packet_id(tok, consts, letmap_hex, allow_dec=False)
            if pid is not None:
                note(pid, "tuple")

        # 4) write_varint-embedded packet ids inside encode_* builders
        for m in _WRITE_VARINT_RE.finditer(sk):
            pos = m.start()
            if in_nonplay(pos):
                continue
            fn = enclosing(pos)
            if fn is None or not fn[1].startswith("encode") or not fn_live(fn[1]):
                continue
            args = _call_args(sk, m.end() - 1)
            if len(args) < 2:
                continue
            tok = _arg_id_token(sk, args[1])
            if tok is not None and re.fullmatch(r"[0-9]+", tok):
                continue
            pid = _resolve_packet_id(tok, consts, letmap_hex, allow_dec=False)
            if pid is not None:
                note(pid, "encode_varint")

    emitted_list = []
    unemitted = []
    for p in sorted(s2c, key=lambda x: x["id"]):
        if p["id"] in emitted:
            emitted_list.append({
                "name": p["name"], "id": p["id"],
                "via": sorted(emitted[p["id"]]),
            })
        else:
            unemitted.append({"name": p["name"], "id": p["id"]})
    return {
        "s2c_play_packets": {
            "total": len(s2c),
            "emitted_count": len(emitted_list),
            "emitted_pct": round(100.0 * len(emitted_list) / max(1, len(s2c)), 1),
            "emitted": emitted_list,
            "unemitted": unemitted,
            "unresolved_send_sites": unresolved_sites,
            "non_play_excluded_sites": excluded_sites,
        }
    }


# ---------------------------------------------------------------------------
# Mode A: behavioral parity feature matrix
# ---------------------------------------------------------------------------

# (bullet group, feature, lowercase needles). impl = live non-test src text,
# test = #[cfg(test)] regions + *_tests.rs + tests/**/*.py.
_BEHAVIOR_MATRIX = [
    ("combat", "combat", ["combat"]),
    ("combat", "wind_charge", ["wind_charge", "windcharge", "wind_burst", "windburst"]),
    ("combat", "shield", ["shield"]),
    ("combat", "totem", ["totem"]),
    ("combat", "trident", ["trident"]),
    ("redstone", "redstone", ["redstone"]),
    ("redstone", "hopper", ["hopper"]),
    ("redstone", "comparator", ["comparator"]),
    ("redstone", "observer", ["observer"]),
    ("redstone", "piston", ["piston"]),
    ("end", "end_portal", ["end_portal", "endportal"]),
    ("end", "end_gateway", ["end_gateway", "endgateway"]),
    ("end", "eye_of_ender", ["eye_of_ender", "eyeofender", "eye of ender"]),
    ("end", "chorus", ["chorus"]),
    ("end", "shulker", ["shulker"]),
    ("nether", "nether_portal", ["nether_portal", "netherportal"]),
    ("nether", "respawn_anchor", ["respawn_anchor", "respawnanchor"]),
    ("nether", "piglin", ["piglin"]),
    ("tame", "tame", ["tame"]),
    ("tame", "leash", ["leash"]),
    ("tame", "horse", ["horse"]),
    ("tame", "parrot", ["parrot"]),
    ("commands", "command_block", ["command_block", "commandblock"]),
    ("commands", "datapack", ["datapack"]),
    ("commands", "datapack_zip", ["zip"]),
    ("commands", "dpc1", ["dpc1"]),
    ("save", "save", ["save_v2", "world_save", "save_game", "player_save"]),
    ("save", "anvil", ["anvil"]),
    ("save", "playerdata", ["playerdata", "player_data"]),
    ("save", "chunk_unload", ["chunk_unload", "unload_chunk",
                             "auto_unload", "sweep_unticketed"]),
]


def audit_behavior_matrix(repo, texts):
    """Per-subsystem COVERED/GAP: needs BOTH a live non-test implementation
    and dedicated parity-test evidence."""
    live_blob_parts = []
    test_blob_parts = []
    for live, test, _s in texts.values():
        if live:
            live_blob_parts.append(live)
        if test:
            test_blob_parts.append(test)
    tests_dir = repo / "tests"
    if tests_dir.is_dir():
        for path in sorted(tests_dir.rglob("*.py")):
            try:
                test_blob_parts.append(path.read_text(errors="replace"))
            except OSError:
                continue
    live_blob = "\n".join(live_blob_parts).lower()
    test_blob = "\n".join(test_blob_parts).lower()

    subsystems = []
    groups = {}
    covered = 0
    for group, feature, needles in _BEHAVIOR_MATRIX:
        impl_hits = [n for n in needles if n in live_blob]
        test_hits = [n for n in needles if n in test_blob]
        status = "COVERED" if impl_hits and test_hits else "GAP"
        if status == "COVERED":
            covered += 1
        subsystems.append({
            "group": group,
            "feature": feature,
            "status": status,
            "impl": bool(impl_hits),
            "test": bool(test_hits),
            "impl_evidence": impl_hits,
            "test_evidence": test_hits,
        })
        g = groups.setdefault(group, {"covered": 0, "total": 0})
        g["total"] += 1
        if status == "COVERED":
            g["covered"] += 1
    return {
        "behavior_matrix": {
            "subsystems": subsystems,
            "totals": {
                "subsystems": len(subsystems),
                "covered": covered,
                "gaps": len(subsystems) - covered,
            },
            "groups": groups,
        }
    }


def mode_a(repo):
    texts = _live_src_texts(repo)
    report = {"schema": SCHEMA, "mode": "offline", "offline": {}}
    pkt = audit_packets(repo)
    c2s = dict(pkt["c2s_play_packets"])
    c2s.pop("_proto_packets", None)
    report["offline"]["packets"] = c2s
    report["offline"].update(audit_gamerules(repo, texts))
    report["offline"].update(audit_commands(repo))
    report["offline"].update(audit_mechanisms(repo, texts))
    report["offline"].update(audit_s2c(repo, texts))
    report["offline"].update(audit_behavior_matrix(repo, texts))
    # consistency: V767 entity table vs entity_registry (same protocol ids)
    d767 = (repo / "proto/src/data_v767.rs").read_text()
    v767_entities = parse_v767_name_table(d767, "V767_ENTITY_TYPES") or []
    ent = parse_entity_specs((repo / "src/entity_registry.rs").read_text())
    mism = []
    for i, name in enumerate(v767_entities):
        if ent.get(i) != name:
            mism.append({"id": i, "v767": name, "registry": ent.get(i)})
    report["offline"]["entity_table_consistency"] = {
        "v767_count": len(v767_entities),
        "registry_count": len(ent),
        "mismatches": mism,
    }
    return report


# ---------------------------------------------------------------------------
# Mode B: vanilla reports
# ---------------------------------------------------------------------------

def _req_obj(v, what):
    if not isinstance(v, dict):
        raise AuditError(f"{what}: expected JSON object, got {type(v).__name__}")
    return v


def _req_int(v, what):
    if not isinstance(v, int) or isinstance(v, bool):
        raise AuditError(f"{what}: expected integer, got {v!r}")
    return v


def parse_packets_report(report):
    """packets.json -> {(phase, direction, name): protocol_id}; fail-closed."""
    out = {}
    for phase, directions in _req_obj(report, "packets.json").items():
        for direction, entries in _req_obj(directions, f"packets.json[{phase}]").items():
            for name, rec in _req_obj(entries, f"packets.json[{phase}][{direction}]").items():
                rec = _req_obj(rec, f"packets.json[{phase}][{direction}][{name}]")
                pid = _req_int(rec.get("protocol_id"),
                               f"packets.json[{phase}][{direction}][{name}].protocol_id")
                key = (str(phase).lower(), str(direction).lower(), str(name))
                if key in out:
                    raise AuditError(f"packets.json: duplicate {key}")
                out[key] = pid
    if not out:
        raise AuditError("packets.json: no packets present")
    return out


def parse_commands_report(report):
    """commands.json brigadier tree -> sorted root literal names."""
    root = _req_obj(report, "commands.json")
    children = _req_obj(root.get("children"), "commands.json.children")
    names = []
    for name, node in children.items():
        node = _req_obj(node, f"commands.json.children[{name}]")
        if node.get("type") == "literal":
            names.append(str(name))
    if not names:
        raise AuditError("commands.json: no root literals found")
    return sorted(names)


def parse_registries_report(report):
    """registries.json -> {registry: {protocol_id: 'minecraft:name'}}."""
    out = {}
    for reg, rec in _req_obj(report, "registries.json").items():
        rec = _req_obj(rec, f"registries.json[{reg}]")
        entries = _req_obj(rec.get("entries"), f"registries.json[{reg}].entries")
        table = {}
        for name, ent in entries.items():
            ent = _req_obj(ent, f"registries.json[{reg}].entries[{name}]")
            eid = _req_int(ent.get("protocol_id"),
                           f"registries.json[{reg}].entries[{name}].protocol_id")
            table[eid] = str(name)
        out[str(reg)] = table
    if not out:
        raise AuditError("registries.json: no registries present")
    return out


def parse_blocks_report(report):
    """reports/blocks.json (optional) -> per-block state counts + totals.

    Vanilla shape: {block_name: {..., "states": [{"id": int, "default": bool,
    ...}]}} with globally contiguous ascending state ids. Fail-closed on a
    corrupt or non-contiguous report, same as the other Mode B inputs.
    """
    blocks = {}
    for name, rec in _req_obj(report, "blocks.json").items():
        rec = _req_obj(rec, f"blocks.json[{name}]")
        states = rec.get("states")
        if not isinstance(states, list) or not states:
            raise AuditError(f"blocks.json[{name}]: expected a non-empty 'states' list")
        ids = []
        for i, st in enumerate(states):
            st = _req_obj(st, f"blocks.json[{name}].states[{i}]")
            ids.append(_req_int(st.get("id"),
                                f"blocks.json[{name}].states[{i}].id"))
        for i in range(1, len(ids)):
            if ids[i] != ids[i - 1] + 1:
                raise AuditError(
                    f"blocks.json[{name}]: state ids are not contiguous "
                    f"(... {ids[i - 1]}, {ids[i]} ...)")
        blocks[str(name)] = len(ids)
    if not blocks:
        raise AuditError("blocks.json: no blocks present")
    return {
        "blocks": blocks,
        "block_count": len(blocks),
        "state_total": sum(blocks.values()),
    }


def load_reports_dir(path):
    path = Path(path)
    if not path.is_dir():
        raise AuditError(f"--reports-dir {path} is not a directory")
    data = {}
    for fn, parser in (("packets.json", parse_packets_report),
                       ("commands.json", parse_commands_report),
                       ("registries.json", parse_registries_report)):
        fp = path / fn
        if not fp.is_file():
            raise AuditError(f"missing required report {fp}")
        try:
            data[fn] = parser(json.loads(fp.read_text()))
        except json.JSONDecodeError as exc:
            raise AuditError(f"{fp}: corrupt JSON: {exc}") from exc
        except AuditError:
            raise
        except Exception as exc:  # noqa: BLE001 - fail closed on any parse issue
            raise AuditError(f"{fp}: {exc}") from exc
    # blocks.json is optional: vanilla --reports emits it, hand-built dirs may not
    blocks_fp = path / "blocks.json"
    if blocks_fp.is_file():
        try:
            data["blocks.json"] = parse_blocks_report(json.loads(blocks_fp.read_text()))
        except json.JSONDecodeError as exc:
            raise AuditError(f"{blocks_fp}: corrupt JSON: {exc}") from exc
        except AuditError:
            raise
        except Exception as exc:  # noqa: BLE001
            raise AuditError(f"{blocks_fp}: {exc}") from exc
    return data, f"reports-dir:{path}"


def run_vanilla_jar(jar):
    jar = Path(jar)
    if not jar.is_file():
        raise AuditError(f"--vanilla-jar {jar} does not exist")
    tmp = Path(tempfile.mkdtemp(prefix="megu-parity-"))
    try:
        out_dir = tmp / "gen"
        cmd = ["java", "-DbundlerMainClass=net.minecraft.data.Main",
               "-jar", str(jar), "--reports", "--output", str(out_dir)]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True,
                                  timeout=600, shell=False)
        except FileNotFoundError as exc:
            raise AuditError("java not found on PATH; cannot run --vanilla-jar") from exc
        except subprocess.TimeoutExpired as exc:
            raise AuditError("vanilla --reports run timed out after 600s") from exc
        if proc.returncode != 0:
            tail = (proc.stderr or proc.stdout or "")[-2000:]
            raise AuditError(f"vanilla --reports exited {proc.returncode}:\n{tail}")
        reports = out_dir / "reports"
        if not reports.is_dir():
            raise AuditError(f"vanilla --reports produced no reports dir at {reports}")
        data, _ = load_reports_dir(reports)
        return data, f"jar:{jar}"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _norm_name(name):
    return name.split(":", 1)[1] if ":" in name else name


def audit_vanilla(repo, report_offline, data, source):
    d767_text = (repo / "proto/src/data_v767.rs").read_text()
    proto_packets = parse_v767_packets(d767_text)
    proto_idx = {}  # (phase,direction,name) -> id
    proto_by_id = {}  # (phase,direction,id) -> name
    for p in proto_packets:
        proto_idx[(p["phase"], p["direction"], p["name"])] = p["id"]
        proto_by_id[(p["phase"], p["direction"], p["id"])] = p["name"]

    vanilla_packets = data["packets.json"]
    missing_in_proto, id_mismatch, name_mismatch, extra_in_proto = [], [], [], []
    matched_keys = set()
    for (phase, direction, vname), vid in sorted(vanilla_packets.items()):
        pname = _norm_name(vname)
        key = (phase, direction, pname)
        if key in proto_idx:
            matched_keys.add(key)
            pid = proto_idx[key]
            if pid != vid:
                id_mismatch.append({
                    "phase": phase, "direction": direction, "name": pname,
                    "vanilla_id": vid, "proto_id": pid})
        elif (phase, direction, vid) in proto_by_id:
            name_mismatch.append({
                "phase": phase, "direction": direction,
                "vanilla_name": vname, "proto_name": proto_by_id[(phase, direction, vid)],
                "id": vid})
        else:
            missing_in_proto.append({
                "phase": phase, "direction": direction, "name": vname, "id": vid})
    # proto entries not represented in vanilla at all
    vanilla_keys = {(ph, dr, _norm_name(n)) for (ph, dr, n) in vanilla_packets}
    vanilla_ids = {(ph, dr, i) for (ph, dr, n), i in vanilla_packets.items()}
    for (phase, direction, name), pid in sorted(proto_idx.items()):
        if (phase, direction, name) in vanilla_keys:
            continue
        if (phase, direction, pid) in vanilla_ids:
            continue  # same id, different name -> covered by name_mismatch
        extra_in_proto.append({"phase": phase, "direction": direction,
                               "name": name, "id": pid})

    # unhandled_in_live_server: vanilla play serverbound ids absent from the
    # live dispatch handled set (recomputed from Mode A output)
    handled_ids = set()
    for pkt in report_offline["offline"]["packets"]["handled"]:
        handled_ids.add(pkt["id"])
    unhandled_in_live = []
    for (phase, direction, vname), vid in sorted(vanilla_packets.items()):
        if phase == "play" and direction == "serverbound":
            pname = _norm_name(vname)
            key = (phase, direction, pname)
            if key in proto_idx and proto_idx[key] not in handled_ids:
                unhandled_in_live.append({"name": pname, "id": vid})

    # commands: vanilla root literals vs registered set
    vanilla_cmds = data["commands.json"]
    registered = set(report_offline["offline"]["commands"]["registered"])
    missing_cmds = sorted(c for c in vanilla_cmds if c not in registered)
    extra_cmds = sorted(c for c in registered if c not in vanilla_cmds)

    # registries
    d767 = d767_text
    ours_tables = {
        "minecraft:entity_type": [
            ("entity_registry", {i: n for i, n in parse_entity_specs(
                (repo / "src/entity_registry.rs").read_text()).items()}),
            ("v767_entity_types", {i: n for i, n in enumerate(
                parse_v767_name_table(d767, "V767_ENTITY_TYPES") or [])}),
        ],
        "minecraft:data_component_type": [
            ("v767_components", {i: n for i, n in enumerate(
                parse_v767_name_table(d767, "V767_COMPONENTS") or [])}),
        ],
        "minecraft:menu": [
            ("v767_menus", {i: n for i, n in enumerate(
                parse_v767_name_table(d767, "V767_MENUS") or [])}),
        ],
        "minecraft:item": [
            ("v767_items", {i: n for i, n in enumerate(
                parse_v767_name_table(d767, "V767_ITEMS") or [])}),
        ],
        "minecraft:block": [
            ("v767_blocks", {i: n for i, n in enumerate(
                parse_v767_name_table(d767, "V767_BLOCKS") or [])}),
        ],
    }
    registries = {}
    for reg, vanilla_table in sorted(data["registries.json"].items()):
        if reg not in VANILLA_REGISTRIES:
            continue
        tables = ours_tables.get(reg, [])
        v_ids = set(vanilla_table)
        missing, mismatched, extra = [], [], []
        # name -> id in vanilla for mismatch detection
        v_by_name = {n: i for i, n in vanilla_table.items()}
        covered_ids = set()
        for eid, ename in sorted(vanilla_table.items()):
            hit = False
            for _label, table in tables:
                if table.get(eid) == ename:
                    hit = True
                    covered_ids.add(eid)
                    break
            if not hit:
                # does any table have this name at another id?
                alt = [f"{label}:id={tid}" for label, table in tables
                       for tid, tn in table.items() if tn == ename and tid != eid]
                entry = {"id": eid, "name": ename}
                if alt:
                    entry["id_mismatch_in"] = alt
                    mismatched.append(entry)
                else:
                    missing.append(entry)
        our_ids = set()
        for _label, table in tables:
            our_ids |= set(table)
        for eid in sorted(our_ids - v_ids):
            names = sorted({t[eid] for _l, t in tables if eid in t})
            extra.append({"id": eid, "names": names})
        registries[reg] = {
            "vanilla_count": len(vanilla_table),
            "ours_count": len(our_ids),
            "missing_ids": missing,
            "id_mismatch": mismatched,
            "extra_ids": extra,
        }

    # blocks.json differential: vanilla block count + total state ids vs the
    # V767_BLOCKS table, the FIRST_MOD_BLOCK_STATE_ID ceiling and (when the
    # generated asset is present) per-block state counts.
    blocks_diff = None
    if "blocks.json" in data:
        vb = data["blocks.json"]
        v767_blocks = parse_v767_name_table(d767, "V767_BLOCKS") or []
        lt = (repo / "src/landing_types.rs").read_text(errors="replace")
        cm = re.search(r"FIRST_MOD_BLOCK_STATE_ID[^=]*=\s*([0-9_]+)", lt)
        ceiling = int(cm.group(1).replace("_", "")) if cm else None
        asset_blocks = {}
        asset_fp = repo / "assets/block_states_1211.json"
        if asset_fp.is_file():
            try:
                adoc = json.loads(asset_fp.read_text())
                for row in adoc.get("blocks", []):
                    if isinstance(row, list) and len(row) >= 3:
                        asset_blocks[str(row[0])] = int(row[2])
            except Exception:  # noqa: BLE001 - asset is advisory here
                asset_blocks = {}
        per_block = []
        for name, cnt in sorted(vb["blocks"].items()):
            ours = asset_blocks.get(name)
            if ours is not None and ours != cnt:
                per_block.append({
                    "block": name,
                    "vanilla_states": cnt,
                    "ours_states": ours,
                })
        blocks_diff = {
            "vanilla": {
                "block_count": vb["block_count"],
                "state_total": vb["state_total"],
            },
            "ours": {
                "v767_block_count": len(v767_blocks),
                "state_ceiling": ceiling,
                "asset_block_count": len(asset_blocks) or None,
                "asset_state_total": sum(asset_blocks.values()) or None,
            },
            "per_block_state_mismatches": per_block[:50],
            "per_block_state_mismatch_count": len(per_block),
        }
        blocks_diff["match"] = (
            vb["block_count"] == len(v767_blocks)
            and ceiling is not None and ceiling == vb["state_total"]
            and not per_block)

    vanilla_out = {
        "source": source,
        "protocol": PROTOCOL,
        "packets": {
            "vanilla_total": len(vanilla_packets),
            "missing_in_proto": missing_in_proto,
            "missing_in_proto_count": len(missing_in_proto),
            "id_mismatch_in_proto": id_mismatch,
            "id_mismatch_in_proto_count": len(id_mismatch),
            "name_mismatch_in_proto": name_mismatch,
            "name_mismatch_in_proto_count": len(name_mismatch),
            "extra_in_proto": extra_in_proto,
            "extra_in_proto_count": len(extra_in_proto),
            "unhandled_in_live_server": unhandled_in_live,
            "unhandled_in_live_server_count": len(unhandled_in_live),
        },
        "commands": {
            "vanilla_root_count": len(vanilla_cmds),
            "missing_vanilla_commands": missing_cmds,
            "missing_vanilla_commands_count": len(missing_cmds),
            "extra_registered_commands": extra_cmds,
            "extra_registered_commands_count": len(extra_cmds),
        },
        "registries": registries,
    }
    if blocks_diff is not None:
        vanilla_out["blocks"] = blocks_diff
    return {"vanilla": vanilla_out}


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

def _pct(num, den):
    return round(100.0 * num / max(1, den), 1)


def render_text(report):
    lines = []
    off = report["offline"]
    p = off["packets"]
    lines.append("mc-spine vanilla parity audit (megu-parity-audit/1)")
    lines.append("=" * 60)
    lines.append(f"Mode A - offline static & live-wiring audit")
    lines.append(f"  C2S Play packets (protocol 767): "
                 f"{p['handled_count']}/{p['total']} handled "
                 f"({p['handled_pct']}%)")
    if p["unhandled"]:
        lines.append(f"    UNHANDLED ({len(p['unhandled'])}): " +
                     ", ".join(f"{x['name']}({hex(x['id'])})" for x in p["unhandled"]))
    g = off["gamerules"]
    lines.append(f"  Gamerules: {g['wired_count']}/{g['total']} wired, "
                 f"{g['defined_not_wired_count']} defined-not-wired")
    for r in g["rules"]:
        if r["status"] == "DEFINED_NOT_WIRED":
            lines.append(f"    DEFINED_NOT_WIRED: {r['name']}"
                         + (f" ({r['atomic']})" if r.get("atomic") else ""))
    c = off["commands"]
    lines.append(f"  Commands: {c['covered_count']}/{c['registered_count']} registered roots dispatched "
                 f"({c['coverage_pct']}%)")
    if c["registered_not_dispatched"]:
        lines.append("    registered_not_dispatched: " + ", ".join(c["registered_not_dispatched"]))
    if c["dispatched_not_registered"]:
        lines.append("    dispatched_not_registered: " + ", ".join(c["dispatched_not_registered"]))
    m = off["mechanisms"]["totals"]
    lines.append(f"  Mechanism fns: {m['functions']} total, "
                 f"{m['live_wired']} live-wired, {m['test_only']} test-only, {m['dead']} dead")
    ec = off["entity_table_consistency"]
    lines.append(f"  Entity table consistency: v767={ec['v767_count']} registry={ec['registry_count']} "
                 f"mismatches={len(ec['mismatches'])}")
    s2c = off.get("s2c_play_packets")
    if s2c:
        lines.append(f"  S2C Play packets: {s2c['emitted_count']}/{s2c['total']} emitted "
                     f"({s2c['emitted_pct']}%)")
        if s2c["unemitted"]:
            lines.append(f"    UNEMITTED ({len(s2c['unemitted'])}): " +
                         ", ".join(f"{x['name']}({hex(x['id'])})" for x in s2c["unemitted"]))
        lines.append(f"    unresolved_send_sites={s2c['unresolved_send_sites']} "
                     f"non_play_excluded={s2c['non_play_excluded_sites']}")
    bm = off.get("behavior_matrix")
    if bm:
        t = bm["totals"]
        lines.append(f"  Behavioral parity matrix: {t['covered']}/{t['subsystems']} COVERED, "
                     f"{t['gaps']} GAP")
        for s in bm["subsystems"]:
            if s["status"] == "GAP":
                missing = []
                if not s["impl"]:
                    missing.append("impl")
                if not s["test"]:
                    missing.append("test")
                lines.append(f"    GAP {s['group']}/{s['feature']}: missing {'+'.join(missing)}")

    if "vanilla" in report:
        v = report["vanilla"]
        lines.append("")
        lines.append(f"Mode B - vanilla differential ({v['source']})")
        vp = v["packets"]
        lines.append(f"  Packets vs vanilla ({vp['vanilla_total']}): "
                     f"missing_in_proto={vp['missing_in_proto_count']} "
                     f"id_mismatch={vp['id_mismatch_in_proto_count']} "
                     f"name_mismatch={vp['name_mismatch_in_proto_count']} "
                     f"extra_in_proto={vp['extra_in_proto_count']} "
                     f"unhandled_in_live={vp['unhandled_in_live_server_count']}")
        for item in vp["missing_in_proto"][:50]:
            lines.append(f"    missing_in_proto: {item['phase']}/{item['direction']} "
                         f"{item['name']} id={hex(item['id'])}")
        for item in vp["unhandled_in_live_server"][:50]:
            lines.append(f"    unhandled_in_live_server: {item['name']} id={hex(item['id'])}")
        vc = v["commands"]
        lines.append(f"  Commands vs vanilla ({vc['vanilla_root_count']} roots): "
                     f"missing_vanilla_commands={vc['missing_vanilla_commands_count']} "
                     f"extra_registered={vc['extra_registered_commands_count']}")
        for name in vc["missing_vanilla_commands"]:
            lines.append(f"    missing_vanilla_command: {name}")
        for reg, rr in sorted(v["registries"].items()):
            lines.append(f"  Registry {reg}: vanilla={rr['vanilla_count']} ours={rr['ours_count']} "
                         f"missing={len(rr['missing_ids'])} id_mismatch={len(rr['id_mismatch'])} "
                         f"extra={len(rr['extra_ids'])}")
            for item in rr["missing_ids"][:30]:
                lines.append(f"    missing: id={item['id']} {item['name']}")
            for item in rr["id_mismatch"][:30]:
                lines.append(f"    id_mismatch: id={item['id']} {item['name']} -> {item['id_mismatch_in']}")
        if "blocks" in v:
            b = v["blocks"]
            lines.append(f"  Blocks: vanilla={b['vanilla']['block_count']} blocks/"
                         f"{b['vanilla']['state_total']} states vs ours "
                         f"v767_blocks={b['ours']['v767_block_count']} "
                         f"ceiling={b['ours']['state_ceiling']} "
                         f"match={b['match']}")
            for item in b["per_block_state_mismatches"][:30]:
                lines.append(f"    block_state_mismatch: {item['block']} "
                             f"vanilla={item['vanilla_states']} ours={item['ours_states']}")
    return "\n".join(lines)


def render_markdown(report):
    lines = ["# mc-spine vanilla parity audit", "",
             f"Schema: `{SCHEMA}`", ""]
    off = report["offline"]
    p = off["packets"]
    lines += ["## Mode A - offline audit", "",
              f"- **C2S Play packets**: {p['handled_count']}/{p['total']} handled ({p['handled_pct']}%)", ""]
    if p["unhandled"]:
        lines.append("Unhandled C2S Play packets:")
        lines.append("")
        lines.append("| packet | id |")
        lines.append("|---|---|")
        for x in p["unhandled"]:
            lines.append(f"| {x['name']} | `{hex(x['id'])}` |")
        lines.append("")
    g = off["gamerules"]
    lines += [f"- **Gamerules**: {g['wired_count']}/{g['total']} wired; "
              f"{g['defined_not_wired_count']} defined-not-wired", ""]
    if g["defined_not_wired_count"]:
        lines.append("| gamerule | atomic | live sites |")
        lines.append("|---|---|---|")
        for r in g["rules"]:
            if r["status"] == "DEFINED_NOT_WIRED":
                lines.append(f"| {r['name']} | {r.get('atomic') or '-'} | {r['live_sites']} |")
        lines.append("")
    c = off["commands"]
    lines += [f"- **Commands**: {c['covered_count']}/{c['registered_count']} registered roots "
              f"dispatched ({c['coverage_pct']}%)", ""]
    if c["registered_not_dispatched"]:
        lines.append("Registered but not dispatched: `" + "`, `".join(c["registered_not_dispatched"]) + "`")
        lines.append("")
    if c["dispatched_not_registered"]:
        lines.append("Dispatched but not registered: `" + "`, `".join(c["dispatched_not_registered"]) + "`")
        lines.append("")
    m = off["mechanisms"]["totals"]
    lines += [f"- **Mechanism fns**: {m['functions']} total; {m['live_wired']} live-wired, "
              f"{m['test_only']} test-only, {m['dead']} dead", ""]
    ec = off["entity_table_consistency"]
    lines += [f"- **Entity table consistency**: v767={ec['v767_count']} "
              f"registry={ec['registry_count']} mismatches={len(ec['mismatches'])}", ""]
    s2c = off.get("s2c_play_packets")
    if s2c:
        lines += [f"- **S2C Play packets**: {s2c['emitted_count']}/{s2c['total']} emitted "
                  f"({s2c['emitted_pct']}%)", ""]
        if s2c["unemitted"]:
            lines.append("Unemitted S2C Play packets:")
            lines.append("")
            lines.append("| packet | id |")
            lines.append("|---|---|")
            for x in s2c["unemitted"]:
                lines.append(f"| {x['name']} | `{hex(x['id'])}` |")
            lines.append("")
    bm = off.get("behavior_matrix")
    if bm:
        t = bm["totals"]
        lines += [f"- **Behavioral parity matrix**: {t['covered']}/{t['subsystems']} COVERED, "
                  f"{t['gaps']} GAP", ""]
        lines.append("| group | feature | status |")
        lines.append("|---|---|---|")
        for s in bm["subsystems"]:
            lines.append(f"| {s['group']} | {s['feature']} | {s['status']} |")
        lines.append("")

    if "vanilla" in report:
        v = report["vanilla"]
        lines += ["## Mode B - vanilla differential", "",
                  f"Source: `{v['source']}`", ""]
        vp = v["packets"]
        lines += [f"Vanilla packets: {vp['vanilla_total']}; "
                  f"missing_in_proto={vp['missing_in_proto_count']}, "
                  f"id_mismatch={vp['id_mismatch_in_proto_count']}, "
                  f"unhandled_in_live={vp['unhandled_in_live_server_count']}", ""]
        vc = v["commands"]
        lines += [f"Vanilla command roots: {vc['vanilla_root_count']}; "
                  f"missing={vc['missing_vanilla_commands_count']}", ""]
        for reg, rr in sorted(v["registries"].items()):
            lines.append(f"- `{reg}`: vanilla={rr['vanilla_count']} ours={rr['ours_count']} "
                         f"missing={len(rr['missing_ids'])} "
                         f"id_mismatch={len(rr['id_mismatch'])} extra={len(rr['extra_ids'])}")
        if "blocks" in v:
            b = v["blocks"]
            lines.append(f"- **blocks.json**: vanilla={b['vanilla']['block_count']} blocks/"
                         f"{b['vanilla']['state_total']} states; ours "
                         f"v767_blocks={b['ours']['v767_block_count']} "
                         f"ceiling={b['ours']['state_ceiling']} "
                         f"match={b['match']}")
        lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def build_parser():
    ap = argparse.ArgumentParser(
        description="mc-spine vs Vanilla 1.21.1 parity gap detector "
                    "(megu-parity-audit/1). Mode A always runs offline; "
                    "Mode B diffs against vanilla --reports output.")
    ap.add_argument("--repo", default=".", help="repo root (default: .)")
    ap.add_argument("--reports-dir", default=None,
                    help="dir containing vanilla reports/{packets,commands,registries}.json")
    ap.add_argument("--vanilla-jar", default=None,
                    help="path to a local Minecraft 1.21.1 server.jar "
                         "(runs net.minecraft.data.Main --reports)")
    ap.add_argument("--format", choices=["text", "markdown", "json"], default="text")
    ap.add_argument("--out", default=None, help="also write the report to this path")
    # Strict regression gate: any violation exits 1 with a diagnostic on stderr.
    ap.add_argument("--strict", action="store_true",
                    help="regression gate: imply --require-zero-proto-drift and "
                         "enforce every floor flag that is set")
    ap.add_argument("--min-c2s-handled", type=int, default=None, metavar="N",
                    help="gate: fail if live-handled C2S Play packets < N")
    ap.add_argument("--min-gamerules-wired", type=int, default=None, metavar="N",
                    help="gate: fail if wired gamerules < N")
    ap.add_argument("--require-zero-proto-drift", action="store_true",
                    help="gate: fail on ANY Mode B proto/command/registry/"
                         "block-state drift (requires --reports-dir/--vanilla-jar)")
    return ap


def evaluate_gate(report, args):
    """Strict-gate violations -> list of exact diagnostic strings (empty = pass)."""
    violations = []
    off = report["offline"]
    if args.min_c2s_handled is not None:
        handled = off["packets"]["handled_count"]
        if handled < args.min_c2s_handled:
            violations.append(
                f"c2s_play handled {handled} < --min-c2s-handled "
                f"{args.min_c2s_handled}")
    if args.min_gamerules_wired is not None:
        wired = off["gamerules"]["wired_count"]
        if wired < args.min_gamerules_wired:
            violations.append(
                f"gamerules wired {wired} < --min-gamerules-wired "
                f"{args.min_gamerules_wired}")
    want_drift = args.require_zero_proto_drift or args.strict
    if want_drift and "vanilla" in report:
        v = report["vanilla"]
        vp = v["packets"]
        if vp["missing_in_proto_count"]:
            violations.append(
                f"proto drift: missing_in_proto={vp['missing_in_proto_count']}")
        if vp["id_mismatch_in_proto_count"]:
            violations.append(
                f"proto drift: id_mismatch={vp['id_mismatch_in_proto_count']}")
        if vp["name_mismatch_in_proto_count"]:
            violations.append(
                f"proto drift: name_mismatch={vp['name_mismatch_in_proto_count']}")
        vc = v["commands"]
        if vc["missing_vanilla_commands_count"]:
            violations.append(
                f"command drift: missing_vanilla_commands="
                f"{vc['missing_vanilla_commands_count']}")
        for reg, rr in sorted(v["registries"].items()):
            n_miss = len(rr["missing_ids"])
            n_mism = len(rr["id_mismatch"])
            if n_miss or n_mism or rr["vanilla_count"] != rr["ours_count"]:
                violations.append(
                    f"registry drift {reg}: missing_ids={n_miss} "
                    f"id_mismatch={n_mism} vanilla_count={rr['vanilla_count']} "
                    f"ours_count={rr['ours_count']}")
        if "blocks" in v and not v["blocks"]["match"]:
            b = v["blocks"]
            violations.append(
                f"block-state drift: vanilla={b['vanilla']['block_count']} blocks/"
                f"{b['vanilla']['state_total']} states vs ours "
                f"v767_blocks={b['ours']['v767_block_count']} "
                f"ceiling={b['ours']['state_ceiling']} "
                f"per_block_mismatches={b['per_block_state_mismatch_count']}")
    return violations


def main(argv=None):
    args = build_parser().parse_args(argv)
    repo = Path(args.repo).resolve()
    try:
        # Mode B inputs are validated before the (slow) Mode A scan so bad
        # --vanilla-jar / --reports-dir paths fail closed immediately.
        vanilla = None
        if args.reports_dir:
            data, source = load_reports_dir(args.reports_dir)
            vanilla = (data, f"reports-dir:{args.reports_dir}")
        elif args.vanilla_jar:
            data, source = run_vanilla_jar(args.vanilla_jar)
            vanilla = (data, source)
        report = mode_a(repo)
        if vanilla is not None:
            data, source = vanilla
            report["mode"] = "offline+vanilla"
            report.update(audit_vanilla(repo, report, data, source))
        if args.format == "json":
            rendered = json.dumps(report, indent=2, sort_keys=False) + "\n"
        elif args.format == "markdown":
            rendered = render_markdown(report) + "\n"
        else:
            rendered = render_text(report) + "\n"
    except AuditError as exc:
        print(f"vanilla_parity_audit: error: {exc}", file=sys.stderr)
        return 2
    except Exception as exc:  # noqa: BLE001 - never crash silently
        print(f"vanilla_parity_audit: internal error: {exc}", file=sys.stderr)
        return 2
    sys.stdout.write(rendered)
    if args.out:
        Path(args.out).write_text(rendered)
    gate_active = (args.strict or args.min_c2s_handled is not None
                   or args.min_gamerules_wired is not None
                   or args.require_zero_proto_drift)
    if gate_active:
        violations = evaluate_gate(report, args)
        if violations:
            for v in violations:
                print(f"vanilla_parity_audit: gate violation: {v}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
