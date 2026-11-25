#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""server.properties key-table & JSON-escaper parity audit (W4/W5).

Two drift classes this gate pins, both pure source parsing — no build, no
JAR, no ``assets/``:

1. **Key-table parity.** ``src/server_config.rs::SERVER_PROPERTIES_KEYS``
   maps the Vanilla ``server.properties`` keys the runtime honours to their
   ``MEGU_*`` environment variables. ``server-files/src/properties.rs``'s
   typed ``ServerProperties`` accessors name the keys the dedicated-server
   file layer reads and writes. The two sets legitimately differ today —
   server-files supports more Vanilla keys than the runtime maps, and the
   runtime maps a few keys server-files does not type — so the gate does
   not demand equality. It demands the *documented* delta: the exact
   intersection, the exact root-only set and the exact server-files-only
   set are ratcheted against ``EXPECTED_ROOT_ONLY``/``EXPECTED_SERVER_FILES_ONLY``.
   Any NEW divergence in either direction fails immediately instead of
   silently appearing.

2. **Canonical JSON escaper.** ``server-files/src/json.rs`` is the crate's
   JSON string escaper: since ``ac6f73bb`` it delegates to the
   workspace-canonical ``megu_core::json`` (``quote`` ->
   ``megu_core::json::quote_json_string``), and before that it carried the
   local ``quote`` implementation. Both shapes pass; a ``json.rs`` with
   neither fails. Inside ``server-files/src`` no *other* file may define a
   function that emits JSON string escapes — a second escaper is exactly
   the drift the dedup lanes removed, and a new one fails here.

Test code cannot satisfy or trip the checks: comments, literals and
``#[cfg(test)]`` items are masked before call sites are located (same
technique as ``tools/opt_wiring_audit.py``), while the key strings
themselves are read back from the raw source inside the located spans.

Usage:
    tools/server_files_parity.py            # human PASS/FAIL summary
    tools/server_files_parity.py --root DIR # audit a synthetic tree
