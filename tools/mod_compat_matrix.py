#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Offline structural compatibility matrix and modpack preflight planner.

This tool reports archive structure only. It does not load classes, contact a
catalogue, or claim that an inspected mod works in a live game.
"""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import zipfile

SCHEMA_VERSION = 1
MAX_NESTED_DEPTH = 8
PERF_MODS = {
    "sodium": ("rendering and culling",),
    "embeddium": ("rendering and culling",),
    "rubidium": ("rendering and culling",),
    "lithium": ("simulation and chunk processing",),
    "starlight": ("lighting",),
    "phosphor": ("lighting",),
    "ferritecore": ("memory use",),
    "modernfix": ("memory use and loading",),
    "krypton": ("network processing",),
    "c2me": ("chunk generation",),
    "noisium": ("chunk generation",),
    "entityculling": ("entity culling",),
    "moreculling": ("block and entity culling",),
    "fastload": ("loading",),
}
DATA_MARKERS = {
    "recipes": ("/recipes/", "/recipe/"),
    "tags": ("/tags/",),
    "loot": ("/loot_tables/", "/loot/"),
    "worldgen": ("/worldgen/", "/structure/", "/structures/"),
}


def _read_zip(path):
    try:
        with zipfile.ZipFile(path) as archive:
            return archive.namelist(), {name: archive.read(name) for name in archive.namelist() if not name.endswith("/")}
    except (OSError, zipfile.BadZipFile, RuntimeError):
        return [], {}


def _nested_entries(content, depth=0):
    if depth >= MAX_NESTED_DEPTH:
        return depth, [], {}
    max_depth, names, payloads = depth, [], {}
    from io import BytesIO
    for entry, data in sorted(content.items()):
        if not entry.lower().endswith((".jar", ".zip")):
            continue
        try:
            with zipfile.ZipFile(BytesIO(data)) as archive:
                nested = {name: archive.read(name) for name in archive.namelist() if not name.endswith("/")}
        except (zipfile.BadZipFile, RuntimeError):
            continue
        child_depth = depth + 1
        max_depth = max(max_depth, child_depth)
        names.extend(f"{entry}!/{name}" for name in nested)
        payloads.update({f"{entry}!/{name}": value for name, value in nested.items()})
        child_max, child_names, child_payloads = _nested_entries(nested, child_depth)
        max_depth = max(max_depth, child_max)
        names.extend(f"{entry}!/{name}" for name in child_names)
        payloads.update({f"{entry}!/{name}": value for name, value in child_payloads.items()})
    return max_depth, names, payloads


def _metadata(entries):
    if "fabric.mod.json" in entries:
        try:
            data = json.loads(entries["fabric.mod.json"])
            return "fabric", str(data.get("id") or "unknown"), str(data.get("version") or "unknown")
        except (ValueError, AttributeError):
            return "fabric", "unknown", "unknown"
    if "quilt.mod.json" in entries:
        try:
            data = json.loads(entries["quilt.mod.json"])
            q = data.get("quilt_loader", {})
            return "quilt", str(q.get("id") or "unknown"), str(q.get("version") or "unknown")
        except (ValueError, AttributeError):
            return "quilt", "unknown", "unknown"
    neo = entries.get("META-INF/neoforge.mods.toml") or entries.get("META-INF/mods.toml")
    if neo:
        try:
            doc = tomllib.loads(neo.decode("utf-8"))
            mod = (doc.get("mods") or [{}])[0]
            return "neoforge", str(mod.get("modId") or "unknown"), str(mod.get("version") or "unknown")
        except (ValueError, UnicodeDecodeError, AttributeError):
            return "neoforge", "unknown", "unknown"
    return "unknown", "unknown", "unknown"


def inspect_archive(path, name=None, depth=0):
    path = Path(path)
    label = name or path.name
    if path.suffix.lower() == ".mmod":
        entries, content = _read_zip(path)
        native_valid = "megu-mod.toml" in content and "megu-mod-bundle.json" in content
        try:
            manifest = tomllib.loads(content["megu-mod.toml"].decode("utf-8"))
            mod = manifest.get("mod", {})
            mod_id, version = str(mod.get("id", "unknown")), str(mod.get("version", "unknown"))
        except (KeyError, ValueError, UnicodeDecodeError):
            mod_id, version = "unknown", "unknown"
        loader = "mmod"
    else:
        entries, content = _read_zip(path)
        native_valid = False
        loader, mod_id, version = _metadata(content)
    nested_depth, nested_names, nested_content = _nested_entries(content, depth)
    all_entries = entries + nested_names
    signals = {kind: sorted({entry for entry in all_entries if any(marker in "/" + entry.lower().lstrip("/") for marker in markers)})
               for kind, markers in DATA_MARKERS.items()}
    mixin_files = sorted(entry for entry in all_entries if "mixin" in entry.lower() and entry.lower().endswith((".json", ".toml")))
    mixin_annotations = sorted({name for entry, data in {**content, **nested_content}.items() if entry.endswith(".class")
                                for name in re.findall(rb"L(?:org/spongepowered/asm/mixin|com/llamalad7/mixinextras)/[^\x00]+", data)})
    has_classes = any(entry.endswith(".class") for entry in all_entries)
    perf = sorted((key, reason[0]) for key, reason in PERF_MODS.items()
                  if key in mod_id.lower() or key in path.name.lower() or any(key in entry.lower() for entry in all_entries))
    structural = {"datapack": signals, "mixin_configs": mixin_files,
                  "mixin_annotations": [item.decode("ascii", "replace") for item in mixin_annotations[:100]],
                  "contains_classes": has_classes, "built_in_perf_overlap": [
                      {"mod": mod, "area": area, "recommendation": "skip: Megu provides a native optimization in this area"}
                      for mod, area in perf]}
    if loader == "mmod" and native_valid:
        status = "Full"
        reasons = []
    elif loader == "mmod":
        status = "Blocked / Sidecar JVM needed"
        reasons = ["native package is missing its manifest or declarative bundle"]
    elif has_classes or mixin_files or mixin_annotations:
        status = "Blocked / Sidecar JVM needed"
        reasons = ["Java bytecode or Mixin structures require an isolated JVM sidecar"]
    elif any(signals.values()):
        status = "Partial (optional features skipped)"
        reasons = ["data resources were detected; loader-specific behavior is not established"]
    else:
        status = "Blocked / Sidecar JVM needed"
        reasons = ["no supported native declarative content was identified"]
    dependencies = []
    try:
        fabric = json.loads(content["fabric.mod.json"]) if "fabric.mod.json" in content else {}
        quilt = json.loads(content["quilt.mod.json"]) if "quilt.mod.json" in content else {}
        if loader == "fabric":
            dependencies = [key for key in (fabric.get("depends") or {})
                            if key not in {"minecraft", "java", "fabricloader"}]
        elif loader == "quilt":
            dependencies = [str(item.get("id")) for item in quilt.get("quilt_loader", {}).get("depends", [])
                            if isinstance(item, dict) and item.get("id") not in {"minecraft", "java", "quilt_loader"}]
        elif loader == "neoforge":
            neo = content.get("META-INF/neoforge.mods.toml") or content.get("META-INF/mods.toml")
            doc = tomllib.loads(neo.decode("utf-8"))
            for _owner, rows in (doc.get("dependencies") or {}).items():
                for row in rows if isinstance(rows, list) else []:
                    dep_id = row.get("modId") if isinstance(row, dict) else None
                    if dep_id and dep_id not in {"minecraft", "neoforge", "forge"} and row.get("type", "required") == "required":
                        dependencies.append(dep_id)
    except (ValueError, UnicodeDecodeError, AttributeError, TypeError):
        dependencies = []
    return {"file": label, "loader": loader, "mod_id": mod_id, "version": version,
            "dependencies": sorted(set(dependencies)),
            "jar_in_jar_depth": nested_depth,
            "structural_signals": structural, "compatibility": status, "reasons": reasons}


def load_catalog(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    rows = data.get("mods", data) if isinstance(data, dict) else data
    if not isinstance(rows, list):
        raise ValueError("catalog must be a JSON array or an object with a mods array")
    result = []
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("path") or row.get("file"), str):
            raise ValueError("each catalog row must contain a path or file")
        item_path = Path(row.get("path") or row.get("file"))
        if not item_path.is_absolute():
            item_path = Path(path).resolve().parent / item_path
        result.append(item_path)
    return sorted(result, key=lambda p: p.name.lower())


def scan(paths, checker=None):
    records = [inspect_archive(path) for path in sorted(paths, key=lambda p: p.name.lower())]
    if checker:
        for record, path in zip(records, sorted(paths, key=lambda p: p.name.lower())):
            result = subprocess.run([checker, str(path)], capture_output=True, text=True, check=False)
            record["megu_mod_check"] = {"exit_code": result.returncode, "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}
    return sorted(records, key=lambda row: (row["mod_id"].lower(), row["file"].lower()))


def modpack_plan(modpack, records):
    path = Path(modpack)
    deps, listed, missing_dependencies = [], [], []
    if path.suffix.lower() == ".mrpack":
        names, data = _read_zip(path)
        if "modrinth.index.json" in data:
            index = json.loads(data["modrinth.index.json"])
            for item in index.get("files", []):
                filename = Path(item.get("path", "")).name
                listed.append(filename)
    else:
        data = json.loads(path.read_text(encoding="utf-8"))
        project_ids = set()
        for item in data.get("files", []):
            if isinstance(item, dict):
                project_id = item.get("projectID")
                file_id = item.get("fileID")
                label = item.get("fileName") or (f"projectID={project_id},fileID={file_id}" if project_id is not None else "unknown")
                listed.append(str(label))
                if project_id is not None:
                    project_ids.add(str(project_id))
                deps.extend(map(str, item.get("requiredFiles", [])))
        deps.extend(str(dep.get("modId")) for dep in data.get("dependencies", [])
                    if isinstance(dep, dict) and dep.get("relationType") == "required" and dep.get("modId") is not None)
        missing_dependencies = sorted(set(deps) - project_ids)
    by_file = {r["file"].lower(): r for r in records}
    installs, skips = [], []
    selected_records = []
    for filename in sorted(set(listed), key=str.lower):
        record = by_file.get(filename.lower())
        if record and record["structural_signals"]["built_in_perf_overlap"]:
            skips.append({"file": filename, "reason": "Megu handles the identified performance area natively; skipping avoids duplicate optimization layers."})
        else:
            installs.append(filename)
            if record:
                selected_records.append(record)
    selected_ids = {record["mod_id"] for record in selected_records}
    missing_dependencies = sorted(set(missing_dependencies) | {
        dependency for record in selected_records for dependency in record.get("dependencies", [])
        if dependency not in selected_ids
    })
    return {"modpack": path.name, "install": installs, "recommended_skip": skips,
            "missing_dependencies": missing_dependencies,
            "blockers": [r["file"] for r in selected_records if r["compatibility"].startswith("Blocked")]}


def markdown(matrix):
    lines = ["| Mod | Loader | Version | JAR-in-JAR depth | Compatibility | Structural signals |",
             "| --- | --- | --- | ---: | --- | --- |"]
    for row in matrix["mods"]:
        sig = row["structural_signals"]
        features = [name for name, values in sig["datapack"].items() if values]
        if sig["mixin_configs"] or sig["mixin_annotations"]:
            features.append("Mixin")
        if sig["built_in_perf_overlap"]:
            features.append("built-in perf overlap")
        lines.append(f"| {row['mod_id']} | {row['loader']} | {row['version']} | {row['jar_in_jar_depth']} | {row['compatibility']} | {', '.join(features) or 'none'} |")
    return "\n".join(lines) + "\n"


def self_test():
    import tempfile
    from zipfile import ZipFile
    with tempfile.TemporaryDirectory(prefix="mod-matrix-") as temp:
        root = Path(temp)
        for filename, entries in (("fabric.jar", {"fabric.mod.json": b'{"id":"sample","version":"1"}', "data/sample/recipe/a.json": b"{}"}),
                                  ("java.jar", {"x.class": b"Mixin", "sample.mixins.json": b"{}"}),
                                  ("plain.mmod", {"megu-mod.toml": b'[mod]\nid="sample"\nversion="1.0.0"', "megu-mod-bundle.json": b"{}"})):
            with ZipFile(root / filename, "w") as archive:
                for name, payload in entries.items():
                    archive.writestr(name, payload)
        assert inspect_archive(root / "fabric.jar")["loader"] == "fabric"
        assert inspect_archive(root / "java.jar")["compatibility"].startswith("Blocked")
        assert inspect_archive(root / "plain.mmod")["compatibility"] == "Full"
    print("mod compatibility matrix self-test: PASS")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--input-dir", type=Path)
    source.add_argument("--catalog", type=Path)
    parser.add_argument("--modpack", type=Path)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--markdown", action="store_true")
    parser.add_argument("--megu-mod-check", help="optional path to megu-mod-check executable")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    try:
        paths = ([p for p in args.input_dir.iterdir() if p.suffix.lower() in (".jar", ".mmod")] if args.input_dir else load_catalog(args.catalog) if args.catalog else [])
        records = scan(paths, args.megu_mod_check)
        output = {"schema_version": SCHEMA_VERSION, "mods": records}
        if args.modpack:
            output["modpack_plan"] = modpack_plan(args.modpack, records)
        if args.markdown:
            print(markdown(output), end="")
        else:
            print(json.dumps(output, indent=2, sort_keys=True))
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
