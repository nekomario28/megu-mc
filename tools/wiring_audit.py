#!/usr/bin/env python3
"""Static, dependency-free audit of Rust production-module wiring.

This checks source-level references, including references to distinctive
public exports. It does not execute Rust, resolve feature flags, or prove that
an observed call site runs at runtime. Contracts pin critical consumers so a
reference from an unrelated helper cannot satisfy an end-to-end boundary.
"""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re


REPO_ROOT = Path(__file__).resolve().parents[1]
CRATES = (
    ".", "core", "proto", "state-store", "server-files", "mod-offload",
    "mod-adapter-core", "create-adapter", "client-port", "client-kernel",
    "client-app", "client-ui", "client-gpu",
)

# Each inner tuple is a set of alternative consumers. Every group must have
# a reference for the contract to pass (GPU explicitly requires both files).
WIRING_CONTRACTS = {
    "client-ui": {
        "mod_menu": (("client-ui/src/state.rs", "client-ui/src/layout.rs"),),
        "recipe_browser": (("client-ui/src/state.rs", "client-ui/src/layout.rs"),),
        "magic_hud": (("client-ui/src/state.rs", "client-ui/src/layout.rs"),),
    },
    "client-port": {
        "vanilla_plus_3d": (("client-kernel/src/mesher.rs", "client-gpu/src/*"),),
        "classic_ores": ((
            "client-kernel/src/mesher.rs", "client-port/src/mesher.rs",
            "client-gpu/src/live_session.rs",
        ),),
        "vanilla_plus_visuals": ((
            "client-kernel/src/mesher.rs", "client-port/src/mesher.rs",
            "client-gpu/src/live_session.rs",
        ),),
        "vanilla_plus_nature": (("client-kernel/src/mesher.rs", "client-gpu/src/*"),),
        "space_sky_bridge": (("client-gpu/src/*",),),
        "builtin_shadow_wgsl": (("client-gpu/src/*",),),
    },
    "client-app": {
        "crash_reporter": (("client-app/src/main.rs",),),
        "spatial_audio": (("client-app/src/session.rs", "client-gpu/src/live_session.rs"),),
        "recovery": (("client-app/src/session.rs", "client-gpu/src/live_session.rs"),),
    },
    "client-gpu": {
        "atmosphere": (("client-gpu/src/renderer.rs",), ("client-gpu/src/live_session.rs",)),
        "subworld_render": (("client-gpu/src/renderer.rs",), ("client-gpu/src/live_session.rs",)),
    },
    "core": {
        "sys_metrics": (("client-ui/src/*", "client-gpu/src/*", "src/*"),),
        "hw_adaptive_engine": (("client-ui/src/*", "client-gpu/src/*", "src/*"),),
    },
    "state-store": {
        "world_snapshot": (("src/anvil_io.rs", "src/main.rs"),),
    },
}

IDENT = re.compile(r"\b[A-Za-z_][A-Za-z_0-9]*\b")
PUBLIC_ITEM = re.compile(
    r"\bpub\s*(?:\([^)]*\)\s*)?"
    r"(?:(?:async|unsafe|const|default)\s+|extern\s+\"[A-Za-z]+\"\s+)*"
    r"(?:fn|struct|enum|union|trait|type|const|static)\s+([A-Za-z_]\w*)"
)
ITEM = re.compile(r"\b(?:fn|struct|enum|union|trait|impl|type|const|static|macro_rules)\b")
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


def _skip_space(code, pos):
    while pos < len(code) and code[pos].isspace():
        pos += 1
    return pos


