#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Permanent trademark-hygiene gate for Megu's first-party UI surface.

This audit is intentionally narrower than a repository-wide trademark search.
Nominative references to external mods are legitimate in compatibility,
catalog, and search-alias tables.  What must never regress is Megu presenting
those marks as its own built-in feature names, module names, or native public
API names.  The scanner therefore checks:

* source filenames in the owned UI surface;
* identifiers in Rust `pub` declarations and public enum variants;
* string literals in production code (inline `#[cfg(test)]` modules excluded).

Documented allowlist:
* `client-ui/src/mod_manager.rs` `slug:`/`name:`/installed-mod-ID rows are
  nominative catalog and compatibility identifiers for third-party mods;
* lines carrying `trademark-hygiene:allow` are explicit search-alias or
  compatibility exceptions and must state the reason; for a rustfmt-split
  declarative entry the reasoned marker may sit on that entry's closing line;
* the literal `optifine/cem` resource path is a compatibility lookup key.

Run: python3 tools/trademark_hygiene_audit.py [--strict] [--json] [--root DIR]
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

SCAN_FILES = (
    "client-gpu/src/window.rs",
    "client-gpu/src/live_session.rs",
    "client-gpu/src/recipe_ingest.rs",
    "core/src/sys_metrics.rs",
)

FORBIDDEN = {
    "JEI": re.compile(r"\bjei\b", re.IGNORECASE),
    "EMI": re.compile(r"\bemi\b", re.IGNORECASE),
    "JER": re.compile(r"\bjer\b", re.IGNORECASE),
    "Ponder": re.compile(r"\bponder\b", re.IGNORECASE),
    "Mouse Tweaks": re.compile(r"\bmouse[-_ ]?tweaks\b|\bmousetweaks\b", re.IGNORECASE),
    "MiniHUD": re.compile(r"\bmini[-_ ]?hud\b|\bminihud\b", re.IGNORECASE),
    "ModMenu": re.compile(r"\bmod[-_ ]?menu\b|\bmodmenu\b", re.IGNORECASE),
    "OptiFine": re.compile(r"\boptifine\b", re.IGNORECASE),
    "Sodium": re.compile(r"\bsodium\b", re.IGNORECASE),
    "Iris": re.compile(r"\biris\b", re.IGNORECASE),
    "Lithium": re.compile(r"\blithium\b", re.IGNORECASE),
    "Sable": re.compile(r"\bsable\b", re.IGNORECASE),
    "Aeronautics": re.compile(r"\baeronautics\b", re.IGNORECASE),
    "Valkyrien": re.compile(r"\bvalkyrien\b", re.IGNORECASE),
    "Create Goggles": re.compile(r"\bcreate[-_ ]?goggles\b", re.IGNORECASE),
}

PUBLIC_DECL = re.compile(
    r"\bpub(?:\([^)]*\))?\s+(?:async\s+)?(?:fn|struct|enum|type|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)"
)
STRING_LITERAL = re.compile(r'"(?:\\.|[^"\\])*"')
ENUM_VARIANT = re.compile(r"^\s*([A-Z][A-Za-z0-9_]*)\s*(?:[,=({]|$)")


def production_lines(text: str) -> list[str]:
    """Return lines before an inline `#[cfg(test)] mod tests` module.

    External integration tests are outside this audit's source scope.  Inline
    unit tests often pin compatibility-parser behavior using real third-party
    IDs; those parser inputs are not built-in UI/public API names either.
    """
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if "#[cfg(test)]" in line:
            tail = "\n".join(lines[index:index + 4])
            if re.search(r"\bmod\s+(tests|test)\b", tail):
                return lines[:index]
    return lines


def strip_line_comment(line: str) -> str:
    """Strip `//` comments while respecting simple Rust string literals."""
    in_string = False
    escaped = False
    for index in range(len(line) - 1):
        ch = line[index]
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch == "/" and line[index + 1] == "/":
            return line[:index]
    return line


ALLOW_MARK = "trademark-hygiene:allow"
CATALOG_ENTRY_STARTS = ("CuratedModrinthQolMod {", "RecommendedModEntry {")


def explicit_allow(line: str) -> bool:
    """True only when the marker carries a human-readable reason."""
    index = line.find(ALLOW_MARK)
    if index < 0:
        return False
    reason = line[index + len(ALLOW_MARK):].strip(" \t:-—–")
    return len(reason) >= 8


