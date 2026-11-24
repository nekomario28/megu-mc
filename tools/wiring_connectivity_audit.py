#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Repo-wide disconnected-subsystem & dead-bridge connectivity detector.

The defect class this gate exists for is "implemented but never wired into
live runtime": a 1,500-line bridge whose ``aeronautics_bridge_tick`` had zero
non-test callers, a ``live_anvil_save_tick`` invoked only from ``#[cfg(test)]``
code, or a ``megu:*`` / ``create:*`` custom channel that exists on the server
but is never handled on the client (or vice versa). Each one still compiles
and its tests stay green, so nothing fails until a player notices the feature
silently does nothing.

All checks run on masked Rust source (same technique as
``tools/wiring_audit.py`` / ``tools/opt_wiring_audit.py``): comments,
string/char literals and ``#[cfg(test)]`` items are blanked out with offsets
preserved, so a call site that only exists in test code — or in a comment —
can never satisfy a check. A second pass keeps string-literal contents (with
brackets neutralised) for the channel and save-tag checks, which are about
the *value* inside the literal.

Checks:

* ``dead_tick_or_bridge`` — ``*_tick`` / ``*_tick_*`` / ``tick_*`` /
  ``live_*`` / ``dimension_mod_*`` / ``*_bridge*`` free functions and
  methods defined in ``src/``, ``client-app/src/``, ``client-gpu/src/``,
  ``mod-adapter-core/src/`` or ``create-adapter/src/`` must have at least
  one non-test reference (call, method call, function-pointer use, macro
  argument or ``use`` import) somewhere in the workspace outside their own
  definition. ``#[allow(dead_code)]`` is *not* an exemption — it is exactly
  how dead bridges hide — but it is reported on the finding.
* ``one_sided_channel`` — every ``megu:*`` / ``create:*`` identifier used in
  a plugin-channel context (a ``*CHANNEL*`` constant or within a few lines
  of ``channel``/``custom_payload``/``plugin``/``register`` vocabulary) must
  appear on both the server side (``src/`` + server crates) and the client
  side (``client-*/src``). ``proto/src`` is shared infrastructure and
  satisfies neither side by itself. ``minecraft:brand`` /
  ``minecraft:register`` / ``minecraft:unregister`` are vanilla-standard and
  excluded. Third-party mod namespaces (``fabric:``, ``neoforge:`` …) are
  intentionally outside the ``megu:``/``create:`` discovery set.
* ``asymmetric_save_tag`` — every 4-byte ``b"TAG!"`` section magic in
  ``src/save_v2.rs`` (literal or ``*MAGIC*`` constant) must have both a
  write/encode site (``extend_from_slice``, ``append_*``, ``emit_*``) and a
  read/decode site (``==``/``!=`` compare, ``starts_with``, ``ends_with``,
  ``split_save_footer``, ``read_*``) in production code.
* ``unwired_subsystem_entry`` — entrypoint-shaped ``fn`` / ``pub fn`` items
  (``*_init``, ``*_register_*``, ``*_sync_*``, ``*_export*``, ``*_import*``,
  ``*_discover*``) in bridge/adapter modules must have a non-test caller.
* ``dead_server_config_field`` — ``server.toml`` / ``server.properties``
  keys exported to ``MEGU_*`` variables, ``live_*`` runtime accessors,
  ``RuntimeOpsConfig`` fields and ``ServerProperties`` /
  ``ServerFairPlayPolicy`` accessors must reach a non-test reader; a knob
  that is only parsed, stored or serialized is a finding.
* ``dead_client_config_field`` — ``ClientSettings`` / ``UiSettings``
  fields must be read outside ``settings.rs``/``state.rs``, the
  ``apply_client_settings_to_ui``/``ui_to_client_settings`` copy bodies
  and the ``settings``/``*_tab`` screens — or be mapped by that bridge to
  a counterpart that is. Pure UI↔TOML round-tripping is a finding.

``audit/wiring_connectivity_waivers.json`` baselines the currently-known
unwired items with a reason and owning lane: the audit fails on any *new*
unwired subsystem while tolerated debt stays visible. A waiver whose item
got wired (or removed) is *stale* — reported as a warning by default so
parallel fixing lanes never break each other, and a hard failure under
``--fail-on-stale`` so the manifest can be pruned cleanly.

Usage:
    tools/wiring_connectivity_audit.py            # human PASS/FAIL report
    tools/wiring_connectivity_audit.py --json     # machine-readable report
    tools/wiring_connectivity_audit.py --fail-on-stale
    tools/wiring_connectivity_audit.py --root DIR # hermetic fixture audit
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
WAIVERS_REL = "audit/wiring_connectivity_waivers.json"

# ---------------------------------------------------------------------------
# Scan layout. `side` classifies a file for the channel symmetry check;
# candidate definitions are only collected where the check's table says so.
# ---------------------------------------------------------------------------

SERVER_CRATE_DIRS = (
    "src", "core/src", "state-store/src", "server-files/src",
    "mod-offload/src", "mod-adapter-core/src", "create-adapter/src",
    "compat",
)
CLIENT_CRATE_DIRS = (
    "client-kernel/src", "client-port/src", "client-ui/src",
    "client-app/src", "client-gpu/src",
)
SHARED_CRATE_DIRS = ("proto/src",)

# Definition sites considered for each check. Call-site evidence is always
# counted across the whole corpus — an include!d sibling or a dependent
# crate is exactly where the missing wire would live.
DEAD_TICK_DIRS = (
    "src", "client-app/src", "client-gpu/src",
    "mod-adapter-core/src", "create-adapter/src",
)
CHANNEL_DIRS = SERVER_CRATE_DIRS + CLIENT_CRATE_DIRS + SHARED_CRATE_DIRS
SAVE_TAG_FILE = "src/save_v2.rs"

# A file is a bridge/adapter module when its name says so, plus the named
# integration entrypoint files called out in the audit contract.
ENTRYPOINT_FILE = re.compile(
    r"(?:bridge|adapter|offload|interop|_io\.rs$|"
    r"subworld_render\.rs$|vanilla_jar\.rs$|live_session\.rs$)"
)

# Check 1 name families. `tick` alone is excluded on purpose: `fn tick` is a
# trait/impl method shape exercised through `.tick(` receivers, and the bare
# word is too generic to mean "live tick entrypoint".
DEAD_TICK_NAME = re.compile(
    r"^(?:"
    r"tick_\w+"
    r"|live_\w+"
    r"|dimension_mod_\w+"
    r"|\w+_bridge(?:_\w+)?"
    r"|\w+_tick(?:_\w+)?"
    r")$"
)

# Check 4 entrypoint name families.
ENTRYPOINT_NAME = re.compile(
    r"^(?:"
    r"\w+_init"
    r"|\w+_register_\w+"
    r"|\w+_sync_\w+"
    r"|\w+_export\w*"
    r"|\w+_import\w*"
    r"|\w+_discover\w*"
    r")$"
)

# ---------------------------------------------------------------------------
# Rust source masking — mirrors tools/opt_wiring_audit.py: comments,
# literals and cfg(test) items are blanked while offsets are preserved.
# ---------------------------------------------------------------------------

RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
CHARACTER = re.compile(r"'(?:[^'\\\n]|\\(?:.|u\{[A-Fa-f0-9_]+\}))'")
CFG_TEST = re.compile(r"\bcfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)")
STRING_BRACKETS = re.compile(r"[(){}\[\]]")


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


