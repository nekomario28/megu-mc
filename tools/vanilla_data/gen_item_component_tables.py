#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Check or export dense item and data-component registry tables."""

import argparse
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
PROTO = ROOT / "proto/src/data_v767.rs"
MENUS = ROOT / "src/menus_and_items.rs"
ITEM_COUNT = 1333
COMPONENT_COUNT = 57
PREFIX = "minecraft:"


def rust_array(path, name):
    source = path.read_text(encoding="utf-8")
    declaration = re.search(
        rf"\b(?:pub(?:\s*\([^)]*\))?\s+)?static\s+{re.escape(name)}\s*:",
        source,
    )
    if declaration is None:
        raise ValueError(f"{path}: {name} declaration not found")
    equals = source.find("=", declaration.end())
    start = source.find("[", equals)
    if equals < 0 or start < 0:
        raise ValueError(f"{path}: {name} array initializer not found")
    depth = 0
    quoted = escaped = False
    end = None
    for pos in range(start, len(source)):
        char = source[pos]
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
            continue
        if char == '"':
            quoted = True
        elif char == "[":
            depth += 1
        elif char == "]":
            depth -= 1
            if depth == 0:
                end = pos
                break
    if end is None:
        raise ValueError(f"{path}: unterminated {name} array")
    return re.findall(r'"((?:[^"\\]|\\.)*)"', source[start : end + 1])


def dense_registry(report, registry):
    record = report.get(registry)
    if not isinstance(record, dict) or not isinstance(record.get("entries"), dict):
        raise ValueError(f"registries.json missing {registry}.entries object")
    by_id = {}
    for name, entry in record["entries"].items():
        if not isinstance(name, str) or not name.startswith(PREFIX):
            raise ValueError(f"{registry}: invalid namespaced name {name!r}")
        if not isinstance(entry, dict):
            raise ValueError(f"{registry}[{name!r}] must be an object")
        number = entry.get("protocol_id")
        if type(number) is not int or number < 0:
            raise ValueError(f"{registry}[{name!r}].protocol_id must be nonnegative integer")
        if number in by_id:
            raise ValueError(f"{registry}: duplicate protocol id {number}")
        by_id[number] = name
    if not by_id:
        raise ValueError(f"{registry}: registry must not be empty")
    expected_ids = set(range(len(by_id)))
    if set(by_id) != expected_ids:
        raise ValueError(f"{registry}: protocol ids must be contiguous from zero")
    names = [by_id[number] for number in range(len(by_id))]
    if len(set(names)) != len(names):
        raise ValueError(f"{registry}: duplicate names")
    return names


def bare(name):
    if not name.startswith(PREFIX):
        raise ValueError(f"expected {PREFIX} name, got {name!r}")
    return name[len(PREFIX) :]


def component_constant(name):
    return re.sub(r"[^A-Z0-9]+", "_", bare(name).upper())


def render(items, components):
    lines = ["/// minecraft:item names in protocol-id order.",
             f"pub(crate) static V767_ITEMS: [&str; {len(items)}] = ["]
    lines.extend(f'    "{name}",' for name in items)
    lines += ["];", "", "/// minecraft:data_component_type names in protocol-id order.",
              f"pub(crate) static V767_COMPONENTS: [&str; {len(components)}] = ["]
    lines.extend(f'    "{name}",' for name in components)
    lines += ["];", "", "pub mod component_ids {"]
    for number, name in enumerate(components):
        lines.append(f"    pub const {component_constant(name)}: u32 = {number};")
    lines += ["}", ""]
    return "\n".join(lines)


def check():
    items = rust_array(PROTO, "V767_ITEMS")
    components = rust_array(PROTO, "V767_COMPONENTS")
    parity_items = rust_array(MENUS, "ITEM_NAMES")
    parity_components = rust_array(MENUS, "COMP_WIRE_NAMES")
    problems = []
    for label, table, expected in (("V767_ITEMS", items, ITEM_COUNT),
                                   ("V767_COMPONENTS", components, COMPONENT_COUNT)):
        if len(table) != expected:
            problems.append(f"{label}: expected {expected} names, found {len(table)}")
        if len(set(table)) != len(table):
            problems.append(f"{label}: duplicate names")
        if any(not name.startswith(PREFIX) for name in table):
            problems.append(f"{label}: names must use the minecraft: namespace")
    if len(parity_items) != len(items):
        problems.append(f"ITEM_NAMES parity length: {len(parity_items)} vs {len(items)}")
    else:
        for idx, (bare_name, full_name) in enumerate(zip(parity_items, items)):
            if PREFIX + bare_name != full_name:
                problems.append(f"ITEM_NAMES parity at id {idx}: {bare_name!r} vs {full_name!r}")
                break
    if len(parity_components) != len(components):
        problems.append(f"COMP_WIRE_NAMES parity length: {len(parity_components)} vs {len(components)}")
    else:
        for idx, (left, right) in enumerate(zip(parity_components, components)):
            if left != right:
                problems.append(f"COMP_WIRE_NAMES parity at id {idx}: {left!r} vs {right!r}")
                break
    marker = "pub mod component_ids {"
    source = PROTO.read_text(encoding="utf-8")
    if marker not in source:
        problems.append("component_ids module not found")
        constants = {}
    else:
        constants = dict((name, int(value)) for name, value in re.findall(
            r"pub const ([A-Z0-9_]+): u32 = (\d+);", source.split(marker, 1)[1]
        ))
    expected_constants = {component_constant(name): idx for idx, name in enumerate(components)}
    if constants != expected_constants:
        problems.append("component_ids constants do not exactly match V767_COMPONENTS")
    if problems:
        for problem in problems:
            print(f"item/component tables: FAIL {problem}")
        return 1
    print(f"item/component tables: PASS ({len(items)} items, {len(components)} components, contiguous ids and parity)")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--from-registries-json", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if args.check:
        if args.out:
            parser.error("--out is only valid with --from-registries-json")
        try:
            return check()
        except (OSError, ValueError) as error:
            print(f"item/component tables: FAIL {error}", file=sys.stderr)
            return 1
    if args.out is None:
        parser.error("--from-registries-json requires --out")
    try:
        report = json.loads(args.from_registries_json.read_text(encoding="utf-8"))
        if not isinstance(report, dict):
            raise ValueError("registries.json root must be an object")
        items = dense_registry(report, "minecraft:item")
        components = dense_registry(report, "minecraft:data_component_type")
        args.out.write_text(render(items, components), encoding="utf-8")
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"item/component tables: FAIL {error}", file=sys.stderr)
        return 1
    print(f"item/component tables: wrote {args.out} ({len(items)} items, {len(components)} components)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
