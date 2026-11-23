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


def load_tolerated_unwired(path):
    """Read the tolerated-unwired allowlist: documented exceptions every
    strict run still fails on beyond, plus a stale-entry check so wired
    modules cannot keep a dead exemption."""
    doc = json.loads(Path(path).read_text())
    entries = doc.get("entries")
    if not isinstance(entries, list):
        raise ValueError("tolerated-unwired file must contain an 'entries' list")
    out = {}
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict) or "module" not in entry or "reason" not in entry:
            raise ValueError(f"tolerated-unwired entry #{index} needs module and reason")
        out[entry["module"]] = entry["reason"]
    return out


def apply_tolerated_unwired(report, tolerated):
    """Split the unwired modules into tolerated and new, and flag tolerated
    entries whose module is wired again (stale exemptions must be dropped)."""
    unwired = {m["module"] for m in report["modules"] if m["status"] == "unwired"}
    report["unwired_new"] = sorted(unwired - tolerated.keys())
    report["unwired_stale_tolerated"] = sorted(m for m in tolerated if m not in unwired)
    return report


def is_av_test_path(rel):
    parts = Path(rel).parts
    if "tests" in parts[:-1]:
        return True
    stem = Path(rel).stem
    return stem == "tests" or stem.endswith("_test") or stem.endswith("_tests")


def av_source_files(root):
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        parts = Path(rel).parts
        if any(part.startswith(".") or part == "target" for part in parts[:-1]):
            continue
        yield rel


def strip_imports(code):
    """Remove `use` declarations so imports/reexports are not call sites."""
    return re.sub(r"\b(?:pub\s*(?:\([^)]*\)\s*)?)?use\s+[^;]*;", " ", code)


def _load_av_json(path, label):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise ValueError(f"missing {label}: {path}") from exc
    except json.JSONDecodeError as exc:
        raise ValueError(f"invalid JSON in {label} {path}: {exc}") from exc


def _normalize_av_features(doc):
    entries = doc.get("features") if isinstance(doc, dict) else None
    if not isinstance(entries, list):
        raise ValueError("AV features file must contain a 'features' list")
    features = []
    errors = []
    for idx, raw in enumerate(entries):
        if not isinstance(raw, dict):
            errors.append(f"AV feature #{idx} is not an object")
            continue
        symbol = raw.get("symbol")
        file = raw.get("file")
        consumers = raw.get("consumers")
        via = raw.get("via", [])
        if not symbol or not file or not isinstance(consumers, list) or not consumers:
            errors.append(f"AV feature #{idx} missing symbol/file/consumers")
            continue
        if not isinstance(via, list):
            errors.append(f"AV feature {symbol} has non-list 'via'")
            continue
        features.append({
            "symbol": str(symbol),
            "file": str(file),
            "consumers": [str(item) for item in consumers],
            "via": [str(item) for item in via],
        })
    return features, errors


def _normalize_av_allowlist(doc):
    entries = doc.get("entries", []) if isinstance(doc, dict) else []
    if not isinstance(entries, list):
        raise ValueError("AV allowlist must contain an 'entries' list")
    normalized = []
    errors = []
    seen = set()
    for idx, raw in enumerate(entries):
        if not isinstance(raw, dict):
            errors.append(f"AV allowlist entry #{idx} is not an object")
            continue
        symbol = raw.get("symbol")
        file = raw.get("file")
        reason = raw.get("reason")
        date = raw.get("date")
        if not symbol or not file or not reason or not date:
            errors.append(f"AV allowlist entry #{idx} missing symbol/file/reason/date")
            continue
        if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", str(date)):
            errors.append(f"AV allowlist entry {symbol} ({file}) has non-ISO date {date!r}")
            continue
        key = (str(file), str(symbol))
        if key in seen:
            errors.append(f"duplicate AV allowlist entry {symbol} ({file})")
            continue
        seen.add(key)
        normalized.append({
            "symbol": str(symbol), "file": str(file),
            "reason": str(reason), "date": str(date),
        })
    return normalized, errors