def inside_mod_catalog_entry(lines: list[str], index: int) -> bool:
    """Whether `index` is a field row inside the two nominative catalogs.

    The exemption intentionally stops at the catalog row boundary: arbitrary
    `name:`/`slug:` lines elsewhere in `mod_manager.rs` are UI strings and must
    be scanned instead of inheriting the catalog's nominative-use exception.
    """
    for back in range(index, max(-1, index - 16), -1):
        text = lines[back]
        if any(start in text for start in CATALOG_ENTRY_STARTS):
            return True
        if back != index and text.strip() in ("},", "};"):
            return False
    return False


def scoped_entry_allow(lines: list[str], index: int) -> bool:
    """Whether a rustfmt-split declarative entry closes with a reasoned marker.

    Search-alias arrays often span several lines. Requiring the marker on every
    literal would drown the actual reason in noise, while a file-scoped marker
    would be far too broad. The supported form is therefore exact and local:
    the enclosing entry must close within a few rows on a line carrying
    `trademark-hygiene:allow <reason>`.
    """
    for lookahead in range(index, min(len(lines), index + 20)):
        stripped = lines[lookahead].strip()
        if explicit_allow(lines[lookahead]) and stripped.startswith(("),", ");")):
            return True
        if stripped.startswith(("),", ");")):
            return False
    return False


def allowed(path: str, line: str, lines: list[str], index: int) -> bool:
    if explicit_allow(line) or scoped_entry_allow(lines, index):
        return True
    if path == "client-ui/src/mod_manager.rs":
        stripped = line.strip()
        # Nominative catalog entries only: upstream project slugs/display
        # names inside `CURATED_MODRINTH_QOL_MODS` and
        # `RECOMMENDED_COMMUNITY_MODS`. A synthetic label elsewhere in the
        # file does not match this narrow structural exemption.
        if re.match(r"^(?:slug|name):\s*\"", stripped) and inside_mod_catalog_entry(lines, index):
            return True
        # Installed-loader ID compatibility tuple: modids often omit the
        # hyphen used by the canonical project slug. This exact tuple is a
        # lookup key, not a built-in feature label.
        if stripped.startswith('("mouse-tweaks", "mousetweaks")'):
            return True
    # Resource-pack compatibility lookup path, not a feature label.
    if "optifine/cem" in line:
        return True
    return False


def forbidden_terms(text: str) -> list[str]:
    return sorted(name for name, pattern in FORBIDDEN.items() if pattern.search(text))


COMPACT_FORBIDDEN = {
    "JEI": "jei",
    "EMI": "emi",
    "JER": "jer",
    "Ponder": "ponder",
    "Mouse Tweaks": "mousetweaks",
    "MiniHUD": "minihud",
    "ModMenu": "modmenu",
    "OptiFine": "optifine",
    "Sodium": "sodium",
    "Iris": "iris",
    "Lithium": "lithium",
    "Sable": "sable",
    "Aeronautics": "aeronautics",
    "Valkyrien": "valkyrien",
    "Create Goggles": "creategoggles",
}

IDENT_TOKEN = re.compile(r"[A-Z]+(?=[A-Z][a-z0-9]|\d|_|$)|[A-Z][a-z0-9]*|[a-z0-9]+")


def identifier_tokens(identifier: str) -> list[str]:
    tokens: list[str] = []
    for part in identifier.split("_"):
        tokens.extend(match.group(0).lower() for match in IDENT_TOKEN.finditer(part))
    return tokens


def forbidden_identifier_terms(identifier: str) -> list[str]:
    """Match whole snake_case/CamelCase identifier tokens, including token pairs.

    This catches `JeiEmiOverlayState`, `mini_hud`, `MouseTweaks`, and
    `OptiFine` without false-positive substring hits inside words such as
    `emission`.
    """
    tokens = identifier_tokens(identifier)
    found = []
    for name, compact in COMPACT_FORBIDDEN.items():
        for start in range(len(tokens)):
            joined = ""
            for end in range(start, len(tokens)):
                joined += tokens[end]
                if joined == compact:
                    found.append(name)
                    break
                if len(joined) >= len(compact):
                    break
    return sorted(set(found))