"""

import argparse
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

CONFIG_FILE = "src/server_config.rs"
PROPERTIES_FILE = "server-files/src/properties.rs"
JSON_FILE = "server-files/src/json.rs"
SERVER_FILES_SRC = "server-files/src"

# The ratcheted known delta, measured on the tree this gate was added to.
# `root-only`: keys SERVER_PROPERTIES_KEYS maps that server-files does not
# type. `server-files-only`: keys the typed ServerProperties surface reads
# or writes that the runtime's env-var table does not map.
# `enable-command-block`/`resource-pack`/`require-resource-pack` joined the
# root table when MEGU_* runtime consumers were wired (CFG-SERVER-1);
# `enforce-secure-profile` moved to the shared set for the same reason.
EXPECTED_ROOT_ONLY = frozenset(
    {
        "server-ip",
        "player-idle-timeout",
        "op-permission-level",
        "allow-flight",
        "enable-command-block",
        "resource-pack",
        "require-resource-pack",
    }
)
EXPECTED_SERVER_FILES_ONLY = frozenset(
    {
        "level-name",
        "enable-rcon",
        "enable-query",
        "query.port",
        "minimap-mode",
        "allow-cave-map",
        "allow-mob-radar",
        "player-radar-scope",
        "allow-waypoint-teleport",
    }
)

# ---------------------------------------------------------------------------
# Rust source masking — mirrors tools/opt_wiring_audit.py so comments,
# literals and cfg(test) items can never produce a finding.
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


FN_HEAD = re.compile(
    r"\b(?:pub(?:\s*\([^)]*\))?\s+)?"
    r"(?:async\s+|unsafe\s+|const\s+|extern\s+\"[A-Za-z]+\"\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def fn_ranges(code):
    """`(name, start, body_start, end)` for every `fn` with a `{…}` body."""
    fns = []
    for match in FN_HEAD.finditer(code):
        name = match.group(1)
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


# ---------------------------------------------------------------------------
# String-literal scanning (raw source, comments/chars/raw-strings aware)
# ---------------------------------------------------------------------------

def string_literals(text):
    """Yield `(start, end, content)` for every `"..."` string literal.

    Line/block comments, char literals and raw strings are skipped so a `"`
    inside them can never be mistaken for a string delimiter.
    """
    pos = 0
    n = len(text)
    while pos < n:
        if text.startswith("//", pos):
            end = text.find("\n", pos)
            pos = n if end < 0 else end
            continue
        if text.startswith("/*", pos):
            pos += 2
            depth = 1
            while pos < n and depth:
                if text.startswith("/*", pos):
                    depth += 1
                    pos += 2
                elif text.startswith("*/", pos):
                    depth -= 1
                    pos += 2
                else:
                    pos += 1
            continue
        match = RAW_STRING.match(text, pos)
        if match:
            terminator = '"' + match.group(1)
            end = text.find(terminator, match.end())
            pos = n if end < 0 else end + len(terminator)
            continue
        if CHARACTER.match(text, pos):
            pos = CHARACTER.match(text, pos).end()
            continue
        if text[pos] == '"':
            start = pos
            pos += 1
            content_start = pos
            while pos < n:
                if text[pos] == "\\":
                    pos += 2
                elif text[pos] == '"':
                    yield start, pos + 1, text[content_start:pos]
                    pos += 1
                    break
                elif text[pos] == "\n":
                    break  # unterminated; not a literal
                else:
                    pos += 1
            continue
        pos += 1


_RUST_ESCAPES = {
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "0": "\0",
    "\\": "\\",
    '"': '"',
    "'": "'",
}


def decode_rust_string(content):
    """Approximate Rust string unescaping — enough to see `\\\"` emissions."""
    out = []
    i = 0
    while i < len(content):
        char = content[i]
        if char != "\\":
            out.append(char)
            i += 1
            continue
        i += 1
        if i >= len(content):
            break
        esc = content[i]
        i += 1
        if esc == "u" and i < len(content) and content[i] == "{":
            end = content.find("}", i)
            i = len(content) if end < 0 else end + 1
            out.append("\x00")
            continue
        if esc in ("x", "X"):
            i = min(i + 2, len(content))
            out.append("\x00")
            continue
        out.append(_RUST_ESCAPES.get(esc, esc))
    return "".join(out)


# A decoded string literal containing backslash+quote is a JSON `\"` escape
# emission — the signature of a hand-rolled JSON string escaper.
JSON_QUOTE_ESCAPE = "\\" + '"'


def emits_json_escapes(body):
    return any(
        JSON_QUOTE_ESCAPE in decode_rust_string(content)
        for _, _, content in string_literals(body)
    )


def rust_string_at(text, pos):
    """The string literal starting at `pos` (after whitespace), or None."""
    pos = _skip_space(text, pos)
    if pos >= len(text) or text[pos] != '"':
        return None
    for start, _end, content in string_literals(text[pos:]):
        if start == 0:
            return _unescape_key(content)
    return None


def _unescape_key(content):
    return decode_rust_string(content)


# ---------------------------------------------------------------------------
# Check 1: SERVER_PROPERTIES_KEYS vs typed ServerProperties keys
# ---------------------------------------------------------------------------

KEY_TABLE_DECL = re.compile(r"\bSERVER_PROPERTIES_KEYS\b")
KEY_TUPLE = re.compile(
    r'\(\s*"((?:[^"\\\n]|\\.)*)"\s*,\s*"((?:[^"\\\n]|\\.)*)"\s*\)'
)

# Typed-key call sites in properties.rs, located on masked code (strings are
# blanked there) and read back from raw: doc.get("k"), doc.set("k", …) —
# including the `self\n.doc\n.get(` chain style — and the prop_* helpers whose
# first argument after `&self.doc,` is the key literal.
DOC_ACCESS = re.compile(r"(?<!\w)doc\s*\.\s*(?:get|set)\s*\(")
PROP_ACCESS = re.compile(
    r"\bprop_(?:bool|str|u16|i64)\s*\(\s*&\s*self\s*\.\s*doc\s*,\s*"
)


def config_keys(root):
    """Ordered unique property keys from SERVER_PROPERTIES_KEYS."""
    path = root / CONFIG_FILE
    raw = path.read_text(errors="replace")
    code = production_code(raw)
    match = KEY_TABLE_DECL.search(code)
    if not match:
        raise ValueError(f"SERVER_PROPERTIES_KEYS not found in {CONFIG_FILE}")
    eq = code.find("=", match.end())
    bracket = code.find("[", eq) if eq >= 0 else -1
    if bracket < 0:
        raise ValueError(f"SERVER_PROPERTIES_KEYS has no table in {CONFIG_FILE}")
    end = _balanced_end(code, bracket, "[", "]")
    keys = []
    for key, _env in KEY_TUPLE.findall(raw[bracket:end]):
        key = _unescape_key(key)
        if key not in keys:
            keys.append(key)
    return keys


def typed_keys(root):
    """Ordered unique property keys the typed ServerProperties surface names."""
    path = root / PROPERTIES_FILE
    raw = path.read_text(errors="replace")
    code = production_code(raw)
    keys = []
    for regex in (DOC_ACCESS, PROP_ACCESS):
        for match in regex.finditer(code):
            key = rust_string_at(raw, match.end())
            if key is not None and key not in keys:
                keys.append(key)
    return keys


# ---------------------------------------------------------------------------
# Check 2: canonical JSON escaper + no second escaper in server-files
# ---------------------------------------------------------------------------

def json_module_canonical(root):
    """server-files/src/json.rs must delegate to megu_core::json or carry
    the local canonical `quote` escaper — never neither."""
    path = root / JSON_FILE
    raw = path.read_text(errors="replace")
    if "megu_core::json" in raw:
        return True, "delegates to megu_core::json"
    code = production_code(raw)
    for name, start, _body_start, end in fn_ranges(code):
        if name == "quote" and emits_json_escapes(raw[start:end]):
            return True, "local canonical quote() escaper"
    return False, "neither megu_core::json delegation nor a canonical quote()"


def escaper_drift(root):
    """[(rel, fn)] — non-canonical JSON string escapers inside server-files."""
    drift = []
    src = root / SERVER_FILES_SRC
    if not src.is_dir():
        return drift
    for path in sorted(src.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        if rel == JSON_FILE:
            continue  # json.rs IS the canonical site
        if path.name.endswith("_tests.rs") or path.name == "test_support.rs":
            continue
        raw = path.read_text(errors="replace")
        code = production_code(raw)
        for name, start, _body_start, end in fn_ranges(code):
            if emits_json_escapes(raw[start:end]):
                drift.append((rel, name, code.count("\n", 0, start) + 1))
    return drift


# ---------------------------------------------------------------------------

def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    options = parser.parse_args(argv)
    root = options.root.resolve()

    failures = 0

    # --- key-table parity, ratcheted against the documented delta ---------
    try:
        root_keys = config_keys(root)
        sf_keys = typed_keys(root)
    except (OSError, ValueError) as error:
        print(f"server-files parity: FAIL {error}")
        return 1
    root_set, sf_set = set(root_keys), set(sf_keys)
    intersection = root_set & sf_set
    root_only = root_set - sf_set
    sf_only = sf_set - root_set
    print(
        f"server-files parity: keys — {len(intersection)} shared, "
        f"{len(root_only)} root-only, {len(sf_only)} server-files-only"
    )
    print(f"  shared:        {', '.join(sorted(intersection))}")
    print(f"  root-only:     {', '.join(sorted(root_only))}")
    print(f"  sf-only:       {', '.join(sorted(sf_only))}")
    if root_only != EXPECTED_ROOT_ONLY:
        failures += 1
        new = sorted(root_only - EXPECTED_ROOT_ONLY)
        gone = sorted(EXPECTED_ROOT_ONLY - root_only)
        print(
            f"server-files parity: FAIL root-only key set drifted "
            f"(new: {new or 'none'}, resolved: {gone or 'none'}) — "
            f"update EXPECTED_ROOT_ONLY only with a documented reason"
        )
    if sf_only != EXPECTED_SERVER_FILES_ONLY:
        failures += 1
        new = sorted(sf_only - EXPECTED_SERVER_FILES_ONLY)
        gone = sorted(EXPECTED_SERVER_FILES_ONLY - sf_only)
        print(
            f"server-files parity: FAIL server-files-only key set drifted "
            f"(new: {new or 'none'}, resolved: {gone or 'none'}) — "
            f"update EXPECTED_SERVER_FILES_ONLY only with a documented reason"
        )

    # --- canonical JSON escaper --------------------------------------------
    try:
        canonical, how = json_module_canonical(root)
    except OSError as error:
        print(f"server-files parity: FAIL cannot read {JSON_FILE}: {error}")
        return 1
    if not canonical:
        failures += 1
        print(
            f"server-files parity: FAIL {JSON_FILE} {how} — the crate's JSON "
            f"string escaper must live in (or delegate to) the canonical module"
        )
    else:
        print(f"server-files parity: PASS json.rs {how}")

    # --- no second JSON escaper inside server-files -------------------------
    drift = escaper_drift(root)
    if drift:
        failures += 1
        for rel, name, line in drift:
            print(
                f"server-files parity: FAIL {rel}:{line} fn {name} emits JSON "
                f"string escapes — route it through crate::json::quote"
            )
    else:
        print("server-files parity: PASS no non-canonical JSON escaper in server-files")

    if failures:
        print(f"server-files parity: FAIL ({failures} check(s) drifted)")
        return 1
    print("server-files parity: PASS (key tables and JSON escaper in parity)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
