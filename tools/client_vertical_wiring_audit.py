#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Bidirectional vertical-slice wiring audit for the live client path.

A gameplay feature is only "wired" when it is reachable in BOTH directions:
the S2C packet has to reach a named handler arm (not ``_ => {}``), the C2S
command has to be produced by a ``LiveSession`` method that real window code
calls, every ``HudState`` channel needs a real writer, and every screen has to
be reachable from the GPU driver. Anything else is a half-implemented feature
that compiles cleanly and silently does nothing — the class of bug that let
dig/place, bossbars, chat, and the death screen slip through.

Four checks, all evaluated on masked Rust source (comments, string/char
literals and ``#[cfg(test)]`` items are blanked out first — same technique as
``tools/wiring_audit.py`` — so test-only callers can never satisfy a check):

1. ``s2c_play_event_consumption`` — every ``PlayEvent`` variant
   (``client-app/src/protocol.rs``) must be referenced by non-test code in
   ``client-app/src/world.rs`` (``apply_play_event``) or
   ``client-gpu/src/live_session.rs`` (``apply_play`` and its helpers).
   Anything else falls into the ``_ => {}`` catch-alls.
2. ``worker_command_producers`` — every ``WorkerCommand`` variant
   (``client-gpu/src/live_session.rs``) must have a non-test construction
   site whose enclosing method is reachable from non-test code in another
   ``client-gpu/src`` file (``window.rs`` drives input).
3. ``hud_state_writers`` — every ``pub`` field of ``HudState``
   (``client-ui/src/state.rs``) must have a non-test write site in
   ``client-gpu/src``: ``hud.<field>(.sub)* =``, a mutating method call
   (``.push``/``.clear``/``.insert``/``.tick``/…), an ``&mut`` borrow of the
   field, or a ``HudState { field: … }`` literal init. Whole-struct
   assignments (``hud = HudState::default()``, ``*hud = self.hud.clone()``)
   do not count — a field that is only ever reset or mirrored is still dead.
4. ``screen_state_reachability`` — every ``Screen`` enum variant and every
   ``pub struct *ScreenState`` in ``client-ui/src`` must be referenced by
   non-test code in ``client-gpu/src`` (``use`` declarations stripped).

Gaps are ratcheted through ``specs/client-vertical-wiring-waivers.json``:
each entry carries a ``priority`` (``P0``/``P1``/``P2``) and a ``reason``.
The run fails on ANY gap lacking a waiver, prints ``STALE WAIVER`` notices
when a waived item becomes wired (or names an item that no longer exists),
and only fails on stale waivers under ``--fail-on-stale`` — so landing a fix
before trimming the JSON never breaks CI, while a new half-wired feature
always does.

Usage:
    tools/client_vertical_wiring_audit.py                  # human summary, strict
    tools/client_vertical_wiring_audit.py --json           # machine-readable report
    tools/client_vertical_wiring_audit.py --report-only    # never fail on gaps
    tools/client_vertical_wiring_audit.py --fail-on-stale  # also fail on stale waivers
    tools/client_vertical_wiring_audit.py --root DIR --waivers PATH
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_WAIVERS = REPO_ROOT / "specs/client-vertical-wiring-waivers.json"

CHECK_S2C = "s2c_play_event_consumption"
CHECK_C2S = "worker_command_producers"
CHECK_HUD = "hud_state_writers"
CHECK_SCREENS = "screen_state_reachability"
CHECKS = (CHECK_S2C, CHECK_C2S, CHECK_HUD, CHECK_SCREENS)

PRIORITIES = ("P0", "P1", "P2")

PROTOCOL_RS = "client-app/src/protocol.rs"
WORLD_RS = "client-app/src/world.rs"
LIVE_SESSION_RS = "client-gpu/src/live_session.rs"
UI_STATE_RS = "client-ui/src/state.rs"
CLIENT_GPU_SRC = "client-gpu/src"
CLIENT_UI_SRC = "client-ui/src"

