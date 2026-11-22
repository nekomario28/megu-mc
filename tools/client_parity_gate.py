#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Single-source-of-truth parity gate for the Vanilla 1.21.1 client manifest.

Reads ``specs/client-1211-parity-manifest.json`` and classifies every catalogued
client feature against real Rust source:

* ``WIRED_LIVE``   - ``kernel_symbol`` is declared in ``kernel_file`` AND is
  referenced (beyond imports/re-exports) by at least one non-test file named in
  the entry's ``live_consumers``.
* ``KERNEL_ONLY``  - the symbol is declared in ``kernel_file`` but no listed
  live consumer references it yet.
* ``MISSING``      - the symbol (or its ``kernel_file``) does not exist yet.

Rust comments, string/char literals and ``#[cfg(test)]`` items are masked before
any matching (same technique as ``tools/wiring_audit.py``), so a mention in a
comment, a doc string, a test module or a ``use`` import can never count as
implemented or wired. ``kernel_symbol`` may be a plain identifier
(``raycast_blocks``) or a ``Type::member`` path (``PlayEvent::ChunkData``,
``VanillaBerKind::Chest``); for the path form the type must be declared and the
member identifier must occur in the same file, and a consumer must mention both
the type and the member identifiers.

Usage:
    tools/client_parity_gate.py                 # scoreboard table
    tools/client_parity_gate.py --json          # machine-readable report
    tools/client_parity_gate.py --strict-kernels# fail on unplanned MISSING
    tools/client_parity_gate.py --strict-live   # fail unless everything is WIRED_LIVE
    tools/client_parity_gate.py --root DIR --manifest PATH
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = REPO_ROOT / "specs/client-1211-parity-manifest.json"

STATUS_WIRED = "WIRED_LIVE"
STATUS_KERNEL = "KERNEL_ONLY"
STATUS_MISSING = "MISSING"
STATUSES = (STATUS_WIRED, STATUS_KERNEL, STATUS_MISSING)

# ---------------------------------------------------------------------------
# Rust source masking — mirrors tools/wiring_audit.py::production_code so
# comments, literals and cfg(test) items can never satisfy a check.
# ---------------------------------------------------------------------------

RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
CHARACTER = re.compile(r"'(?:[^'\\\n]|\\(?:.|u\{[A-Fa-f0-9_]+\}))'")
CFG_TEST = re.compile(r"\bcfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)")
IDENT = re.compile(r"\b[A-Za-z_][A-Za-z_0-9]*\b")

# Declaration spellings accepted for a plain kernel_symbol. `pub` and the
# usual qualifiers are optional so private helpers (e.g. a `fn` only reachable
# inside its own module) still count as "the kernel exists".
_DECL_KEYWORDS = r"(?:fn|struct|enum|union|trait|type|const|static)"
_DECL_PREFIX = (
    r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?"
    r"(?:(?:async|unsafe|const|default)\s+|extern\s+\"[A-Za-z]+\"\s+)*"
)
_TYPE_DECL = re.compile(
    r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?"
    r"(?:struct|enum|union|trait|type)\s+([A-Za-z_]\w*)"
)


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


def _skip_space(code, pos):
    while pos < len(code) and code[pos].isspace():
        pos += 1
    return pos