def production_code(text, keep_strings=False):
    """Mask Rust comments/literals and cfg(test) items, preserving offsets.

    With ``keep_strings`` the contents of string literals stay readable but
    bracket characters inside them are still blanked, so format strings can
    never corrupt the brace accounting used to find ``#[cfg(test)]`` item
    ends. Channel and save-tag checks need the literal values; everything
    else uses the fully masked form.
    """
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
            # Preserve the literal's text for channel/tag discovery while
            # neutralising brackets so `"{".repeat(n)` and friends cannot
            # unbalance the cfg(test) item walk below.
            s, e = string_span
            for idx in range(s, min(e, len(code))):
                if code[idx] in "(){}[]":
                    code[idx] = " "
        else:
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


# ---------------------------------------------------------------------------
# Rust item helpers (operate on masked code)
# ---------------------------------------------------------------------------

FN_HEAD = re.compile(
    r"\b(pub(?:\s*\([^)]*\))?)?\s*"
    r"(?:async\s+|unsafe\s+|const\s+|extern\s+\"[A-Za-z]+\"\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def fn_ranges(code):
    """`(name, start, body_start, end)` for every `fn` with a `{…}` body."""
    fns = []
    for match in FN_HEAD.finditer(code):
        name = match.group(2)
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
        fns.append((name, match.start(), brace, end))
    return fns


def _skip_space_back(code, pos):
    while pos > 0 and code[pos - 1].isspace():
        pos -= 1
    return pos


def preceding_attribute_spans(code, pos):
    """`(inner_start, inner_end)` spans of `#[...]` attrs before `pos`."""
    spans = []
    cursor = _skip_space_back(code, pos)
    while cursor > 0 and code[cursor - 1] == "]":
        depth = 0
        idx = cursor - 1
        while idx >= 0:
            if code[idx] == "]":
                depth += 1
            elif code[idx] == "[":
                depth -= 1
                if depth == 0:
                    break
            idx -= 1
        if idx < 0:
            break
        hash_pos = _skip_space_back(code, idx)
        if hash_pos <= 0 or code[hash_pos - 1] != "#":
            break
        spans.append((idx + 1, cursor - 1))
        cursor = _skip_space_back(code, hash_pos - 1)
    return spans


def fn_attributes(raw, code, fn_start):
    """Attribute text (from `raw`, which keeps cfg/feature strings) before a fn."""
    return " ".join(
        raw[s:e] for s, e in preceding_attribute_spans(code, fn_start)
    )


QUALIFIER_WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]*$")
FN_QUALIFIERS = {"pub", "unsafe", "const", "async", "extern", "default"}
# A `name! { … }` invocation (and a `macro_rules! name { … }` body) can emit
# the fns inside it — including their callers — so a fn defined in a macro
# block cannot be judged from literal source text.
MACRO_INVOKE = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]*!\s*\{")
MACRO_RULES = re.compile(r"\bmacro_rules!\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")


def macro_spans(code):
    """`(start, end)` of `name! {…}` and `macro_rules! name {…}` blocks."""
    spans = []
    for pat in (MACRO_INVOKE, MACRO_RULES):
        for match in pat.finditer(code):
            brace = code.index("{", match.start())
            spans.append((brace, _balanced_end(code, brace, "{", "}")))
    return spans


def item_start(code, fn_kw_pos):
    """Walk back over `pub`/`unsafe`/`extern "C"` qualifiers before `fn`.

    Masking blanks the `"C"` in `extern "C"`, so the fn-head match starts at
    `fn` and the qualifiers — and the attributes before them — must be
    reached by walking left over visible words and whitespace.
    """
    pos = _skip_space_back(code, fn_kw_pos)
    while pos > 0:
        if code[pos - 1] == ")":
            # `pub(crate)`/`pub(super)` tail: hop the balanced paren group.
            depth = 0
            idx = pos - 1
            while idx >= 0:
                if code[idx] == ")":
                    depth += 1
                elif code[idx] == "(":
                    depth -= 1
                    if depth == 0:
                        break
                idx -= 1
            if idx < 0:
                break
            word = QUALIFIER_WORD.search(code[:idx])
            if word and word.group(0) == "pub":
                pos = _skip_space_back(code, word.start())
                continue
            break
        word = QUALIFIER_WORD.search(code[:pos])
        if word and word.group(0) in FN_QUALIFIERS:
            pos = _skip_space_back(code, word.start())
            continue
        break
    return pos


def _is_fn_head(code, pos):
    """True when the `name` at `pos` is preceded only by the `fn` keyword."""
    prefix = code[max(0, pos - 200):pos]
    return bool(re.search(r"\bfn\s*$", prefix))


def line_of(code, pos):
    return code.count("\n", 0, pos) + 1


# ---------------------------------------------------------------------------
# Corpus
# ---------------------------------------------------------------------------

def _is_test_path(rel):
    name = Path(rel).name
    if name.endswith("_tests.rs"):
        return True
    parts = Path(rel).parts
    return "tests" in parts or "benches" in parts or "target" in parts


def iter_rust_files(root, rel_dirs):
    for rel_dir in rel_dirs:
        base = root / rel_dir
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            rel = path.relative_to(root).as_posix()
            if _is_test_path(rel):
                continue
            yield rel, path


def file_side(rel):
    top = Path(rel).parts[0]
    if top.startswith("client-"):
        return "client"
    if top == "proto":
        return "shared"
    return "server"


def build_corpus(root):
    """`{rel: {"raw", "code", "str_code", "side"}}` over every scanned dir."""
    corpus = {}
    dirs = sorted(set(DEAD_TICK_DIRS + CHANNEL_DIRS + (SAVE_TAG_FILE.rsplit("/", 1)[0],)))
    for rel, path in iter_rust_files(root, dirs):
        try:
            raw = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        corpus[rel] = {
            "raw": raw,
            "code": production_code(raw),
            "str_code": production_code(raw, keep_strings=True),
            "side": file_side(rel),
        }
    return corpus


def reference_count(corpus, name, own_rel, own_start, own_end):
    """Non-test `name` references outside `own_*` fn span, workspace-wide.

    Counts `name(` calls, `.name(` method calls, `&name`/`name` function
    pointer uses, macro arguments and `use` imports — any bare word that is
    not a `fn name` definition head, so a trait declaration or a sibling
    dead definition can never launder an unwired entrypoint.
    """
    needle = re.compile(rf"(?<![A-Za-z0-9_]){re.escape(name)}(?![A-Za-z0-9_])")
    hits = 0
    for rel, entry in corpus.items():
        code = entry["code"]
        if name not in code:
            continue
        for match in needle.finditer(code):
            pos = match.start()
            if rel == own_rel and own_start <= pos < own_end:
                continue
            if _is_fn_head(code, pos):
                continue
            hits += 1
    return hits


# ---------------------------------------------------------------------------
# Check 1 + 4: unwired entrypoint-shaped functions
# ---------------------------------------------------------------------------

def fn_wiring_candidates(corpus):
    """All candidate function definitions for checks 1 and 4."""
    candidates = []
    for rel, entry in corpus.items():
        in_tick_dir = rel.startswith(tuple(d + "/" for d in DEAD_TICK_DIRS))
        entry_file = bool(ENTRYPOINT_FILE.search(rel))
        if not in_tick_dir and not entry_file:
            continue
        code = entry["code"]
        mspans = macro_spans(code)
        for name, start, body_start, end in fn_ranges(code):
            if any(s <= start < e for s, e in mspans):
                continue
            check = None
            if in_tick_dir and DEAD_TICK_NAME.match(name):
                check = "dead_tick_or_bridge"
            elif entry_file and ENTRYPOINT_NAME.match(name):
                check = "unwired_subsystem_entry"
            if check is None:
                continue
            istart = item_start(code, start)
            attrs = fn_attributes(entry["raw"], code, istart)
            head = entry["raw"][istart:body_start]
            candidates.append({
                "check": check,
                "symbol": name,
                "file": rel,
                "line": line_of(entry["raw"], start),
                "start": start,
                "end": end,
                "allow_dead_code": "allow(dead_code)" in attrs.replace(" ", ""),
                "ffi_boundary": "no_mangle" in attrs or "extern" in head,
            })
    return candidates


def check_dead_functions(corpus):
    """Findings for dead_tick_or_bridge + unwired_subsystem_entry."""
    findings = []
    totals = {"dead_tick_or_bridge": 0, "unwired_subsystem_entry": 0}
    wired = {"dead_tick_or_bridge": 0, "unwired_subsystem_entry": 0}
    for cand in fn_wiring_candidates(corpus):
        if cand["ffi_boundary"]:
            continue
        check = cand["check"]
        totals[check] += 1
        refs = reference_count(
            corpus, cand["symbol"], cand["file"], cand["start"], cand["end"])
        if refs:
            wired[check] += 1
            continue
        findings.append({
            "check": check,
            "symbol": cand["symbol"],
            "file": cand["file"],
            "line": cand["line"],
            "detail": (
                "defined but has no non-test reference outside its own "
                "definition (calls under #[cfg(test)] do not count)"
                + ("; carries #[allow(dead_code)]"
                   if cand["allow_dead_code"] else "")),
            "allow_dead_code": cand["allow_dead_code"],
        })
    return findings, totals, wired


# ---------------------------------------------------------------------------
# Check 2: server <-> client custom channel symmetry
# ---------------------------------------------------------------------------

CHANNEL_ID = re.compile(r'"((?:megu|create):[a-z0-9_/-]+)"')
CHANNEL_CONTEXT = re.compile(
    r"(?i)(?:channel|custom.?payload|plugin|register)"
)
CONTEXT_WINDOW = 300
# Vanilla-standard channels are never findings.
CHANNEL_ALLOWLIST = {
    "minecraft:brand", "minecraft:register", "minecraft:unregister",
}


def check_channels(corpus):
    findings = []
    channels = {}
    for rel, entry in corpus.items():
        code = entry["str_code"]
        for match in CHANNEL_ID.finditer(code):
            channel = match.group(1)
            if channel in CHANNEL_ALLOWLIST:
                continue
            window = code[max(0, match.start() - CONTEXT_WINDOW):
                          match.end() + CONTEXT_WINDOW]
            if not CHANNEL_CONTEXT.search(window):
                continue
            info = channels.setdefault(
                channel, {"server": [], "client": [], "shared": []})
            info[entry["side"]].append(
                f"{rel}:{line_of(entry['raw'], match.start())}")
    for channel, sides in sorted(channels.items()):
        server, client, shared = sides["server"], sides["client"], sides["shared"]
        if server and client:
            continue
        if server:
            detail = ("custom channel exists on the server side only — "
                      "nothing on the client side ever references it")
        elif client:
            detail = ("custom channel exists on the client side only — "
                      "nothing on the server side ever references it")
        else:
            detail = ("custom channel exists only in shared declarations — "
                      "neither server nor client production code references it")
        present_sites = server + client + shared
        findings.append({
            "check": "one_sided_channel",
            "symbol": channel,
            "file": None,
            "sites": present_sites,
            "detail": detail,
        })
    total = len(channels)
    wired = total - len(findings)
    return findings, total, wired


# ---------------------------------------------------------------------------
# Check 3: save_v2 tag write/read symmetry
# ---------------------------------------------------------------------------

TAG_LITERAL = re.compile(r'\bb"([A-Z0-9]{4})"')
TAG_CONST = re.compile(
    r"\bconst\s+([A-Z_][A-Z0-9_]*(?:MAGIC|TAG|TRAILER)[A-Z0-9_]*)\s*:\s*"
    r"(?:&?\[u8;\s*4\]|&\[u8\])\s*=\s*&?b\"([A-Z0-9]{4})\""
)
WRITE_CONTEXT = re.compile(
    r"(?:extend_from_slice|append_\w+|\.extend\s*\(|emit_\w+|_emit\b|"
    r"push_all|write_all|\.push\b)"
)
READ_CONTEXT = re.compile(
    r"(?:==|!=|starts_with|ends_with|split_\w*|read_\w*|take::|position|"
    r"matches!|filter|strip|expect_\w*|contains)"
)
TAG_CONTEXT_WINDOW = 120


def _nearest_marker(code, pos):
    """Classify a tag occurrence by its closest write/read marker.

    A ±80-char window cannot separate adjacent `extend_from_slice` and
    `ends_with` lines, so the verdict goes to whichever marker sits closer
    to the token — the call wrapping the token always wins over a call on
    a neighbouring line.
    """
    best = None
    for kind, pat in (("write", WRITE_CONTEXT), ("read", READ_CONTEXT)):
        for match in pat.finditer(
                code, max(0, pos - TAG_CONTEXT_WINDOW),
                pos + TAG_CONTEXT_WINDOW):
            dist = min(abs(match.start() - pos), abs(match.end() - pos))
            if best is None or dist < best[0]:
                best = (dist, kind)
    return best[1] if best else None


def _tag_evidence(corpus, const_name, literal, decl_spans):
    """Classify every production occurrence of a save tag's tokens.

    `decl_spans` are the `[start, end)` ranges of the `const …MAGIC… = b"…";`
    statements binding this tag — occurrences inside them are the
    declaration, not a use.
    """
    writes, reads, other = [], [], []
    pats = []
    if const_name:
        pats.append(re.compile(rf"(?<![A-Za-z0-9_]){re.escape(const_name)}"
                               r"(?![A-Za-z0-9_])"))
    pats.append(re.compile(rf'\bb"{re.escape(literal)}"'))
    for rel, entry in corpus.items():
        code = entry["str_code"]
        spans = decl_spans.get(rel, [])
        for pat in pats:
            for match in pat.finditer(code):
                pos = match.start()
                if any(s <= pos < e for s, e in spans):
                    continue
                site = f"{rel}:{line_of(entry['raw'], pos)}"
                kind = _nearest_marker(code, pos)
                if kind == "write":
                    writes.append(site)
                elif kind == "read":
                    reads.append(site)
                else:
                    other.append(site)
    return writes, reads, other


def check_save_tags(corpus):
    findings = []
    entry = corpus.get(SAVE_TAG_FILE)
    if entry is None:
        return [{
            "check": "asymmetric_save_tag",
            "symbol": SAVE_TAG_FILE,
            "file": SAVE_TAG_FILE,
            "line": 0,
            "detail": "save_v2.rs missing — tag roundtrips cannot be audited",
        }], 0, 0
    code = entry["str_code"]
    tags = {}
    decl_spans = {SAVE_TAG_FILE: []}
    for match in TAG_CONST.finditer(code):
        name, value = match.group(1), match.group(2)
        tag = tags.setdefault(value, {"consts": set(), "line": 0})
        tag["consts"].add(name)
        tag["line"] = line_of(entry["raw"], match.start())
        stmt_end = code.find(";", match.end())
        decl_spans[SAVE_TAG_FILE].append(
            (match.start(), stmt_end + 1 if stmt_end >= 0 else match.end()))
    for match in TAG_LITERAL.finditer(code):
        value = match.group(1)
        tag = tags.setdefault(value, {"consts": set(), "line": 0})
        tag["line"] = tag["line"] or line_of(entry["raw"], match.start())
    total = len(tags)
    for value in sorted(tags):
        const_name = sorted(tags[value]["consts"])[0] if tags[value]["consts"] else None
        writes, reads, other = _tag_evidence(
            corpus, const_name, value, decl_spans)
        if writes and reads:
            continue
        if writes:
            detail = (f"tag b\"{value}\" is written ({len(writes)} site(s)) "
                      "but never read — persisted bytes no decoder strips")
        elif reads:
            detail = (f"tag b\"{value}\" is read ({len(reads)} site(s)) "
                      "but never written — dead decoder arm")
        else:
            detail = (f"tag b\"{value}\" has no write or read context "
                      f"({len(other)} bare occurrence(s)) — dead tag")
        findings.append({
            "check": "asymmetric_save_tag",
            "symbol": value,
            "file": SAVE_TAG_FILE,
            "line": tags[value]["line"],
            "detail": detail,
            "write_sites": writes,
            "read_sites": reads,
        })
    wired = total - len(findings)
    return findings, total, wired


# ---------------------------------------------------------------------------
# Check 5: server & client config runtime consumers
#
# The defect class is "the knob round-trips but changes nothing": a
# `server.toml` key exported to an environment variable nothing reads, a
# `RuntimeOpsConfig` field mirrored into a typed view nobody consumes, or
# a client setting copied between `UiSettings` and `ClientSettings` and
# back to TOML without ever reaching the renderer/session/audio/input path
# it claims to tune. Everything parses, every widget draws, every test
# stays green — and the setting silently does nothing.
#
# Evidence rules (on the same masked corpus as the other checks):
#
# * `dead_server_config_field` — `src/server_config.rs` key tables
#   (`SERVER_CONFIG_KEYS`, `SERVER_PROPERTIES_KEYS`,
#   `SERVER_CONFIG_ANTICHEAT_KEYS`), `live_*` accessors and
#   `RuntimeOpsConfig` fields; `server-files/src/properties.rs`
#   `ServerProperties` accessor pairs and `ServerFairPlayPolicy` fields.
#   A key is wired when its `MEGU_*` variable is read outside
#   `server_config.rs`, or when the read inside `server_config.rs` reaches
#   a live sink: an atomic whose `live_*` accessor has a real caller, or a
#   `RuntimeOpsConfig`/`AcConfig` field that outside code touches.
#   `src/test_support.rs` — reachable only through `#[cfg(test)] mod` —
#   and `#[cfg(test)]`-scrubbed code never count.
# * `dead_client_config_field` — `ClientSettings` + nested-struct fields
#   (`client-app/src/settings.rs`) and `UiSettings` + nested-struct fields
#   (`client-ui/src/state.rs`). Reads inside `settings.rs`/`state.rs`, the
#   `apply_client_settings_to_ui`/`ui_to_client_settings` copy bodies and
#   the `settings`/`*_tab` screens in `client-ui/src/layout.rs` are
#   plumbing. A field is wired when outside code reads it (an accessor
#   like `fov_f32()` counts when the accessor itself has a live caller) or
#   when the bridge maps it to a counterpart that is wired.
# ---------------------------------------------------------------------------

SERVER_CONFIG_FILE = "src/server_config.rs"
ANTICHEAT_FILE = "src/anticheat.rs"
PROPERTIES_FILE = "server-files/src/properties.rs"
CLIENT_SETTINGS_FILE = "client-app/src/settings.rs"
UI_STATE_FILE = "client-ui/src/state.rs"
UI_LAYOUT_FILE = "client-ui/src/layout.rs"
BRIDGE_FILE = "client-gpu/src/window.rs"
BRIDGE_FNS = {"apply_client_settings_to_ui", "ui_to_client_settings"}

CS_SCOPE_DIRS = ("client-app/src/", "client-gpu/src/")
UI_SCOPE_DIRS = ("client-ui/src/", "client-gpu/src/")
PROPERTIES_SCOPE_DIRS = ("server-files/",)

# `#[cfg(test)] mod name;` declarations — per-item masking cannot see them
# (the masked file leaves `mod test_support;` unresolvable), so a whole
# file that is only reachable through one is test code for every check.
TEST_GATED_MOD = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+"
    r"([a-z_][a-z0-9_]*)\s*;")