def remove_surface_declarations(code):
    """Module declarations and public reexports do not constitute usage."""
    code = re.sub(r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+\w+\s*;", " ", code)
    code = re.sub(r"\bpub\s+(?:\([^)]*\)\s*)?use\s+[^;]*;", " ", code)
    return code


def source_paths(root):
    for crate in CRATES:
        source_dir = root / ("src" if crate == "." else f"{crate}/src")
        if not source_dir.is_dir():
            continue
        for path in sorted(source_dir.rglob("*.rs")):
            if "bin" in path.relative_to(source_dir).parts[:-1]:
                continue
            if path.stem == "tests" or path.stem.endswith(("_test", "_tests")):
                continue
            yield path.relative_to(root).as_posix(), crate


def package_names(root):
    aliases = {}
    for crate in CRATES:
        manifest = root / ("Cargo.toml" if crate == "." else f"{crate}/Cargo.toml")
        if manifest.is_file():
            match = re.search(r'^name\s*=\s*"([^"]+)"', manifest.read_text(), re.MULTILINE)
            if match:
                aliases[crate] = match.group(1).replace("-", "_")
    return aliases


def _module_path(path, crate):
    prefix = "src/" if crate == "." else f"{crate}/src/"
    return path[len(prefix):-3].replace("/", "::")


def audit(root):
    raw_sources = {path: (root / path).read_text(encoding="utf-8")
                   for path, _ in source_paths(root)}
    test_only_paths = set()
    for path, raw in raw_sources.items():
        for match in re.finditer(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*mod\s+(\w+)\s*;", raw):
            test_only_paths.add((Path(path).parent / f"{match.group(1)}.rs").as_posix())

    sources = {}
    modules = []
    exports_by_name = Counter()
    included_by = defaultdict(set)
    for path, crate in source_paths(root):
        if path in test_only_paths:
            continue
        code = production_code(raw_sources[path])
        for match in re.finditer(r"\binclude\s*!\s*\(", code):
            literal = re.match(r'\s*"([^"\n]+\.rs)"\s*\)', raw_sources[path][match.end():])
            if literal:
                target = (Path(path).parent / literal.group(1)).as_posix()
                included_by[target].add(path)
        # A lib.rs may implement runtime behavior, so keep its executable code.
        sources[path] = remove_surface_declarations(code)
        if path.endswith(("/lib.rs", "/main.rs")):
            continue
        if not ITEM.search(code):
            continue
        exports = sorted(set(PUBLIC_ITEM.findall(code)))
        modules.append({
            "path": path, "crate": crate, "name": _module_path(path, crate),
            "exports": exports,
        })
        exports_by_name.update(exports)

    aliases = package_names(root)
    # Precompute source identifiers once. A filename or declaration alone
    # cannot produce an edge. This also keeps the audit fast on large crates.
    identifiers = {path: set(IDENT.findall(code)) for path, code in sources.items()}
    by_identifier = defaultdict(set)
    for path, words in identifiers.items():
        for word in words:
            by_identifier[word].add(path)

    def consumers(module):
        name = module["name"].split("::")[-1]
        path = module["path"]
        crate = module["crate"]
        alias = aliases.get(crate, "")
        candidates = set(by_identifier.get(name, ()))
        # Distinctive public symbols detect use after an import or a reexport.
        # Bare PAGE_SIZE, read_slice, or update are common unrelated identifiers.
        unique_exports = [symbol for symbol in module["exports"]
                          if exports_by_name[symbol] == 1 and
                          ((len(symbol) >= 8 and symbol[0].isupper() and
                            any(char.islower() for char in symbol)) or
                           (len(symbol) >= 16 and symbol.count("_") >= 2 and
                            symbol.islower()))]
        for symbol in unique_exports:
            candidates.update(by_identifier.get(symbol, ()))

        hits = set(included_by.get(path, ()))
        qualified = re.compile(r"\b" + re.escape(name) + r"\s*::")
        imported = re.compile(r"\buse\b[^;]*\b" + re.escape(name) + r"\b")
        cross_crate = re.compile(r"\b" + re.escape(alias) + r"\s*::\s*" + re.escape(name) + r"\b") if alias else None
        for candidate in sorted(candidates):
            if candidate == path:
                continue
            code = sources[candidate]
            same_crate = candidate.startswith("src/") if crate == "." else candidate.startswith(f"{crate}/src/")
            symbol_ref = any(symbol in identifiers[candidate] for symbol in unique_exports)
            module_ref = bool((same_crate and (qualified.search(code) or imported.search(code)))
                              or (cross_crate and cross_crate.search(code)))
            if symbol_ref or module_ref:
                hits.add(candidate)
        return sorted(hits)

    results = []
    refs_by_module = {}
    for module in modules:
        refs = consumers(module)
        refs_by_module[(module["crate"], module["name"])] = refs
        results.append({"module": module["path"], "status": "wired" if refs else "unwired",
                        "referenced_by": refs})

    contract_results = []
    for crate, entries in WIRING_CONTRACTS.items():
        for name, groups in entries.items():
            path = f"{crate}/src/{name}.rs"
            # Contracts apply to implemented or declared modules. This also
            # allows a small synthetic workspace with no contract crates.
            lib = production_code(raw_sources.get(f"{crate}/src/lib.rs", ""))
            # A declared but missing source is a pending contract as well.
            declared = re.search(r"\bmod\s+" + name + r"\s*;", lib)
            if path not in sources and not declared:
                continue
            refs = set(refs_by_module.get((crate, name), ()))
            matched = []
            missing = []
            for group in groups:
                selected = sorted(path for path in refs if any(
                    path.startswith(pattern[:-1]) if pattern.endswith("*") else path == pattern
                    for pattern in group))
                matched.append(selected)
                if not selected:
                    missing.append(list(group))
            contract_results.append({"module": f"{crate}/{name}",
                                     "status": "pending" if missing else "wired",
                                     "required": [list(group) for group in groups],
                                     "matched": matched, "missing": missing})

    counts = Counter(result["status"] for result in results)
    contract_counts = Counter(result["status"] for result in contract_results)
    return {
        "root": str(root),
        "summary": {
            "modules_total": len(results), "modules_wired": counts["wired"],
            "modules_unwired": counts["unwired"],
            "contracts_total": len(contract_results), "contracts_wired": contract_counts["wired"],
            "contracts_pending": contract_counts["pending"],
        },
        "contracts": contract_results, "modules": results,
    }


def print_text(report):
    summary = report["summary"]
    print(f"Module wiring: {summary['modules_wired']}/{summary['modules_total']} wired, "
          f"{summary['modules_unwired']} unwired")
    print(f"Wiring contracts: {summary['contracts_wired']}/{summary['contracts_total']} wired, "
          f"{summary['contracts_pending']} pending")
    for contract in report["contracts"]:
        if contract["status"] == "pending":
            targets = [" or ".join(group) for group in contract["missing"]]
            print(f"PENDING contract {contract['module']} -> {' AND '.join(targets)}")
    for module in report["modules"]:
        if module["status"] == "unwired":
            print(f"UNWIRED module {module['module']}: no production consumers")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=REPO_ROOT, help="repository directory")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    parser.add_argument("--strict", action="store_true", help="fail for unwired modules or contracts")
    parser.add_argument("--report-only", action="store_true", help="report pending wiring without failing")
    args = parser.parse_args()
    root = args.root.resolve()
    if not (root / "src").is_dir():
        parser.error(f"missing Rust source directory: {root / 'src'}")
    report = audit(root)
    if args.format == "json":
        print(json.dumps(report, indent=2))
    else:
        print_text(report)
    summary = report["summary"]
    if args.strict and not args.report_only and (summary["modules_unwired"] or summary["contracts_pending"]):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
