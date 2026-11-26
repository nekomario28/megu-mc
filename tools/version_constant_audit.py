#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Version-constant drift audit — permanent per-push CI guard (W7).

`core/src/version.rs` (re-exported as `megu_core::version`) is the canonical
registry for the protocol and `DataVersion` integers of the two vanilla
targets megu simulates (1.21.1 = protocol 767 / data 3955, 26.3 = protocol
777 / data 5023). A `const PROTOCOL_VERSION: u32 = 767;` anywhere else is a
second source of truth: it cannot follow a target-set change, and two sites
that disagree fail only at runtime, if at all.

This gate scans every non-test `.rs` file in the workspace for scalar
`const`/`static` items whose name mentions a version axis (`VERSION` or
`PROTOCOL`, case-insensitive) and whose literal value is one of the pinned
numbers {767, 777, 3955, 5023}. Matches under `core/src/version.rs` itself
are the canonical source and always pass. Everything else must be listed in
``WAIVERS`` — an explicit ``(path, symbol, value)`` triple with the reason
the literal is still pinned — so the current tree passes cleanly while ANY
new hardcoded protocol/data-version constant fails CI immediately, and a
waived constant whose *value* changes fails too (the waiver pins the number,
not just the name).

Test code cannot satisfy or trip the check: comments, string/char literals
and ``#[cfg(test)]`` items are masked before the scan (same technique as
``tools/opt_wiring_audit.py``), and ``tests/``/``benches/``/``examples/``
directories, ``*_tests.rs`` files, ``test_support.rs``, frozen ``evidence/``
snapshots and ``third_party/`` vendored code are out of scope.

Usage:
    tools/version_constant_audit.py            # human PASS/FAIL summary
    tools/version_constant_audit.py --root DIR # audit a synthetic tree
"""

import argparse
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

# The pinned protocol/DataVersion integers of the target set. A version-axis
# constant carrying one of these outside the canonical registry is drift.
PINNED_VALUES = {767, 777, 3955, 5023}

# Scalar const/static items. The value may carry an integer suffix and `_`
# separators; anything else (`&[…]` tables, expressions, calls) cannot match.
CONST_ITEM = re.compile(
    r"\b(?:const|static)\s+"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?"
    r"([A-Za-z_][A-Za-z0-9_]*)"
    r"\s*(?::\s*[^=;]+)?=\s*(\d[\d_]*)\s*"
    r"(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize)?\s*;"
)

# Names that name a protocol/data version axis. `DATA_VERSION`/`PROTOCOL_VERSION`
# are covered by `VERSION`; bare `PROTOCOL`/`DATA` names are included so a drift
# cannot hide behind a shorter identifier.
VERSION_AXIS = re.compile(r"VERSION|PROTOCOL|DATA_VERSION", re.IGNORECASE)

CANONICAL = "core/src/version.rs"

# Known legacy sites, pinned to (path, symbol, value). Each entry is a
# ratchet: the literal is tolerated ONLY at this exact site with this exact
# value. Removing the constant makes the entry stale (reported, not failed);
# adding a new one anywhere fails. Reasons name why the literal is still
# pinned and what would retire it.
WAIVERS = {
    ("src/save_v2.rs", "SAVE_DATA_VERSION_RUNNING", 3955):
        "native save-schema header literal written into VER1 footers; "
        "pending migration onto megu_core::version",
    ("src/save_v2.rs", "SAVE_PROTOCOL_VERSION_RUNNING", 767):
        "native save-schema header literal written into VER1 footers; "
        "pending migration onto megu_core::version",
    ("create-adapter/src/aeronautics.rs", "AERO_DATA_VERSION_1_21_1", 3955):
        "AERO2 envelope DataVersion stamp for the 1.21.1 target; "
        "pending migration onto megu_core::version",
    ("create-adapter/src/aeronautics.rs", "AERO_DATA_VERSION_26_3", 5023):
        "AERO2 envelope DataVersion stamp for the 26.3 target; "
        "pending migration onto megu_core::version",
}

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


# ---------------------------------------------------------------------------
# Scan
# ---------------------------------------------------------------------------

# Directories that are never workspace production code: build output, Rust
# integration tests/benches/examples, frozen evidence snapshots and vendored
# third-party trees.
EXCLUDED_PARTS = {"target", "tests", "benches", "examples", "evidence", "third_party"}


def rust_files(root):
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root)
        if EXCLUDED_PARTS & set(rel.parts[:-1]):
            continue
        if path.name.endswith("_tests.rs") or path.name == "test_support.rs":
            continue
        yield rel, path


def scan(root):
    """Return (findings, waived, stale_waivers).

    findings: [(rel, line, symbol, value)] unwaived drift sites.
    waived: [(rel, line, symbol, value, reason)] pinned legacy sites.
    stale_waivers: waiver keys whose site no longer exists.
    """
    findings = []
    waived = []
    seen_waivers = set()
    for rel, path in rust_files(root):
        rel_posix = rel.as_posix()
        if rel_posix == CANONICAL:
            continue
        text = path.read_text(errors="replace")
        # This raw-text prefilter is only a necessary-condition check:
        # masking can remove tokens but cannot create a scalar const match.
        # Comments/strings with candidate syntax still go through the
        # unchanged production-code matcher before anything is reported.
        if not CONST_ITEM.search(text):
            continue
        code = production_code(text)
        for match in CONST_ITEM.finditer(code):
            symbol, raw_value = match.group(1), match.group(2)
            if not VERSION_AXIS.search(symbol):
                continue
            value = int(raw_value.replace("_", ""))
            if value not in PINNED_VALUES:
                continue
            line = code.count("\n", 0, match.start()) + 1
            key = (rel_posix, symbol, value)
            if key in WAIVERS:
                seen_waivers.add(key)
                waived.append((rel_posix, line, symbol, value, WAIVERS[key]))
            else:
                findings.append((rel_posix, line, symbol, value))
    stale = sorted(set(WAIVERS) - seen_waivers)
    return findings, waived, stale


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    options = parser.parse_args(argv)
    root = options.root.resolve()

    findings, waived, stale = scan(root)
    for rel, line, symbol, value, reason in waived:
        print(f"version-constant audit: WAIVED {rel}:{line} {symbol} = {value} ({reason})")
    for rel, line, symbol, value in findings:
        print(
            f"version-constant audit: FAIL {rel}:{line} {symbol} = {value} — "
            f"hardcoded protocol/data-version constant outside {CANONICAL}; "
            f"route it through megu_core::version or add a justified WAIVERS entry"
        )
    for rel, symbol, value in stale:
        print(
            f"version-constant audit: note — stale waiver {rel} {symbol} = {value} "
            f"no longer found; remove it from WAIVERS"
        )
    if findings:
        print(f"version-constant audit: FAIL ({len(findings)} unwaived site(s))")
        return 1
    print(
        f"version-constant audit: PASS "
        f"({len(waived)} waived legacy site(s), {len(stale)} stale waiver(s))"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