# A `MEGU_*` literal counts as a read only next to lookup vocabulary —
# `is_set`-style plumbing and `set_var`/`env_remove`/`Command::env` writes
# never carry it. `get("VAR")` covers the `impl Fn(&str) -> Option<String>`
# getter indirection used by `compression_threshold_env` & friends.
ENV_READ = re.compile(
    r"(?:env::var|var_os|env_flag|env_parse|option_env!|getenv|\bget\s*\()")
ENV_READ_WINDOW = 160
# A `"VAR"` literal that is the first argument of an env *write* —
# `command.env("VAR", …)`, `set_var("VAR", …)`, `remove_var("VAR")`,
# `W4Env::new().set("VAR", …)` — is plumbing, never a consumer.
ENV_WRITE_CALL = re.compile(
    r"(?:\.envs?|\.env_os|set_var|set_var_os|remove_var|env_remove|"
    r"\.set)\s*\(\s*$")

# Methods on the audited config/settings structs that are pure
# serialization/copy/format plumbing — a caller of one is not a runtime
# consumer of the fields the body reads.
CONFIG_SINK_METHOD = re.compile(
    r"^(?:default|from_preset|new|parse|parse_bytes|fmt|unquote|"
    r"to_toml|from_toml|parse_toml|apply_toml|write_toml|"
    r"import_vanilla_options_txt|apply_vanilla_options_txt|"
    r"from_doc|summary|redacted_secret|doc|doc_mut|get|set|remove|keys|"
    r"load_from|save_to|render|as_str|as_env|to_env|name|next|step|"
    r"[a-z_0-9]*_(?:label|status))$")