def strip_surface_declarations(code):
    """Module declarations, imports and re-exports are not usages."""
    code = re.sub(r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+\w+\s*;", " ", code)
    code = re.sub(r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?use\s+[^;]*;", " ", code)
    return code


# ---------------------------------------------------------------------------
# Symbol matching
# ---------------------------------------------------------------------------

def _decl_regex(name):
    return re.compile(_DECL_PREFIX + _DECL_KEYWORDS + r"\s+" + re.escape(name) + r"\b")


def _split_symbol(symbol):
    """Return (type_name, member_name) for `Type::member`, else (None, symbol)."""
    if "::" in symbol:
        type_name, _, member = symbol.rpartition("::")
        return type_name, member
    return None, symbol


def kernel_symbol_exists(code, symbol):
    """True when `symbol` is declared in the masked kernel source."""
    type_name, member = _split_symbol(symbol)
    if type_name is not None:
        # `Type::member`: the type must be declared and the member identifier
        # must occur (enum variant, associated fn/const, impl method).
        declared_types = set(_TYPE_DECL.findall(code))
        if type_name not in declared_types:
            return False
        return re.search(r"\b" + re.escape(member) + r"\b", code) is not None
    return _decl_regex(member).search(code) is not None


def consumer_references(code, symbol):
    """True when masked consumer code references `symbol` beyond imports."""
    type_name, member = _split_symbol(symbol)
    if re.search(r"\b" + re.escape(member) + r"\b", code) is None:
        return False
    if type_name is None:
        return True
    return re.search(r"\b" + re.escape(type_name) + r"\b", code) is not None


def is_test_path(rel):
    parts = Path(rel).parts
    if "tests" in parts[:-1]:
        return True
    stem = Path(rel).stem
    return stem == "tests" or stem.endswith("_test") or stem.endswith("_tests")


# ---------------------------------------------------------------------------
# Manifest schema
# ---------------------------------------------------------------------------

def validate_manifest(doc):
    """Return a list of schema errors; empty means the manifest is usable."""
    errors = []
    if not isinstance(doc, dict):
        return ["manifest root is not a JSON object"]
    categories = doc.get("categories")
    if not isinstance(categories, list) or not categories:
        errors.append("manifest 'categories' must be a non-empty list")
        categories = []
    features = doc.get("features")
    if not isinstance(features, list) or not features:
        errors.append("manifest 'features' must be a non-empty list")
        return errors
    known = set(categories)
    seen_ids = set()
    for idx, raw in enumerate(features):
        label = f"feature #{idx}"
        if not isinstance(raw, dict):
            errors.append(f"{label} is not an object")
            continue
        fid = raw.get("id")
        if not isinstance(fid, str) or not fid:
            errors.append(f"{label} missing string 'id'")
        elif fid in seen_ids:
            errors.append(f"duplicate feature id {fid!r}")
        else:
            seen_ids.add(fid)
            label = f"feature {fid!r}"
        category = raw.get("category")
        if category not in known:
            errors.append(f"{label} has unknown category {category!r}")
        for field in ("description", "kernel_file", "kernel_symbol"):
            if not isinstance(raw.get(field), str) or not raw[field]:
                errors.append(f"{label} missing non-empty string {field!r}")
        consumers = raw.get("live_consumers")
        if (not isinstance(consumers, list) or not consumers
                or not all(isinstance(c, str) and c for c in consumers)):
            errors.append(f"{label} 'live_consumers' must be a non-empty string list")
        status = raw.get("status")
        if status is not None and status != "planned":
            errors.append(f"{label} has unsupported status {status!r}")
    return errors


# ---------------------------------------------------------------------------
# Audit
# ---------------------------------------------------------------------------

def audit(root, manifest_path):
    """Classify every manifest feature; returns (report, schema_errors)."""
    root = Path(root)
    manifest_path = Path(manifest_path)
    try:
        doc = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return None, [f"cannot load manifest {manifest_path}: {exc}"]
    errors = validate_manifest(doc)
    if errors:
        return None, errors

    features = doc["features"]

    # Cache masked production code per repo-relative file.
    cache = {}

    def masked(rel):
        if rel not in cache:
            path = root / rel
            if not path.is_file() or is_test_path(rel):
                cache[rel] = None
            else:
                try:
                    cache[rel] = strip_surface_declarations(
                        production_code(path.read_text(encoding="utf-8")))
                except (OSError, UnicodeDecodeError):
                    cache[rel] = None
        return cache[rel]

    results = []
    category_order = list(doc["categories"])
    cat_index = {name: idx for idx, name in enumerate(category_order)}
    features_sorted = sorted(
        features, key=lambda f: (cat_index.get(f["category"], 99), f["id"]))

    for feature in features_sorted:
        kernel_rel = feature["kernel_file"]
        kernel_code = masked(kernel_rel)
        kernel_exists = kernel_code is not None and kernel_symbol_exists(
            kernel_code, feature["kernel_symbol"])

        live_refs = []
        missing_consumers = []
        for consumer in feature["live_consumers"]:
            if consumer == kernel_rel or is_test_path(consumer):
                continue
            code = masked(consumer)
            if code is None:
                missing_consumers.append(consumer)
                continue
            if consumer_references(code, feature["kernel_symbol"]):
                live_refs.append(consumer)

        if not kernel_exists:
            status = STATUS_MISSING
        elif live_refs:
            status = STATUS_WIRED
        else:
            status = STATUS_KERNEL

        results.append({
            "id": feature["id"],
            "category": feature["category"],
            "status": status,
            "kernel_file": kernel_rel,
            "kernel_symbol": feature["kernel_symbol"],
            "kernel_exists": kernel_exists,
            "live_refs": live_refs,
            "missing_consumers": missing_consumers,
            "planned": feature.get("status") == "planned",
        })

    counts = {status: 0 for status in STATUSES}
    for result in results:
        counts[result["status"]] += 1

    categories = []
    for name in category_order:
        rows = [r for r in results if r["category"] == name]
        categories.append({
            "category": name,
            "wired_live": sum(1 for r in rows if r["status"] == STATUS_WIRED),
            "kernel_only": sum(1 for r in rows if r["status"] == STATUS_KERNEL),
            "missing": sum(1 for r in rows if r["status"] == STATUS_MISSING),
            "total": len(rows),
        })

    unplanned_missing = [
        r["id"] for r in results
        if r["status"] == STATUS_MISSING and not r["planned"]
    ]
    report = {
        "root": str(root),
        "manifest": str(manifest_path),
        "target": doc.get("target", {}),
        "summary": {
            "features_total": len(results),
            "wired_live": counts[STATUS_WIRED],
            "kernel_only": counts[STATUS_KERNEL],
            "missing": counts[STATUS_MISSING],
            "planned_missing": sum(
                1 for r in results
                if r["status"] == STATUS_MISSING and r["planned"]),
            "unplanned_missing": len(unplanned_missing),
        },
        "categories": categories,
        "features": results,
        "unplanned_missing": unplanned_missing,
    }
    return report, []


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

def print_text(report):
    target = report.get("target") or {}
    label = f"minecraft {target.get('minecraft', '?')} / protocol {target.get('protocol', '?')}"
    print(f"Client parity gate — {label}")
    print(f"Manifest: {report['manifest']}")
    header = f"{'Category':<24} {'Wired Live':>10} {'Kernel Only':>11} {'Missing':>8} {'Total':>6}"
    print(header)
    print("-" * len(header))
    for row in report["categories"]:
        print(f"{row['category']:<24} {row['wired_live']:>10} "
              f"{row['kernel_only']:>11} {row['missing']:>8} {row['total']:>6}")
    s = report["summary"]
    print("-" * len(header))
    print(f"{'TOTAL':<24} {s['wired_live']:>10} {s['kernel_only']:>11} "
          f"{s['missing']:>8} {s['features_total']:>6}")
    print()
    pending = [f for f in report["features"] if f["status"] != STATUS_WIRED]
    if pending:
        print(f"Not wired live ({len(pending)}):")
        for f in pending:
            marker = " [planned]" if f["planned"] else ""
            print(f"  {f['status']:<11} {f['id']}{marker} "
                  f"({f['kernel_file']}::{f['kernel_symbol']})")
    if report["unplanned_missing"]:
        print()
        print("UNPLANNED MISSING (no status=planned marker):")
        for fid in report["unplanned_missing"]:
            print(f"  {fid}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT,
                        help="repository root (default: this checkout)")
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST,
                        help="parity manifest JSON")
    parser.add_argument("--json", action="store_true",
                        help="emit the full report as JSON")
    parser.add_argument("--strict-kernels", action="store_true",
                        help="exit 1 when a MISSING feature lacks status=planned")
    parser.add_argument("--strict-live", action="store_true",
                        help="exit 1 unless every feature is WIRED_LIVE")
    args = parser.parse_args(argv)

    root = args.root.resolve()
    manifest = args.manifest
    if not manifest.is_absolute():
        manifest = root / manifest
    report, errors = audit(root, manifest)
    if errors:
        for error in errors:
            print(f"manifest error: {error}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print_text(report)

    summary = report["summary"]
    if args.strict_kernels and summary["unplanned_missing"]:
        return 1
    if args.strict_live and (
            summary["wired_live"] != summary["features_total"]):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