def scan_file(root: Path, rel: str) -> list[dict[str, object]]:
    path = root / rel
    findings: list[dict[str, object]] = []
    stem = path.stem
    for term in forbidden_identifier_terms(stem):
        findings.append({
            "path": rel,
            "line": 0,
            "kind": "module-filename",
            "terms": [term],
            "text": path.name,
        })

    lines = production_lines(path.read_text(encoding="utf-8"))
    enum_depth = 0
    enum_open = False
    in_public_enum = False
    pending_public_enum = False
    for lineno, original in enumerate(lines, 1):
        code = strip_line_comment(original)
        if not code.strip():
            continue

        decl = PUBLIC_DECL.search(code)
        # An explicit exception may sit inside a rustfmt-split multi-line
        # declaration (for example on the first parameter line). It still
        # has to be a reasoned `trademark-hygiene:allow` marker; unmarked
        # compatibility names remain findings.
        decl_explicitly_allowed = decl is not None and any(
            explicit_allow(lines[lookahead])
            for lookahead in range(
                max(0, lineno - 4), min(len(lines), lineno + 3)
            )
        )
        if decl and not (allowed(rel, original, lines, lineno - 1) or decl_explicitly_allowed):
            terms = forbidden_identifier_terms(decl.group(1))
            if terms:
                findings.append({
                    "path": rel,
                    "line": lineno,
                    "kind": "public-api-name",
                    "terms": terms,
                    "text": original.strip(),
                })
            if re.search(r"\benum\s+" + re.escape(decl.group(1)), code):
                pending_public_enum = True

        if in_public_enum:
            variant = ENUM_VARIANT.match(code)
            if variant and enum_open and not allowed(rel, original, lines, lineno - 1):
                terms = forbidden_identifier_terms(variant.group(1))
                if terms:
                    findings.append({
                        "path": rel,
                        "line": lineno,
                        "kind": "public-enum-variant",
                        "terms": terms,
                        "text": original.strip(),
                    })

        if pending_public_enum or in_public_enum:
            enum_depth += code.count("{") - code.count("}")
            if enum_depth > 0:
                enum_open = True
                in_public_enum = True
                pending_public_enum = False
            elif enum_open:
                in_public_enum = False
                enum_open = False
                enum_depth = 0
            elif enum_depth < 0:
                pending_public_enum = False
                enum_depth = 0

        if allowed(rel, original, lines, lineno - 1):
            continue
        for literal in STRING_LITERAL.findall(code):
            terms = forbidden_terms(literal)
            if terms:
                findings.append({
                    "path": rel,
                    "line": lineno,
                    "kind": "string-literal",
                    "terms": terms,
                    "text": original.strip(),
                })
                break
    return findings


def scan(root: Path) -> list[dict[str, object]]:
    findings: list[dict[str, object]] = []
    # client-ui and proto were the original surface; client-gpu, client-kernel,
    # state-store, mod-offload and wasm-mod-runtime measure clean and are now
    # held to the same standard. client-port, client-app, core and
    # server-files carry reviewed nominative compatibility uses (Iris/OptiFine
    # shaderpack interop, i18n aliases, optifine_compat) that need explicit
    # `trademark-hygiene:allow` markers before they can join; the adapter
    # crates legitimately name the mods they adapt.
    for subdir in ("client-ui/src", "proto/src", "client-gpu/src",
                   "client-kernel/src", "state-store/src", "mod-offload/src",
                   "wasm-mod-runtime/src"):
        src_dir = root / subdir
        if src_dir.is_dir():
            for path in sorted(src_dir.rglob("*.rs")):
                findings.extend(scan_file(root, path.relative_to(root).as_posix()))
    for rel in SCAN_FILES:
        if (root / rel).is_file():
            findings.extend(scan_file(root, rel))
    return findings


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--strict", action="store_true", help="exit nonzero when findings exist")
    parser.add_argument("--json", action="store_true", help="emit machine-readable findings")
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    args = parser.parse_args()

    root = args.root.resolve()
    findings = scan(root)
    payload = {
        "ok": not findings,
        "findings": findings,
        "scanned_roots": ["client-ui/src", "proto/src", "client-gpu/src",
                          "client-kernel/src", "state-store/src",
                          "mod-offload/src", "wasm-mod-runtime/src",
                          *SCAN_FILES],
    }
    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        if findings:
            print(f"trademark hygiene audit: {len(findings)} finding(s)")
            for finding in findings:
                terms = ",".join(finding["terms"])
                print(
                    f"{finding['path']}:{finding['line']}: "
                    f"{finding['kind']} [{terms}] {finding['text']}"
                )
        else:
            print("trademark hygiene audit: clean")
    return 1 if args.strict and findings else 0


if __name__ == "__main__":
    sys.exit(main())