# [anticheat] doc keys resolve onto these AcConfig fields in `from_doc`.
ANTICHEAT_KEY_FIELD = {
    "antixray": "anti_xray",
    "antifullbright": "anti_fullbright",
    "hide_underground": "hide_underground",
    "check_ops": "check_ops",
    "enabled": "preset",
    "preset": "preset",
    "alerts": "alerts",
    "exempt": "exempt",
    "exempt_ops": "check_ops",
    "ops_bypass": "check_ops",
}
# AcConfig resolution helpers: field reads inside them are parsing, not
# consumption.
ANTICHEAT_PLUMBING_FNS = ("from_doc", "ac_unbooted_default", "summary")

STRUCT_DECL = re.compile(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)")
FIELD_DECL = re.compile(
    r"(?:^|\n)\s*(?:#\s*\[[^\]]*\]\s*)*"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:\s*([^,{;\n]+)")
IMPL_HEAD = re.compile(
    r"\bimpl\b\s*(?:<[^>{}]*>\s*)?"
    r"(?:[A-Za-z_][\w:<>]*\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)")
SELF_FIELD = re.compile(
    r"\bself\.([a-z_][a-z0-9_]*(?:\.[a-z_][a-z0-9_]*)*)")
TYPE_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def struct_fields(code):
    """`{name: (brace_pos, end, [(field, type, field_pos)])}` for structs."""
    out = {}
    for match in STRUCT_DECL.finditer(code):
        pos, depth, brace = match.end(), 0, -1
        while pos < len(code):
            ch = code[pos]
            if ch == "<":
                depth += 1
            elif ch == ">":
                depth = max(0, depth - 1)
            elif depth == 0 and ch == "{":
                brace = pos
                break
            elif depth == 0 and ch in "(;":
                break
            pos += 1
        if brace < 0:
            continue
        end = _balanced_end(code, brace, "{", "}")
        fields = [
            (m.group(1), m.group(2).strip(), brace + 1 + m.start(1))
            for m in FIELD_DECL.finditer(code[brace + 1:end])
        ]
        out[match.group(1)] = (brace, end, fields)
    return out


def _child_struct(structs, name, ftype):
    """The first same-file struct named in a field type (or `None`)."""
    return next(
        (t for t in TYPE_IDENT.findall(ftype)
         if t in structs and t != name), None)


def struct_paths(structs, root):
    """`{struct_name: field-path tuple from `root`}` — `()` for root itself."""
    paths = {}

    def walk(name, path):
        if name in paths:
            return
        paths[name] = path
        for field, ftype, _pos in structs.get(name, (0, 0, []))[2]:
            child = _child_struct(structs, name, ftype)
            if child is not None:
                walk(child, path + (field,))

    walk(root, ())
    return paths


def leaf_fields(structs, root):
    """`(path_tuple, field_pos)` for every leaf field under `root`'s tree."""
    leaves = []

    def walk(name, path):
        for field, ftype, pos in structs.get(name, (0, 0, []))[2]:
            child = _child_struct(structs, name, ftype)
            if child is not None:
                walk(child, path + (field,))
            else:
                leaves.append((path + (field,), pos))

    walk(root, ())
    return leaves


def impl_method_fields(code, struct_names):
    """`{(struct, method): {self-field-paths}}` for non-sink methods.

    A method like `fn fov_f32(&self) -> f32 { self.fov_deg * … }` maps the
    accessor name to the fields it reads; calls to the accessor in live
    code count as consumption of those fields.
    """
    methods = {}
    for match in IMPL_HEAD.finditer(code):
        name = match.group(1)
        if name not in struct_names:
            continue
        brace = code.find("{", match.end(), match.end() + 200)
        if brace < 0:
            continue
        block_end = _balanced_end(code, brace, "{", "}")
        for fname, fstart, fbstart, fend in fn_ranges(
                code[brace:block_end]):
            del fstart
            if CONFIG_SINK_METHOD.match(fname):
                continue
            methods.setdefault((name, fname), {
                tuple(p.split(".")) for p in
                SELF_FIELD.findall(code[brace + fbstart:brace + fend])
            })
    return methods


def test_gated_files(corpus):
    """Files only reachable through a `#[cfg(test)] mod` declaration."""
    gated = set()
    for rel, entry in corpus.items():
        for match in TEST_GATED_MOD.finditer(entry["raw"]):
            line_start = entry["raw"].rfind("\n", 0, match.start()) + 1
            if entry["raw"][line_start:match.start()].lstrip().startswith(
                    ("//", "/*", "*")):
                continue
            parent = rel.rsplit("/", 1)[0]
            for cand in (f"{parent}/{match.group(1)}.rs",
                         f"{parent}/{match.group(1)}/mod.rs"):
                if cand in corpus:
                    gated.add(cand)
    return gated


def _is_write(code, pos):
    """True when the token ending at `pos` is assignment LHS (`=`, `+=`…)."""
    i = _skip_space(code, pos)
    if i >= len(code):
        return False
    if code[i] == "=":
        return i + 1 >= len(code) or code[i + 1] != "="
    return (code[i] in "+-*/%|&^" and i + 1 < len(code)
            and code[i + 1] == "=")


def evidence_sites(corpus, needle, scope_dirs, skip_files, skip_spans):
    """Non-write `.field`/`name(` sites within scope, outside exclusions."""
    sites = []
    for rel, entry in corpus.items():
        if scope_dirs and not rel.startswith(scope_dirs):
            continue
        if rel in skip_files:
            continue
        code = entry["code"]
        spans = skip_spans.get(rel, ())
        for match in needle.finditer(code):
            pos = match.start()
            if any(s <= pos < e for s, e in spans):
                continue
            if _is_write(code, match.end()):
                continue
            sites.append(f"{rel}:{line_of(entry['raw'], pos)}")
    return sites


def _fn_span_map(corpus, rel, wanted=None, name_pat=None):
    entry = corpus.get(rel)
    if entry is None:
        return {}
    out = {}
    for name, start, _bs, end in fn_ranges(entry["code"]):
        if wanted is not None and name not in wanted:
            continue
        if name_pat is not None and not name_pat.match(name):
            continue
        out[name] = (start, end)
    return out


def _table_entries(str_code, table_name):
    """`[(key, env_var, pos)]` for a `const NAME: &[(..)] = &[(k,v),…]`."""
    m = re.search(rf"\b{re.escape(table_name)}\b[^=]*=\s*&\[", str_code)
    if m is None:
        return []
    start = str_code.index("[", m.end() - 2)
    region = str_code[start:_balanced_end(str_code, start, "[", "]")]
    return [
        (e.group(1), e.group(2), start + e.start())
        for e in re.finditer(
            r'\(\s*"([\w.-]+)"\s*,\s*"(MEGU_[A-Z0-9_]+)"\s*\)', region)
    ]


def _str_list_entries(str_code, table_name):
    """`[(item, pos)]` for a `const NAME: &[&str] = &["a",…]` table."""
    m = re.search(rf"\b{re.escape(table_name)}\b[^=]*=\s*&\[", str_code)
    if m is None:
        return []
    start = str_code.index("[", m.end() - 2)
    region = str_code[start:_balanced_end(str_code, start, "[", "]")]
    return [
        (e.group(1), start + e.start())
        for e in re.finditer(r'"(\w+)"', region)
    ]


def _field_pat(path):
    """`.field` or `.parent.field` read pattern for a leaf path tuple."""
    field = path[-1]
    if len(path) > 1:
        chain = r"\.".join(re.escape(p) for p in path[:-1])
        return re.compile(
            rf"\.(?:{chain}\.)?{re.escape(field)}\b")
    return re.compile(rf"\.{re.escape(field)}\b")


def bridge_links(corpus):
    """`{cs_path: {ui_path}}` + `{ui_path: {cs_path}}` from the copy fns.

    `apply_client_settings_to_ui` statements move `src.<cs>` → `ui.<ui>`;
    `ui_to_client_settings` moves `ui.<ui>` → `out.<cs>`. Any statement
    touching both a source and a destination path links them.
    """
    entry = corpus.get(BRIDGE_FILE)
    cs_ui, ui_cs = {}, {}
    if entry is None:
        return cs_ui, ui_cs
    code = entry["code"]
    for name, _s, bstart, end in fn_ranges(code):
        if name not in BRIDGE_FNS:
            continue
        for stmt in code[bstart:end].split(";"):
            uis = re.findall(r"\bui\.([a-z_][\w.]*)", stmt)
            srcs = re.findall(r"\bsrc\.([a-z_][\w.]*)", stmt)
            outs = re.findall(r"\bout\.([a-z_][\w.]*)", stmt)
            for u in uis:
                up = tuple(u.split("."))
                for s in srcs:
                    cs_ui.setdefault(tuple(s.split(".")), set()).add(up)
                for o in outs:
                    ui_cs.setdefault(up, set()).add(tuple(o.split(".")))
    return cs_ui, ui_cs


def check_client_config(corpus, gated):
    """Check 5b: dead `ClientSettings` / `UiSettings` fields."""
    cs_entry = corpus.get(CLIENT_SETTINGS_FILE)
    ui_entry = corpus.get(UI_STATE_FILE)
    # Plumbing spans: bridge copy bodies + the UI settings screens.
    skip_spans = {}
    for _name, span in _fn_span_map(
            corpus, BRIDGE_FILE, wanted=BRIDGE_FNS).items():
        skip_spans.setdefault(BRIDGE_FILE, []).append(span)
    tab_pat = re.compile(r"^(?:settings|\w+_tab)$")
    for _name, span in _fn_span_map(
            corpus, UI_LAYOUT_FILE, name_pat=tab_pat).items():
        skip_spans.setdefault(UI_LAYOUT_FILE, []).append(span)

    sides = []
    for entry, root, scope, skip_file in (
            (cs_entry, "ClientSettings", CS_SCOPE_DIRS,
             CLIENT_SETTINGS_FILE),
            (ui_entry, "UiSettings", UI_SCOPE_DIRS, UI_STATE_FILE)):
        if entry is None:
            continue
        structs = struct_fields(entry["code"])
        sides.append({
            "entry": entry,
            "root": root,
            "scope": scope,
            "skip": {skip_file} | gated,
            "paths": struct_paths(structs, root),
            "methods": impl_method_fields(entry["code"], set(structs)),
            "candidates": [
                (path, line_of(entry["raw"], pos))
                for path, pos in leaf_fields(structs, root)
            ],
        })

    # symbol -> {"line", "file", "path", "sites"}
    direct = {}
    for side in sides:
        for path, line in side["candidates"]:
            symbol = f"{side['root']}." + ".".join(path)
            direct[symbol] = {
                "line": line,
                "path": path,
                "root": side["root"],
                "sites": evidence_sites(
                    corpus, _field_pat(path), side["scope"],
                    side["skip"], skip_spans),
            }
    # Accessor calls in scope mark the fields the accessor's body reads
    # (exact, ancestor, and descendant candidate paths all count: reading
    # `self.mini_hud.enabled` consumes the `mini_hud` leaf).
    for side in sides:
        for (sname, fname), spaths in side["methods"].items():
            owner = side["paths"].get(sname)
            if owner is None:
                continue
            sites = evidence_sites(
                corpus, re.compile(rf"\.{fname}\s*\("),
                side["scope"], side["skip"], skip_spans)
            if not sites:
                continue
            for spath in spaths:
                cp = owner + spath
                for symbol, cand in direct.items():
                    if cand["root"] != side["root"]:
                        continue
                    cpath = cand["path"]
                    if (cp[:len(cpath)] == cpath
                            or cpath[:len(cp)] == cp):
                        cand["sites"].extend(sites)

    # Wiring = direct read evidence, propagated across the UiSettings ↔
    # ClientSettings bridge links (either direction suffices).
    cs_ui, ui_cs = bridge_links(corpus)
    wired_set = {s for s, c in direct.items() if c["sites"]}
    changed = True
    while changed:
        changed = False
        for csp, uis in cs_ui.items():
            sym = "ClientSettings." + ".".join(csp)
            if sym in direct and sym not in wired_set and any(
                    "UiSettings." + ".".join(u) in wired_set for u in uis):
                wired_set.add(sym)
                changed = True
        for uip, css in ui_cs.items():
            sym = "UiSettings." + ".".join(uip)
            if sym in direct and sym not in wired_set and any(
                    "ClientSettings." + ".".join(c) in wired_set
                    for c in css):
                wired_set.add(sym)
                changed = True

    findings = []
    for symbol in sorted(direct):
        if symbol in wired_set:
            continue
        cand = direct[symbol]
        findings.append({
            "check": "dead_client_config_field",
            "symbol": symbol,
            "file": CLIENT_SETTINGS_FILE
                    if cand["root"] == "ClientSettings"
                    else UI_STATE_FILE,
            "line": cand["line"],
            "detail": (
                f"{'.'.join(cand['path'])} is parsed/serialized and copied "
                "through the UI↔settings bridge but no live "
                "renderer/session/audio/input path ever reads it"),
        })
    total = len(direct)
    return findings, total, total - len(findings)


def check_server_config(corpus, gated):
    """Check 5a: dead server config keys, atomics, and accessor pairs."""
    findings = []
    total = 0
    sc = corpus.get(SERVER_CONFIG_FILE)
    props = corpus.get(PROPERTIES_FILE)
    if sc is None and props is None:
        return [], 0, 0

    live_candidates = {}   # accessor -> (line, static)
    live_wired = {}        # accessor -> bool
    runtime_live = {}      # field path str -> bool
    inside_sites = {}      # env var -> [("field"|"static", name)]
    key_entries = []       # (table_label, key, env, pos)

    if sc is not None:
        sc_code, sc_str, sc_raw = sc["code"], sc["str_code"], sc["raw"]
        fns = fn_ranges(sc_code)

        # --- `live_*` accessors and the `LIVE_*` atomics they serve ---
        for name, start, bstart, end in fns:
            if not name.startswith("live_"):
                continue
            m = re.search(r"\bLIVE_[A-Z0-9_]+\b", sc_code[bstart:end])
            static = m.group(0) if m else "LIVE_" + name[5:].upper()
            live_candidates[name] = (line_of(sc_raw, start), static)
        for name, (line, static) in live_candidates.items():
            needle = re.compile(
                rf"\b(?:{re.escape(name)}|{re.escape(static)})\b")
            live_wired[name] = bool(evidence_sites(
                corpus, needle, None, {SERVER_CONFIG_FILE} | gated, {}))

        # --- `RuntimeOpsConfig` fields: `.field` reads in files that ---
        # actually touch the typed view (a same-named local struct field
        # must not satisfy the check).
        structs = struct_fields(sc_code)
        rt_files = {
            rel for rel, entry in corpus.items()
            if rel != SERVER_CONFIG_FILE and rel not in gated
            and re.search(r"\b(?:RuntimeOpsConfig|runtime_ops)\b",
                          entry["code"])
        }
        for path, _pos in leaf_fields(structs, "RuntimeOpsConfig"):
            runtime_live[".".join(path)] = any(
                s.split(":", 1)[0] in rt_files
                for s in evidence_sites(
                    corpus, _field_pat(path), None,
                    {SERVER_CONFIG_FILE} | gated, {}))

        # --- key tables → env vars --------------------------------------
        for label, table in (("[server]", "SERVER_CONFIG_KEYS"),
                             ("server.properties",
                              "SERVER_PROPERTIES_KEYS")):
            for key, env, pos in _table_entries(sc_str, table):
                key_entries.append((label, key, env, pos))

        # Env reads *inside* server_config.rs only count when they feed a
        # live sink: attribute each literal to the `RuntimeOpsConfig` field
        # it fills (the `from_doc` literal + helpers invoked from it) or
        # the `LIVE_*` atomic it writes (the apply_runtime_server_config
        # args inside `server_config_boot`).
        rt_fields = {p[-1] for p, _pos in
                     leaf_fields(structs, "RuntimeOpsConfig")}
        fd_span = next(
            ((s, e) for n, s, _b, e in fns if n == "from_doc"), None)
        fn_field = {}
        if fd_span:
            for m in re.finditer(
                    r"([a-z_]\w*)\s*:\s*([a-z_]\w*)\s*\(",
                    sc_code[fd_span[0]:fd_span[1]]):
                if m.group(1) in rt_fields:
                    fn_field[m.group(2)] = m.group(1)

        def _enclosing(pos):
            return next((n for n, s, _b, e in fns if s <= pos < e), None)

        for m in re.finditer(r'"(MEGU_[A-Z0-9_]+)"', sc_str):
            if ENV_WRITE_CALL.search(
                    sc_str[max(0, m.start() - 40):m.start()]):
                continue
            window = sc_str[max(0, m.start() - ENV_READ_WINDOW):
                            m.end() + ENV_READ_WINDOW]
            if not re.search(r"(?:env::var|env\s*\()", window):
                continue
            env = m.group(1)
            fn = _enclosing(m.start())
            sink = None
            if fn == "from_doc" and fd_span:
                # nearest `field:` label backwards inside the literal
                cols = list(re.finditer(
                    r"([a-z_]\w*)\s*:", sc_code[fd_span[0]:m.start()]))
                if cols and cols[-1].group(1) in rt_fields:
                    sink = ("field", cols[-1].group(1))
            elif fn in fn_field:
                sink = ("field", fn_field[fn])
            elif fn == "server_config_boot":
                static = "LIVE_" + env[5:]
                if static in sc_code:
                    sink = ("static", static)
            if sink is not None:
                inside_sites.setdefault(env, []).append(sink)

        def _static_live(name):
            for acc, (_line, st) in live_candidates.items():
                if st == name:
                    return live_wired.get(acc, False)
            return bool(evidence_sites(
                corpus, re.compile(rf"\b{re.escape(name)}\b"), None,
                {SERVER_CONFIG_FILE} | gated, {}))

        env_live = {
            env: bool(_env_read_sites(
                corpus, env, {SERVER_CONFIG_FILE} | gated)) or any(
                    _static_live(name) if kind == "static"
                    else runtime_live.get(name, False)
                    for kind, name in inside_sites.get(env, ()))
            for _label, _key, env, _pos in key_entries
        }

        # --- findings: accessors, struct fields, env vars ---------------
        for name, (line, static) in live_candidates.items():
            total += 1
            if not live_wired[name]:
                findings.append({
                    "check": "dead_server_config_field",
                    "symbol": name,
                    "file": SERVER_CONFIG_FILE,
                    "line": line,
                    "detail": (
                        f"{static} is written by apply_runtime_server_"
                        f"config but {name}() has no non-test caller "
                        "outside server_config.rs — the runtime value is "
                        "stored, never consumed"),
                })
        for path, pos in leaf_fields(structs, "RuntimeOpsConfig"):
            total += 1
            if not runtime_live.get(".".join(path), False):
                findings.append({
                    "check": "dead_server_config_field",
                    "symbol": f"RuntimeOpsConfig.{'.'.join(path)}",
                    "file": SERVER_CONFIG_FILE,
                    "line": line_of(sc_raw, pos),
                    "detail": (
                        "RuntimeOpsConfig field is parsed into the typed "
                        "view but no non-test code outside server_config.rs"
                        " ever reads runtime_ops()/RuntimeOpsConfig"
                        + ("" if rt_files else " at all")
                        + " — stored, never consumed"),
                })
        for env in env_live:
            total += 1
            if not env_live[env]:
                keys = sorted({f"{label} `{key}`" for label, key, e, _p
                               in key_entries if e == env})
                pos = next(p for _l, _k, e, p in key_entries if e == env)
                findings.append({
                    "check": "dead_server_config_field",
                    "symbol": env,
                    "file": SERVER_CONFIG_FILE,
                    "line": line_of(sc_raw, pos),
                    "detail": (
                        f"{', '.join(keys)} exports to {env} but no "
                        "non-test code reads the variable — the setting "
                        "is stored/exported, never consumed"),
                })

        # --- [anticheat] keys → AcConfig fields --------------------------
        ac_skip = {ANTICHEAT_FILE: [
            span for _name, span in _fn_span_map(
                corpus, ANTICHEAT_FILE,
                wanted=set(ANTICHEAT_PLUMBING_FNS)).items()]}
        for key, pos in _str_list_entries(
                sc_str, "SERVER_CONFIG_ANTICHEAT_KEYS"):
            field = ANTICHEAT_KEY_FIELD.get(key)
            if field is None:
                continue
            total += 1
            if not evidence_sites(
                    corpus, re.compile(rf"\.{re.escape(field)}\b"),
                    ("src/",), {SERVER_CONFIG_FILE} | gated, ac_skip):
                findings.append({
                    "check": "dead_server_config_field",
                    "symbol": f"anticheat.{key}",
                    "file": SERVER_CONFIG_FILE,
                    "line": line_of(sc_raw, pos),
                    "detail": (
                        f"[anticheat] key `{key}` resolves to "
                        f"AcConfig.{field}, which no non-test code outside "
                        "the parser ever reads — parsed, never consumed"),
                })

    # --- ServerProperties accessor pairs + ServerFairPlayPolicy ---------
    if props is not None:
        methods = _impl_block_fns(props["code"], "ServerProperties")
        pairs = {}
        for name, (start, end) in methods.items():
            head = props["code"][start:props["code"].find("{", start, end)]
            if re.search(rf"fn\s+{re.escape(name)}\s*\(\s*&self\s*\)",
                         head):
                pairs[name] = start
            m = re.match(r"set_(\w+)", name)
            if m and "&mut self" in head:
                pairs.setdefault(m.group(1), start)
        for name in sorted(pairs):
            needle = re.compile(
                rf"\.(?:{re.escape(name)}|set_{re.escape(name)})\s*\(")
            sites = evidence_sites(
                corpus, needle, PROPERTIES_SCOPE_DIRS,
                {PROPERTIES_FILE} | gated, {})
            total += 1
            if not sites:
                findings.append({
                    "check": "dead_server_config_field",
                    "symbol": f"ServerProperties.{name}",
                    "file": PROPERTIES_FILE,
                    "line": line_of(props["raw"], pairs[name]),
                    "detail": (
                        f"server.properties accessor {name}()/set_{name}()"
                        " is never called outside properties.rs — the key "
                        "round-trips the file but no runtime path "
                        "consumes it"),
                })

        # ServerFairPlayPolicy fields: live only where the policy itself
        # is touched (`fair_play_policy()` call or a non-`use`
        # ServerFairPlayPolicy reference in the file).
        policy_files = set()
        for rel, entry in corpus.items():
            if rel == PROPERTIES_FILE or rel in gated:
                continue
            if not rel.startswith(PROPERTIES_SCOPE_DIRS):
                continue
            code = entry["code"]
            if re.search(r"\.fair_play_policy\s*\(", code):
                policy_files.add(rel)
            elif "ServerFairPlayPolicy" in re.sub(
                    r"\buse\b[^;]*;", " ", code, flags=re.S):
                policy_files.add(rel)
        for path, pos in leaf_fields(
                struct_fields(props["code"]), "ServerFairPlayPolicy"):
            sites = [
                s for s in evidence_sites(
                    corpus, _field_pat(path), PROPERTIES_SCOPE_DIRS,
                    {PROPERTIES_FILE} | gated, {})
                if s.split(":", 1)[0] in policy_files]
            total += 1
            if not sites:
                findings.append({
                    "check": "dead_server_config_field",
                    "symbol": f"ServerFairPlayPolicy.{'.'.join(path)}",
                    "file": PROPERTIES_FILE,
                    "line": line_of(props["raw"], pos),
                    "detail": (
                        "ServerFairPlayPolicy field is parsed from "
                        "server.properties but the policy is never "
                        "fetched/constructed outside properties.rs — "
                        "stored, never consumed"),
                })
    wired = total - len(findings)
    return findings, total, wired


def _env_read_sites(corpus, var, skip_files):
    """Literal `"VAR"` occurrences next to a read marker, outside skips."""
    hits = []
    lit = re.compile(rf'"{re.escape(var)}"')
    for rel, entry in corpus.items():
        if rel in skip_files:
            continue
        code = entry["str_code"]
        if var not in code:
            continue
        for m in lit.finditer(code):
            if ENV_WRITE_CALL.search(
                    code[max(0, m.start() - 40):m.start()]):
                continue
            window = code[max(0, m.start() - ENV_READ_WINDOW):
                          m.end() + ENV_READ_WINDOW]
            if ENV_READ.search(window):
                hits.append(rel)
                break
    return hits


def _impl_block_fns(code, type_name):
    """`{method: (fn_start, fn_end)}` inside `impl Type { … }` blocks."""
    out = {}
    for match in IMPL_HEAD.finditer(code):
        if match.group(1) != type_name:
            continue
        brace = code.find("{", match.end(), match.end() + 200)
        if brace < 0:
            continue
        block_end = _balanced_end(code, brace, "{", "}")
        for name, start, _b, end in fn_ranges(code[brace:block_end]):
            out[name] = (brace + start, brace + end)
    return out


# ---------------------------------------------------------------------------
# Waivers
# ---------------------------------------------------------------------------

def load_waivers(root):
    path = root / WAIVERS_REL
    if not path.is_file():
        return []
    data = json.loads(path.read_text(encoding="utf-8"))
    return data.get("waivers", [])


def waiver_matches(waiver, finding):
    if waiver.get("check") != finding["check"]:
        return False
    if waiver.get("symbol") != finding["symbol"]:
        return False
    waiver_file = waiver.get("file")
    if waiver_file and waiver_file != "*" and waiver_file != finding.get("file"):
        return False
    return True


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

CHECK_ORDER = (
    "dead_tick_or_bridge",
    "one_sided_channel",
    "asymmetric_save_tag",
    "unwired_subsystem_entry",
    "dead_server_config_field",
    "dead_client_config_field",
)


def run_audit(root):
    corpus = build_corpus(root)
    dead_findings, dead_totals, dead_wired = check_dead_functions(corpus)
    chan_findings, chan_total, chan_wired = check_channels(corpus)
    tag_findings, tag_total, tag_wired = check_save_tags(corpus)
    gated = test_gated_files(corpus)
    srv_findings, srv_total, srv_wired = check_server_config(corpus, gated)
    cli_findings, cli_total, cli_wired = check_client_config(corpus, gated)

    stats = {
        "files_scanned": len(corpus),
        "candidates": {
            "dead_tick_or_bridge": dead_totals["dead_tick_or_bridge"],
            "one_sided_channel": chan_total,
            "asymmetric_save_tag": tag_total,
            "unwired_subsystem_entry": dead_totals["unwired_subsystem_entry"],
            "dead_server_config_field": srv_total,
            "dead_client_config_field": cli_total,
        },
        "wired": {
            "dead_tick_or_bridge": dead_wired["dead_tick_or_bridge"],
            "one_sided_channel": chan_wired,
            "asymmetric_save_tag": tag_wired,
            "unwired_subsystem_entry": dead_wired["unwired_subsystem_entry"],
            "dead_server_config_field": srv_wired,
            "dead_client_config_field": cli_wired,
        },
    }
    all_findings = (dead_findings + chan_findings + tag_findings
                    + srv_findings + cli_findings)

    waivers = load_waivers(root)
    unwaived, waived = [], []
    for finding in all_findings:
        if any(waiver_matches(w, finding) for w in waivers):
            waived.append(finding)
        else:
            unwaived.append(finding)
    stale = [
        w for w in waivers
        if not any(waiver_matches(w, f) for f in all_findings)
    ]
    return {
        "stats": stats,
        "findings": all_findings,
        "waived": waived,
        "unwaived": unwaived,
        "stale_waivers": stale,
    }


def _finding_loc(finding):
    if finding.get("file") and finding.get("line"):
        return f"{finding['file']}:{finding['line']}"
    if finding.get("sites"):
        return finding["sites"][0]
    return finding.get("file") or "-"


def print_human(report):
    stats = report["stats"]
    print(f"wiring_connectivity_audit: scanned {stats['files_scanned']} files")
    for check in CHECK_ORDER:
        findings = [f for f in report["findings"] if f["check"] == check]
        waived = [f for f in report["waived"] if f["check"] == check]
        unwaived = [f for f in report["unwaived"] if f["check"] == check]
        total = stats["candidates"].get(check, len(findings))
        wired = stats["wired"].get(check, 0)
        print(f"[{check}] {total} candidate(s): "
              f"{wired} wired, {len(waived)} waived, "
              f"{len(unwaived)} unwaived")
        for finding in findings:
            tag = "WAIVED" if finding in waived else "FAIL"
            extra = f" — {finding['detail']}"
            print(f"  {tag} {finding['symbol']} @ {_finding_loc(finding)}{extra}")
            for site in finding.get("sites", [])[1:]:
                print(f"       site {site}")
    if report["stale_waivers"]:
        print(f"stale waivers: {len(report['stale_waivers'])} "
              "(item wired or removed — prune the manifest)")
        for waiver in report["stale_waivers"]:
            print(f"  STALE {waiver.get('check')}:{waiver.get('symbol')}"
                  f" ({waiver.get('reason', 'no reason given')})")
    if report["unwaived"]:
        print(f"wiring_connectivity_audit: FAIL — "
              f"{len(report['unwaived'])} unwaived finding(s)")
    else:
        print("wiring_connectivity_audit: PASS "
              f"({len(report['waived'])} waived finding(s) tracked)")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument("--json", action="store_true",
                        help="emit the machine-readable report")
    parser.add_argument("--fail-on-stale", action="store_true",
                        help="fail when a waiver no longer matches any finding")
    args = parser.parse_args(argv)

    report = run_audit(args.root)
    if args.json:
        out = {k: v for k, v in report.items() if not k.startswith("_")}
        print(json.dumps(out, indent=2, sort_keys=True))
    else:
        print_human(report)

    if report["unwaived"]:
        return 1
    if args.fail_on_stale and report["stale_waivers"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