def audit_av(root, features_path=None, allowlist_path=None):
    """Audit declared AV feature entry symbols for live consumer reachability."""
    root = Path(root)
    features_path = Path(features_path) if features_path else root / "tools/wiring_audit_features.json"
    allowlist_path = (Path(allowlist_path) if allowlist_path
                      else root / "tools/wiring_audit_allowlist.json")
    features_doc = _load_av_json(features_path, "AV features file")
    allow_doc = (_load_av_json(allowlist_path, "AV allowlist")
                 if allowlist_path.is_file() else {"entries": []})
    features, errors = _normalize_av_features(features_doc)
    allow_entries, allow_errors = _normalize_av_allowlist(allow_doc)
    errors.extend(allow_errors)
    seen_features = set()
    for feature in features:
        key = (feature["file"], feature["symbol"])
        if key in seen_features:
            errors.append(f"duplicate AV feature {feature['symbol']} ({feature['file']})")
        seen_features.add(key)

    interesting = {f["symbol"] for f in features}
    for feature in features:
        interesting.update(feature["via"])

    all_files = list(av_source_files(root))
    raw_sources = {}
    test_only = set()
    for rel in all_files:
        text = (root / rel).read_text(encoding="utf-8")
        raw_sources[rel] = text
        for match in re.finditer(
                r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*mod\s+(\w+)\s*;", text):
            test_only.add((Path(rel).parent / f"{match.group(1)}.rs").as_posix())

    def is_test(rel):
        return is_av_test_path(rel) or rel in test_only

    refs = defaultdict(set)
    for rel in all_files:
        if is_test(rel):
            continue
        code = strip_imports(production_code(raw_sources[rel]))
        hits = set(IDENT.findall(code)) & interesting
        for symbol in hits:
            refs[symbol].add(rel)

    allow_by_key = {(e["file"], e["symbol"]): e for e in allow_entries}
    feature_by_key = {(f["file"], f["symbol"]): f for f in features}
    for feature in features:
        symbol = feature["symbol"]
        file = feature["file"]
        consumers = set(feature["consumers"])
        direct = refs.get(symbol, set())
        via_refs = set()
        for via in feature["via"]:
            via_refs.update(refs.get(via, set()))
        effective = direct | via_refs
        feature["refs"] = sorted(direct - {file})
        feature["consumer_refs"] = sorted(
            path for path in effective if path in consumers and path != file)
        feature["status"] = "wired" if feature["consumer_refs"] else "unreachable"
        feature["allowlisted"] = (file, symbol) in allow_by_key
        if not (root / file).is_file():
            errors.append(f"AV feature {symbol}: missing owner file {file}")
        elif file not in direct:
            errors.append(f"AV feature {symbol}: owner file {file} has no production reference")
        for consumer in feature["consumers"]:
            if is_test(consumer):
                errors.append(f"AV feature {symbol}: consumer root {consumer} is a test path")
            elif not (root / consumer).is_file():
                errors.append(f"AV feature {symbol}: missing consumer root {consumer}")

    unreachable_unallowlisted = [
        f for f in features
        if f["status"] == "unreachable" and not f["allowlisted"]
    ]
    stale_allowlist = [
        e for key, e in allow_by_key.items()
        if feature_by_key.get(key) is None
        or feature_by_key[key]["status"] != "unreachable"
    ]
    wired = sum(1 for f in features if f["status"] == "wired")
    unreachable = len(features) - wired
    return {
        "root": str(root),
        "features_path": str(features_path),
        "allowlist_path": str(allowlist_path),
        "summary": {
            "features_total": len(features),
            "features_wired": wired,
            "features_unreachable": unreachable,
            "allowlist_entries": len(allow_entries),
            "unreachable_unallowlisted": len(unreachable_unallowlisted),
            "stale_allowlist_entries": len(stale_allowlist),
            "errors": len(errors),
        },
        "features": features,
        "allowlist": {
            "entries": allow_entries,
            "stale": stale_allowlist,
            "unreachable_unallowlisted": [
                {"symbol": f["symbol"], "file": f["file"]} for f in unreachable_unallowlisted
            ],
        },
        "errors": errors,
    }


def print_av_text(report):
    summary = report["summary"]
    print(f"AV reachability: {summary['features_wired']}/{summary['features_total']} wired, "
          f"{summary['features_unreachable']} unreachable")
    if summary["allowlist_entries"]:
        print(f"AV allowlist: {summary['allowlist_entries']} entries, "
              f"{summary['stale_allowlist_entries']} stale")
    for error in report["errors"]:
        print(f"ERROR {error}")
    for feature in report["features"]:
        suffix = " (allowlisted)" if feature["allowlisted"] else ""
        consumers = ",".join(feature["consumers"])
        print(f"{feature['status']:<11} {feature['symbol']:<36} {feature['file']:<48} "
              f"consumers={consumers}{suffix}")
        if feature["consumer_refs"]:
            print(f"           live refs: {', '.join(feature['consumer_refs'])}")
        elif feature["refs"]:
            print(f"           non-root refs: {', '.join(feature['refs'])}")
    for entry in report["allowlist"]["stale"]:
        print(f"STALE allowlist {entry['symbol']} ({entry['file']})")


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
    parser.add_argument("--strict-contracts", action="store_true", help="fail for pending wiring contracts")
    parser.add_argument("--report-only", action="store_true", help="report pending wiring without failing")
    parser.add_argument("--av-reachability", action="store_true",
                        help="audit declared audiovisual feature entry symbols for live consumers")
    parser.add_argument("--av-features", type=Path, default=None,
                        help="AV feature list (default <root>/tools/wiring_audit_features.json)")
    parser.add_argument("--av-allowlist", type=Path, default=None,
                        help="AV allowlist (default <root>/tools/wiring_audit_allowlist.json)")
    parser.add_argument("--tolerated-unwired", type=Path, default=None,
                        help="JSON list of documented unwired modules --strict still tolerates")
    args = parser.parse_args()
    root = args.root.resolve()
    if not (root / "src").is_dir():
        parser.error(f"missing Rust source directory: {root / 'src'}")
    if args.av_reachability:
        try:
            report = audit_av(root, args.av_features, args.av_allowlist)
        except ValueError as exc:
            parser.error(str(exc))
        if args.format == "json":
            print(json.dumps(report, indent=2))
        else:
            print_av_text(report)
        summary = report["summary"]
        if args.strict and (
            summary["unreachable_unallowlisted"]
            or summary["stale_allowlist_entries"]
            or summary["errors"]
        ):
            return 1
        return 0
    report = audit(root)
    if args.tolerated_unwired:
        try:
            tolerated = load_tolerated_unwired(args.tolerated_unwired)
        except (OSError, ValueError) as exc:
            parser.error(str(exc))
        apply_tolerated_unwired(report, tolerated)
    if args.format == "json":
        print(json.dumps(report, indent=2))
    else:
        print_text(report)
        for module in report.get("unwired_new", ()):
            print(f"UNWIRED-NEW module {module}: not in the tolerated list")
        for module in report.get("unwired_stale_tolerated", ()):
            print(f"STALE-TOLERATED module {module}: wired now — drop the exemption")
    summary = report["summary"]
    unwired_blocking = (report["unwired_new"] if args.tolerated_unwired
                        else summary["modules_unwired"])
    stale_blocking = report.get("unwired_stale_tolerated", []) if args.tolerated_unwired else []
    if not args.report_only and (
        (args.strict and (unwired_blocking or stale_blocking or summary["contracts_pending"]))
        or (args.strict_contracts and summary["contracts_pending"])
    ):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