# ---------------------------------------------------------------------------
# Rust source masking — mirrors tools/wiring_audit.py::production_code so
# comments, literals and cfg(test) items can never satisfy a check.
# ---------------------------------------------------------------------------

RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
CHARACTER = re.compile(r"'(?:[^'\\\n]|\\(?:.|u\{[A-Fa-f0-9_]+\}))'")
CFG_TEST = re.compile(r"\bcfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)")
IDENT = re.compile(r"\b[A-Za-z_][A-Za-z_0-9]*\b")


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
    code[start:end] = " " * (end - start)


def _skip_space(code, pos):
    while pos < len(code) and code[pos].isspace():
        pos += 1
    return pos


def production_code(text):
    """Mask Rust comments/literals and cfg(test) items, preserving offsets."""
    code = list(text)
    pos = 0
    while pos < len(text):
        start = pos
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
        elif match := CHARACTER.match(text, pos):
            pos = match.end()
        else:
            pos += 1
            continue
        _mask(code, start, min(pos, len(text)))

    clean = "".join(code)
    # cfg(test) functions, inline modules, and uses are compile-time-only.
    # Do not mask cfg(not(test)) or cfg(any(test, feature = ...)).
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


def strip_use_declarations(code):
    """`use`/`pub use`/`mod` declarations name items without wiring them."""
    code = re.sub(r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+\w+\s*;", " ", code)
    code = re.sub(r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?use\s+[^;]*;", " ", code)
    return code


# ---------------------------------------------------------------------------
# Rust surface extraction (operates on masked code)
# ---------------------------------------------------------------------------


def item_body(code, keyword, name):
    """Return the `{…}` body of `enum|struct|impl NAME`, or None."""
    match = re.search(
        rf"\b{keyword}\s+{re.escape(name)}\b[^{{;]*\{{", code)
    if match is None:
        return None
    start = code.index("{", match.start())
    return code[start + 1:_balanced_end(code, start, "{", "}") - 1]


def _top_level_segments(body):
    """Split a brace body on top-level commas (depth-0), yielding segments."""
    segments = []
    depth = 0
    start = 0
    for idx, char in enumerate(body):
        if char in "{([":
            depth += 1
        elif char in "})]":
            depth -= 1
        elif char == "," and depth == 0:
            segments.append(body[start:idx])
            start = idx + 1
    tail = body[start:]
    if tail.strip():
        segments.append(tail)
    return segments


def enum_variants(body):
    """Variant names of an enum body: first identifier of each top-level
    comma-separated segment (handles unit, tuple and struct variants)."""
    variants = []
    for segment in _top_level_segments(body):
        # Strip leading attributes like #[default] that survived masking.
        segment = re.sub(r"#\s*\[[^\]]*\]", " ", segment)
        match = IDENT.search(segment)
        if match:
            variants.append(match.group(0))
    return variants


def pub_field_names(body):
    """`pub` field names of a braced struct body (top level only)."""
    fields = []
    for segment in _top_level_segments(body):
        segment = re.sub(r"#\s*\[[^\]]*\]", " ", segment)
        match = re.search(r"\bpub\s+([a-z_][A-Za-z0-9_]*)\s*:", segment)
        if match:
            fields.append(match.group(1))
    return fields


FN_HEAD = re.compile(
    r"\b(pub(?:\s*\([^)]*\))?)?\s*"
    r"(?:async\s+|unsafe\s+|const\s+|extern\s+\"[A-Za-z]+\"\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def fn_ranges(code):
    """`(name, is_pub, start, end)` for every `fn` with a `{…}` body."""
    fns = []
    for match in FN_HEAD.finditer(code):
        name = match.group(2)
        # Scan the signature for the body `{` at depth 0; a `;` at depth 0
        # first means a bodiless declaration (trait fn, extern). `;` inside
        # `[T; N]` array types stays below the radar.
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
        fns.append((name, bool(match.group(1)), match.start(), end))
    return fns


# ---------------------------------------------------------------------------
# Waiver file
# ---------------------------------------------------------------------------


def load_waivers(path):
    if not path.is_file():
        return {}, [f"waiver file {path} does not exist"]
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        return {}, [f"waiver file {path} is not valid JSON: {error}"]
    entries = data.get("waivers")
    if not isinstance(entries, list):
        return {}, [f"waiver file {path} needs a top-level \"waivers\" list"]
    waivers, errors = {}, []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            errors.append(f"waiver #{index} is not an object")
            continue
        check, item = entry.get("check"), entry.get("id")
        priority, reason = entry.get("priority"), entry.get("reason")
        if check not in CHECKS:
            errors.append(f"waiver #{index} has unknown check {check!r}")
            continue
        if not isinstance(item, str) or not item:
            errors.append(f"waiver #{index} ({check}) needs a non-empty \"id\"")
            continue
        if priority not in PRIORITIES:
            errors.append(
                f"waiver {check}:{item} needs priority one of {PRIORITIES}")
            continue
        if not isinstance(reason, str) or not reason.strip():
            errors.append(f"waiver {check}:{item} needs a non-empty \"reason\"")
            continue
        waivers[(check, item)] = {"priority": priority, "reason": reason.strip()}
    return waivers, errors


# ---------------------------------------------------------------------------
# The four checks — each returns `{item_id: {"wired": bool, "detail": str}}`
# ---------------------------------------------------------------------------


def _rust_sources(root, crate_src):
    src_dir = root / crate_src
    for path in sorted(src_dir.rglob("*.rs")):
        parts = path.relative_to(src_dir).parts
        if "tests" in parts[:-1] or path.stem in ("tests",) \
                or path.stem.endswith(("_test", "_tests")):
            continue
        yield path


def check_s2c_play_events(sources):
    protocol = sources.get(PROTOCOL_RS)
    if protocol is None:
        raise FileNotFoundError(PROTOCOL_RS)
    body = item_body(protocol, "enum", "PlayEvent")
    if body is None:
        raise ValueError(f"enum PlayEvent not found in {PROTOCOL_RS}")
    consumers = "\n".join(
        sources.get(path, "") for path in (WORLD_RS, LIVE_SESSION_RS))
    results = {}
    for variant in enum_variants(body):
        needle = re.compile(rf"(?<!\w)PlayEvent::{variant}\b")
        wired = bool(needle.search(consumers))
        results[f"PlayEvent::{variant}"] = {
            "wired": wired,
            "detail": (
                "handled by a named arm in apply_play_event/apply_play"
                if wired else
                "no named handler — falls into the `_ => {}` catch-all"),
        }
    return results


# A `WorkerCommand::Variant` occurrence is a *pattern* (match arm / let
# destructure / matches!) when preceded by one of these tokens…
_PATTERN_PREFIX = re.compile(
    r"(?:Ok|Err|Some|None|matches|assert(?:_\w+)?|debug_assert(?:_\w+)?|"
    r"if\s+let|while\s+let|for|let)\s*!?\s*[(,\s]*$")
# …or when the tokens right after the variant's own (…) or {…} body close
# wrappers and land on `=>`.
_PATTERN_SUFFIX = re.compile(r"^\s*[),\s]*=>")


def _is_pattern_occurrence(code, start, name_end):
    prefix = code[max(0, start - 64):start]
    if _PATTERN_PREFIX.search(prefix):
        return True
    pos = _skip_space(code, name_end)
    if pos < len(code) and code[pos] in "({":
        pos = _balanced_end(code, pos, code[pos], ")" if code[pos] == "(" else "}")
    return bool(_PATTERN_SUFFIX.match(code[pos:pos + 64]))


def check_worker_commands(sources, gpu_sources):
    live = sources.get(LIVE_SESSION_RS)
    if live is None:
        raise FileNotFoundError(LIVE_SESSION_RS)
    body = item_body(live, "enum", "WorkerCommand")
    if body is None:
        raise ValueError(f"enum WorkerCommand not found in {LIVE_SESSION_RS}")
    fns = fn_ranges(live)

    def enclosing_fn(pos):
        for name, is_pub, start, end in fns:
            if start <= pos < end:
                return name
        return None

    def externally_called(name):
        needle = re.compile(rf"(?<![\w.]){name}\s*\(|\.{name}\s*\(")
        for path, code in gpu_sources.items():
            if path == LIVE_SESSION_RS:
                continue
            if needle.search(code):
                return True
        return False

    # A producer counts as wired when its enclosing method is called from
    # another client-gpu file, or from a method that itself is (one level of
    # internal helpers covers the real call shapes).
    internal_edges = {}  # callee -> {callers}
    for name, is_pub, start, end in fns:
        body_text = live[start:end]
        for callee in re.finditer(
                r"(?<![\w:])(?:self\.)?([a-z_][A-Za-z0-9_]*)\s*\(", body_text):
            internal_edges.setdefault(callee.group(1), set()).add(name)
    reachable = {name for name, _, _, _ in fns if externally_called(name)}
    for callee, callers in internal_edges.items():
        if callers & reachable:
            reachable.add(callee)

    results = {}
    for variant in enum_variants(body):
        needle = re.compile(rf"(?<!\w)WorkerCommand::{variant}\b")
        producers = []
        for match in needle.finditer(live):
            if _is_pattern_occurrence(live, match.start(), match.end()):
                continue
            fn_name = enclosing_fn(match.start())
            if fn_name:
                producers.append(fn_name)
        wired = any(name in reachable for name in producers)
        detail = (
            f"produced via {', '.join(sorted(set(producers)))} "
            "(called from outside live_session.rs)" if wired else
            (f"constructed in {', '.join(sorted(set(producers)))} but never "
             "called from non-test code outside live_session.rs"
             if producers else
             "no non-test construction site at all"))
        results[f"WorkerCommand::{variant}"] = {"wired": wired, "detail": detail}
    return results


# Method names treated as writes when called on `hud.<field>(.sub)*`.
_MUTATING_METHODS = (
    "push", "push_str", "extend", "append", "insert", "remove", "drain",
    "clear", "pop", "retain", "truncate", "resize", "fill", "dedup", "sort",
    "sort_by", "sort_by_key", "sort_unstable", "swap", "swap_remove",
    "tick", "show", "reset", "take", "replace", "update", "apply", "record",
    "note", "absorb", "begin", "finish", "advance", "merge", "mark", "bump",
    "increment", "decrement", "start", "stop", "refresh", "sync", "bake",
    "write", "enter", "exit", "open", "close", "activate", "deactivate",
)
_MUTATING_RE = "|".join(_MUTATING_METHODS)


def _hud_field_write_re(field):
    """Write patterns for one HudState field (masked client-gpu code):
    assignment (`=`, `+=`, …), mutating method calls, `&mut` borrows."""
    path = rf"\bhud\.{field}(?:\.[A-Za-z_]\w*)*"
    return re.compile(
        rf"{path}\s*(?:[+\-*/%&|^]|<<|>>)?=(?![=>])"
        rf"|{path}\s*\.\s*(?:{_MUTATING_RE})\s*\("
        rf"|&mut\s+[A-Za-z0-9_.()\[\]&*\s]*?{path}\b"
    )


def _hud_literal_fields(code):
    """Field names initialised by `HudState { … }` literals."""
    fields = set()
    for match in re.finditer(r"\bHudState\s*\{", code):
        start = code.index("{", match.start())
        body = code[start + 1:_balanced_end(code, start, "{", "}") - 1]
        for segment in _top_level_segments(body):
            field = re.match(r"\s*([a-z_]\w*)\s*:", segment)
            if field:
                fields.add(field.group(1))
            elif re.match(r"\s*\.\.", segment):
                continue
    return fields


def check_hud_state(sources, gpu_sources):
    state = sources.get(UI_STATE_RS)
    if state is None:
        raise FileNotFoundError(UI_STATE_RS)
    body = item_body(state, "struct", "HudState")
    if body is None:
        raise ValueError(f"struct HudState not found in {UI_STATE_RS}")
    literal_fields = set()
    for code in gpu_sources.values():
        literal_fields |= _hud_literal_fields(code)
    results = {}
    for field in pub_field_names(body):
        write_re = _hud_field_write_re(field)
        wired = field in literal_fields or any(
            write_re.search(code) for code in gpu_sources.values())
        results[f"HudState::{field}"] = {
            "wired": wired,
            "detail": (
                "written from non-test client-gpu code" if wired else
                "read-only/defaulted — no non-test write site in client-gpu"),
        }
    return results


def check_screens(sources, gpu_sources, ui_sources):
    state = sources.get(UI_STATE_RS)
    if state is None:
        raise FileNotFoundError(UI_STATE_RS)
    body = item_body(state, "enum", "Screen")
    if body is None:
        raise ValueError(f"enum Screen not found in {UI_STATE_RS}")
    gpu_text = "\n".join(gpu_sources.values())
    results = {}
    for variant in enum_variants(body):
        needle = re.compile(rf"(?<!\w)Screen::{variant}\b")
        wired = bool(needle.search(gpu_text))
        results[f"Screen::{variant}"] = {
            "wired": wired,
            "detail": (
                "named by non-test client-gpu code" if wired else
                "never named in client-gpu — unreachable or only reached via "
                "generic OpenScreen dispatch"),
        }
    for path, code in ui_sources.items():
        for match in re.finditer(r"\bpub\s+struct\s+([A-Za-z_]\w*ScreenState)\b", code):
            name = match.group(1)
            key = name
            if key in results:
                continue
            wired = bool(re.search(rf"(?<!\w){name}\b", gpu_text))
            results[key] = {
                "wired": wired,
                "detail": (
                    f"{name} referenced from non-test client-gpu code" if wired
                    else f"{name} (in {path}) never referenced in client-gpu"),
            }
    return results


# ---------------------------------------------------------------------------
# Audit driver
# ---------------------------------------------------------------------------


def _masked_sources(root, paths):
    sources = {}
    for rel in paths:
        path = root / rel
        if path.is_file():
            sources[rel] = production_code(path.read_text(encoding="utf-8"))
    return sources


def run_audit(root):
    core = _masked_sources(root, (PROTOCOL_RS, WORLD_RS, LIVE_SESSION_RS, UI_STATE_RS))
    gpu_sources = {}
    for path in _rust_sources(root, CLIENT_GPU_SRC):
        rel = path.relative_to(root).as_posix()
        gpu_sources[rel] = strip_use_declarations(
            production_code(path.read_text(encoding="utf-8")))
    live = strip_use_declarations(
        production_code((root / LIVE_SESSION_RS).read_text(encoding="utf-8")))
    gpu_sources[LIVE_SESSION_RS] = live
    ui_sources = {}
    for path in _rust_sources(root, CLIENT_UI_SRC):
        rel = path.relative_to(root).as_posix()
        ui_sources[rel] = strip_use_declarations(
            production_code(path.read_text(encoding="utf-8")))
    consumers = dict(core)
    consumers[WORLD_RS] = strip_use_declarations(consumers[WORLD_RS])
    consumers[LIVE_SESSION_RS] = live
    results = {
        CHECK_S2C: check_s2c_play_events(consumers),
        CHECK_C2S: check_worker_commands(consumers, gpu_sources),
        CHECK_HUD: check_hud_state(consumers, gpu_sources),
        CHECK_SCREENS: check_screens(consumers, gpu_sources, ui_sources),
    }
    return results


def evaluate(results, waivers):
    """Split every check item into wired / waived / unwaived / stale-waiver."""
    report = {"checks": {}, "waivers": {"stale": []}}
    failed = False
    waived_keys = set()
    for check in CHECKS:
        items = results[check]
        entry = {"wired": [], "waived": [], "unwaived": []}
        for item_id in sorted(items):
            waiver = waivers.get((check, item_id))
            if items[item_id]["wired"]:
                entry["wired"].append(item_id)
                if waiver is not None:
                    report["waivers"]["stale"].append(
                        {"check": check, "id": item_id,
                         "priority": waiver["priority"],
                         "reason": waiver["reason"],
                         "why": "item is now wired — drop the waiver"})
                    waived_keys.add((check, item_id))
            elif waiver is not None:
                entry["waived"].append(
                    {"id": item_id, "priority": waiver["priority"],
                     "reason": waiver["reason"],
                     "detail": items[item_id]["detail"]})
                waived_keys.add((check, item_id))
            else:
                entry["unwaived"].append(
                    {"id": item_id, "detail": items[item_id]["detail"]})
                failed = True
        entry["counts"] = {
            "wired": len(entry["wired"]),
            "waived": len(entry["waived"]),
            "unwaived": len(entry["unwaived"]),
            "waived_by_priority": {
                p: sum(1 for w in entry["waived"] if w["priority"] == p)
                for p in PRIORITIES
            },
        }
        report["checks"][check] = entry
    for (check, item_id), waiver in sorted(waivers.items()):
        if (check, item_id) in waived_keys:
            continue
        report["waivers"]["stale"].append(
            {"check": check, "id": item_id, "priority": waiver["priority"],
             "reason": waiver["reason"],
             "why": "waiver names an item the audit no longer sees — drop it"})
    report["failed"] = failed
    return report


def _print_human(report):
    total_wired = total_waived = total_unwaived = 0
    for check in CHECKS:
        entry = report["checks"][check]
        counts = entry["counts"]
        prio = counts["waived_by_priority"]
        total_wired += counts["wired"]
        total_waived += counts["waived"]
        total_unwaived += counts["unwaived"]
        print(f"== {check}")
        print(f"   wired {counts['wired']}  "
              f"waived {counts['waived']} "
              f"(P0 {prio['P0']} / P1 {prio['P1']} / P2 {prio['P2']})  "
              f"unwaived {counts['unwaived']}")
        for item in entry["unwaived"]:
            print(f"   UNWIRED {item['id']}  — {item['detail']}")
        for item in entry["waived"]:
            print(f"   waived  {item['id']}  [{item['priority']}] {item['reason']}")
    stale = report["waivers"]["stale"]
    for item in stale:
        print(f"STALE WAIVER (can now be removed!): {item['check']} "
              f"{item['id']} — {item['why']}")
    print(f"total: {total_wired} wired, {total_waived} waived, "
          f"{total_unwaived} unwaived, {len(stale)} stale waivers")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument("--waivers", type=Path, default=DEFAULT_WAIVERS)
    parser.add_argument("--json", action="store_true",
                        help="emit the machine-readable report")
    parser.add_argument("--report-only", action="store_true",
                        help="never fail on unwaived gaps")
    parser.add_argument("--fail-on-stale", action="store_true",
                        help="also fail when a waiver is stale (fixed or bogus)")
    args = parser.parse_args(argv)

    try:
        results = run_audit(args.root)
    except (FileNotFoundError, ValueError) as error:
        print(f"client_vertical_wiring_audit: {error}", file=sys.stderr)
        return 2
    waivers, errors = load_waivers(args.waivers)
    for error in errors:
        print(f"client_vertical_wiring_audit: {error}", file=sys.stderr)
    report = evaluate(results, waivers)
    report["waiver_errors"] = errors

    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        _print_human(report)

    if errors:
        return 2
    if report["waivers"]["stale"] and args.fail_on_stale:
        return 1
    if report["failed"] and not args.report_only:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
