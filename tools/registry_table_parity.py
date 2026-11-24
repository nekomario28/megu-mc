#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""V767 registry-table parity audit — zero-JAR per-push CI guard (W14).

Two hand-maintained name tables must stay index-identical to the protocol
tables they mirror, and nothing else checks that today:

* ``ITEM_NAMES`` in ``src/menus_and_items.rs`` (bare names, index = item
  protocol id) vs ``V767_ITEMS`` in ``proto/src/data_v767.rs`` (canonical
  ``minecraft:``-prefixed names, same indexing). Parity means equal length
  AND ``"minecraft:" + bare == full`` at every index.
* ``COMP_WIRE_NAMES`` in ``src/menus_and_items.rs`` vs ``V767_COMPONENTS``
  in ``proto/src/data_v767.rs`` — both already carry the ``minecraft:``
  prefix, so parity is exact name equality at every index.

A one-entry insert, delete or rename in either table desynchronizes every
id >= that index on the wire; this gate fails with the exact index and both
names instead of letting the drift ship. Everything is parsed from Rust
source — no Mojang JAR, no ``assets/`` — so it runs in the fast per-push
job. Test code cannot satisfy the check: comments, literals and
``#[cfg(test)]`` items are masked before the table declarations are located
(same technique as ``tools/opt_wiring_audit.py``), and the string entries
themselves are read back out of the raw source inside the located span.

Usage:
    tools/registry_table_parity.py            # human PASS/FAIL summary
    tools/registry_table_parity.py --root DIR # audit a synthetic tree
"""

import argparse
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

ITEMS_FILE = "src/menus_and_items.rs"
PROTO_FILE = "proto/src/data_v767.rs"

# (left table, right table, comparison). `prefix` means the left entry is a
# bare name and must equal the right entry minus the "minecraft:" prefix.
TABLES = [
    (ITEMS_FILE, "ITEM_NAMES", PROTO_FILE, "V767_ITEMS", "prefix"),
    (ITEMS_FILE, "COMP_WIRE_NAMES", PROTO_FILE, "V767_COMPONENTS", "exact"),
]

MINECRAFT_PREFIX = "minecraft:"
MAX_REPORTED = 10

# ---------------------------------------------------------------------------
# Rust source masking — mirrors tools/opt_wiring_audit.py so comments,
# literals and cfg(test) items can never locate a phantom table.
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


# ---------------------------------------------------------------------------
# Table extraction
# ---------------------------------------------------------------------------

TABLE_DECL = re.compile(
    r"\b(?:static|const)\s+(?:pub(?:\s*\([^)]*\))?\s+)?NAME\b"
)

STRING_ENTRY = re.compile(r'"((?:[^"\\\n]|\\.)*)"')


def _unescape(entry):
    """Undo the Rust string escapes registry names actually use."""
    return (
        entry.replace('\\"', '"')
        .replace("\\\\", "\\")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r")
    )


def extract_table(path, table_name):
    """The string entries of `static|const TABLE_NAME: [&str; N] = [ ... ]`.

    The declaration is located in masked production code (so a same-named
    table inside `#[cfg(test)]` cannot be picked), then the entries are read
    as string literals from the raw source inside the located `[...]` span.
    """
    raw = path.read_text(errors="replace")
    code = production_code(raw)
    decl = TABLE_DECL.pattern.replace("NAME", re.escape(table_name))
    matches = list(re.finditer(decl, code))
    if not matches:
        raise ValueError(f"{table_name}: declaration not found in {path}")
    if len(matches) > 1:
        raise ValueError(
            f"{table_name}: {len(matches)} declarations in {path} — ambiguous"
        )
    # The type ascription `[&str; N]` precedes the `=`; the entry list is the
    # first `[` after the assignment.
    eq = code.find("=", matches[0].end())
    if eq < 0:
        raise ValueError(f"{table_name}: no initializer in {path}")
    bracket = code.find("[", eq)
    if bracket < 0:
        raise ValueError(f"{table_name}: initializer is not an array in {path}")
    end = _balanced_end(code, bracket, "[", "]")
    return [_unescape(m.group(1)) for m in STRING_ENTRY.finditer(raw[bracket:end])]


def compare(left_file, left_name, right_file, right_name, mode, root):
    """(ok, problems) for one table pair."""
    left = extract_table(root / left_file, left_name)
    right = extract_table(root / right_file, right_name)
    problems = []
    if len(left) != len(right):
        problems.append(
            f"length drift: {left_name} has {len(left)} entries, "
            f"{right_name} has {len(right)}"
        )
    for index in range(min(len(left), len(right))):
        a, b = left[index], right[index]
        ok = (MINECRAFT_PREFIX + a == b) if mode == "prefix" else (a == b)
        if not ok:
            problems.append(
                f"index {index}: {left_name}={a!r} vs {right_name}={b!r}"
            )
    return problems, len(left), len(right)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    options = parser.parse_args(argv)
    root = options.root.resolve()

    failures = 0
    for left_file, left_name, right_file, right_name, mode in TABLES:
        try:
            problems, left_len, right_len = compare(
                left_file, left_name, right_file, right_name, mode, root
            )
        except ValueError as error:
            print(f"registry-table parity: FAIL {error}")
            failures += 1
            continue
        label = f"{left_name}({left_file}) vs {right_name}({right_file})"
        if problems:
            failures += 1
            print(f"registry-table parity: FAIL {label}")
            for problem in problems[:MAX_REPORTED]:
                print(f"  {problem}")
            if len(problems) > MAX_REPORTED:
                print(f"  … and {len(problems) - MAX_REPORTED} more mismatch(es)")
        else:
            print(
                f"registry-table parity: PASS {label} "
                f"({left_len} entries, index-by-index identical)"
            )
    if failures:
        print(f"registry-table parity: FAIL ({failures} table pair(s) drifted)")
        return 1
    print("registry-table parity: PASS (all table pairs in parity)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
