#!/usr/bin/env python3
"""Fail-closed static compatibility audit for bounded Fabric 1.21.1 server mods."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
from urllib.request import urlopen
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parent
SCANNER = ROOT / "java/net/megu/probe/FabricShimCheck.java"
MAX_JAR = 64 * 1024 * 1024


def digest(data):
    return hashlib.sha256(data).hexdigest()


def fail(code, message, jar=None, resource=None):
    return {"code": code, "message": message, **({"jar": jar} if jar else {}),
            **({"resource": resource} if resource else {})}


def version_tuple(value):
    match = re.fullmatch(r"(\d+(?:\.\d+)*)(?:[-+].*)?", value)
    if not match:
        raise ValueError("unsupported version expression")
    return tuple(int(part) for part in match.group(1).split("."))


def satisfies(expression, version):
    if expression == "*": return True
    target = version_tuple(version)
    pieces = expression.split()
    if not pieces: raise ValueError("empty version predicate")
    for piece in pieces:
        match = re.fullmatch(r"(>=|<=|>|<|=)?(\d+(?:\.\d+)*(?:[-+][A-Za-z0-9.-]+)?)", piece)
        if not match: raise ValueError("unsupported version predicate: " + expression)
        op, raw = match.groups(); candidate = version_tuple(raw)
        op = op or "="
        if not {"=": target == candidate, ">": target > candidate, "<": target < candidate,
                ">=": target >= candidate, "<=": target <= candidate}[op]: return False
    return True


def deps():
    pins = json.loads((ROOT / "dependencies.json").read_text())["libraries"]
    selected = [p for p in pins if p["name"] in ("org.ow2.asm:asm:9.10.1", "org.ow2.asm:asm-tree:9.10.1")]
    paths = []
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    for pin in selected:
        path = cache / pin["url"].rsplit("/", 1)[1]
        if not path.exists():
            with urlopen(pin["url"], timeout=30) as response:
                data = response.read(20 * 1024 * 1024 + 1)
            if len(data) > 20 * 1024 * 1024 or digest(data) != pin["sha256"]:
                raise ValueError("pinned ASM dependency size/hash mismatch: " + path.name)
            path.write_bytes(data)
        if digest(path.read_bytes()) != pin["sha256"]:
            raise ValueError("pinned ASM dependency hash mismatch: " + path.name)
        paths.append(path)
    return paths


def scanner(java_home, asm, tmpdir, classes):
    cp = os.pathsep.join(map(str, asm))
    javac = str(java_home / "bin/javac") if java_home else "javac"
    command = [javac, "-J-Djava.io.tmpdir=" + str(tmpdir), "-J-XX:-UsePerfData", "--release", "21", "-proc:none", "-cp", cp, "-d", str(classes), str(SCANNER)]
    subprocess.run(command, check=True, env={**os.environ, "TMPDIR": str(tmpdir)})
    return cp


def metadata_for(path, zf, names):
    reasons, client_mixins, declared_classes = [], set(), set()
    if len(names) != len(set(names)):
        reasons.append(fail("DUPLICATE_ARCHIVE_ENTRY", "duplicate ZIP entries", str(path)))
    meta_paths = [n for n in names if n == "fabric.mod.json"]
    if len(meta_paths) != 1:
        return {}, [fail("MISSING_METADATA", "expected exactly one fabric.mod.json", str(path), "fabric.mod.json")], client_mixins
    try:
        raw = json.loads(zf.read("fabric.mod.json"))
        if not isinstance(raw, dict): raise ValueError("metadata root must be an object")
    except Exception as exc:
        return {}, [fail("MALFORMED_METADATA", str(exc), str(path), "fabric.mod.json")], client_mixins
    for key, valid in (("schemaVersion", lambda x: x == 1), ("id", lambda x: isinstance(x, str) and bool(x)),
                       ("version", lambda x: isinstance(x, str) and bool(x))):
        if not valid(raw.get(key)):
            reasons.append(fail("INVALID_METADATA", "invalid or missing " + key, str(path), "fabric.mod.json"))
    env = raw.get("environment", "*")
    if env == "client": reasons.append(fail("CLIENT_ONLY_MOD", "mod environment is client", str(path), "fabric.mod.json"))
    elif env not in ("*", "server"):
        reasons.append(fail("UNSUPPORTED_ENVIRONMENT", "environment must be '*', 'server', or 'client'", str(path), "fabric.mod.json"))
    depends = raw.get("depends", {})
    if not isinstance(depends, dict): reasons.append(fail("INVALID_DEPENDS", "depends must be an object", str(path), "fabric.mod.json"))
    elif "fabricloader" not in depends:
        reasons.append(fail("INVALID_DEPENDS", "fabricloader dependency is required", str(path), "fabric.mod.json"))
    else:
        for dependency, predicate in depends.items():
            expressions = predicate if isinstance(predicate, list) else [predicate]
            if not expressions or any(not isinstance(item, str) for item in expressions):
                reasons.append(fail("INVALID_DEPENDS", "dependency predicates must be strings or nonempty string arrays", str(path), "fabric.mod.json")); continue
            try:
                targets = ("0.15.11", "0.19.5") if dependency == "fabricloader" else (("1.21.1",) if dependency == "minecraft" else ())
                if targets and not any(any(satisfies(item, target) for item in expressions) for target in targets):
                    reasons.append(fail("UNSATISFIED_DEPENDENCY", "dependency excludes the pinned server profile: " + dependency, str(path), "fabric.mod.json"))
                else:
                    for item in expressions: satisfies(item, "0.19.5")
            except ValueError as exc:
                reasons.append(fail("UNSUPPORTED_DEPENDENCY_PREDICATE", str(exc), str(path), "fabric.mod.json"))
    if "languageAdapters" in raw:
        reasons.append(fail("UNSUPPORTED_LANGUAGE_ADAPTER", "custom language adapters are unsupported", str(path), "fabric.mod.json"))
    entrypoints = raw.get("entrypoints", {})
    if not isinstance(entrypoints, dict): reasons.append(fail("INVALID_ENTRYPOINTS", "entrypoints must be an object", str(path), "fabric.mod.json"))
    else:
        for key in ("main", "server"):
            values = entrypoints.get(key, [])
            if not isinstance(values, list):
                reasons.append(fail("INVALID_ENTRYPOINTS", f"entrypoints.{key} must be an array", str(path), "fabric.mod.json"))
            elif any(not isinstance(v, str) or ":" in v for v in values):
                reasons.append(fail("UNSUPPORTED_ENTRYPOINT", "entrypoint adapters/objects are not supported", str(path), "fabric.mod.json"))
    if raw.get("accessWidener") is not None:
        reasons.append(fail("UNSUPPORTED_ACCESS_WIDENER", "declared access widener is unsupported", str(path), str(raw.get("accessWidener"))))
    if any(n.endswith(".accesswidener") for n in names):
        reasons.append(fail("UNSUPPORTED_ACCESS_WIDENER", "access widener resource is unsupported", str(path)))
    for entry in names:
        if entry.lower().endswith(".jar") and not entry.startswith("META-INF/"):
            reasons.append(fail("UNVERIFIED_NESTED_JAR", "nested JARs are not audited", str(path), entry))
    declared = raw.get("mixins", [])
    if not isinstance(declared, list):
        reasons.append(fail("INVALID_MIXINS", "mixins must be an array", str(path), "fabric.mod.json")); declared = []
    for item in declared:
        if isinstance(item, str): config = item; side = "*"
        elif isinstance(item, dict) and isinstance(item.get("config"), str): config = item["config"]; side = item.get("environment", "*")
        else:
            reasons.append(fail("INVALID_MIXINS", "mixin declaration must name a config", str(path), "fabric.mod.json")); continue
        if side not in ("*", "server", "client"):
            reasons.append(fail("UNSUPPORTED_MIXIN_ENVIRONMENT", "invalid declared mixin environment", str(path), config)); continue
        if config not in names:
            reasons.append(fail("MISSING_MIXIN_CONFIG", "declared mixin config is absent", str(path), config)); continue
        try:
            cfg = json.loads(zf.read(config))
            if not isinstance(cfg, dict): raise ValueError("config root must be an object")
            if cfg.get("plugin") or cfg.get("refmap") or cfg.get("overwrites"):
                reasons.append(fail("UNSUPPORTED_MIXIN_CONFIG", "plugins, refmaps, and overwrites are unsupported", str(path), config))
            config_env = cfg.get("environment", "*")
            if config_env not in ("*", "CLIENT", "SERVER"):
                reasons.append(fail("UNSUPPORTED_MIXIN_ENVIRONMENT", "invalid mixin config environment", str(path), config))
            package = cfg.get("package", "")
            if package and not isinstance(package, str): raise ValueError("package must be a string")
            prefix = package.replace(".", "/") + "/" if package else ""
            mixin_lists = {}
            for key in ("mixins", "client", "server"):
                members = cfg.get(key, [])
                if not isinstance(members, list) or any(not isinstance(name, str) for name in members):
                    raise ValueError(key + " must be an array of class names")
                mixin_lists[key] = members
                declared_classes.update(prefix + name.replace(".", "/") for name in members)
            client_names = set(mixin_lists["client"])
            if config_env == "CLIENT" or side == "client":
                client_names.update(mixin_lists["mixins"] + mixin_lists["server"])
            client_mixins.update(prefix + name.replace(".", "/") for name in client_names)
        except Exception as exc:
            reasons.append(fail("MALFORMED_MIXIN_CONFIG", str(exc), str(path), config))
    for class_name in sorted(declared_classes):
        if class_name + ".class" not in names:
            reasons.append(fail("MISSING_MIXIN_CLASS", "declared mixin class is absent", str(path), class_name))
    return raw, reasons, client_mixins


PLATFORM_PREFIXES = ("net/minecraft/", "net/fabricmc/", "com/mojang/brigadier/")
RUNTIME_RESERVED = (*PLATFORM_PREFIXES, "net/megu/probe/", "java/",
                    "org/spongepowered/asm/", "org/objectweb/asm/")
ENTRY_CLASS = re.compile(r"[A-Za-z_$][A-Za-z0-9_$]*(?:\.[A-Za-z_$][A-Za-z0-9_$]*)*")


def runtime_metadata(raw, names, reasons, jar):
    if raw.get("mixins"):
        reasons.append(fail("UNSUPPORTED_RUNTIME_MIXIN", "runtime has no Mixin transformer", jar, "fabric.mod.json"))
    if raw.get("jars") or any(n.lower().endswith(".jar") for n in names):
        reasons.append(fail("UNVERIFIED_NESTED_JAR", "runtime loads only submitted JARs", jar))
    if "META-INF/INDEX.LIST" in names or any(n.startswith("META-INF/versions/") for n in names):
        reasons.append(fail("UNSUPPORTED_ARCHIVE_LAYOUT", "indexed and multi-release JARs are unsupported", jar))
    if "META-INF/neoforge.mods.toml" in names or "META-INF/mods.toml" in names:
        reasons.append(fail("CONFLICTING_METADATA", "JAR also declares another mod loader", jar))
    if isinstance(raw.get("depends"), dict):
        for dependency in sorted(set(raw["depends"]) - {"fabricloader", "minecraft", "java"}):
            reasons.append(fail("UNSUPPORTED_RUNTIME_DEPENDENCY", dependency, jar, "fabric.mod.json"))
        predicates = raw["depends"].get("java", "*")
        predicates = predicates if isinstance(predicates, list) else [predicates]
        if predicates and all(isinstance(p, str) for p in predicates):
            try:
                if not any(satisfies(p, "21") for p in predicates):
                    reasons.append(fail("UNSATISFIED_DEPENDENCY", "requires a different Java version", jar))
            except ValueError:
                pass  # metadata_for already reports unsupported predicates.
    entries = {"main": [], "server": [], "client": []}
    if isinstance(raw.get("entrypoints"), dict):
        for kind, result in entries.items():
            configured = raw["entrypoints"].get(kind, [])
            if isinstance(configured, list):
                for name in configured:
                    if isinstance(name, str) and ENTRY_CLASS.fullmatch(name):
                        result.append(name.replace(".", "/"))
                    elif kind != "client":
                        reasons.append(fail("UNSUPPORTED_ENTRYPOINT", "expected a Java class name", jar, kind))
    return entries


def runtime_classes(rows, entries, reasons, jar):
    server_entries = set(entries["main"] + entries["server"])
    excluded = {row["class"] for row in rows if row["client"]}
    excluded.update(set(entries["client"]) - server_entries)
    classes = {row["class"] for row in rows}
    for name in sorted(server_entries):
        if name not in classes:
            reasons.append(fail("MISSING_ENTRYPOINT_CLASS", "entrypoint is absent from its JAR", jar, name))
        elif name in excluded:
            reasons.append(fail("CLIENT_ONLY_ENTRYPOINT", "server entrypoint is client-only", jar, name))
    for row in rows:
        owner = row["class"]
        if (row["entry"] != owner + ".class" or
                any(ord(ch) < 32 for ch in owner) or not 45 <= row["version"] <= 65):
            reasons.append(fail("INVALID_CLASS_ENTRY", "invalid class entry name or Java version", jar, row["entry"]))
        if owner.startswith(RUNTIME_RESERVED):
            reasons.append(fail("RESERVED_CLASS", "mod defines a class in a host package", jar, owner))
        if any(ref.startswith("org/spongepowered/asm/") for ref in row["refs"]):
            reasons.append(fail("UNSUPPORTED_RUNTIME_MIXIN", "class references Mixin", jar, owner))
        if owner in excluded:
            continue
        if row["client_member"]:
            reasons.append(fail("CLIENT_ONLY_MEMBER", "runtime cannot strip client-only members", jar, owner))
        for ref in sorted(set(row["refs"]) & excluded):
            reasons.append(fail("CLIENT_ONLY_REFERENCE", "server class references " + ref, jar, owner))
        if any(ref.startswith("net/megu/probe/") for ref in row["refs"]):
            reasons.append(fail("INTERNAL_RUNTIME_REFERENCE", "class references probe internals", jar, owner))
    return sorted(excluded & classes)


def audit(args, classes_surface, tmp, classes, cp):
    methods = {owner: set(spec.get("methods", [])) for owner, spec in classes_surface.items()}
    fields = {owner: set(spec.get("fields", [])) for owner, spec in classes_surface.items()}
    java = str(args.java_home / "bin/java") if args.java_home else "java"
    records, plan, mod_ids = [], [], set()
    for index, path in enumerate(args.jars):
        path = path.resolve()
        jar = f"{index}.jar" if args.runtime_plan else str(path)
        record = {"jar": jar, "sha256": None, "status": "REJECTED", "supported_classes": [],
                  "unsupported_classes": [], "supported_members": [], "unsupported_members": [],
                  "client_classes": [], "diagnostics": []}
        records.append(record)
        reasons = record["diagnostics"]
        if not path.is_file() or path.stat().st_size > MAX_JAR:
            reasons.append(fail("INVALID_JAR", "missing or exceeds size limit", jar))
            continue
        record["sha256"] = digest(path.read_bytes())
        try:
            with ZipFile(path) as zf:
                names = zf.namelist()
                raw, metadata_reasons, client_mixins = metadata_for(jar, zf, names)
        except Exception as exc:
            reasons.append(fail("INVALID_JAR", str(exc), jar))
            continue
        reasons.extend(metadata_reasons)
        if args.runtime_plan:
            entries = runtime_metadata(raw, names, reasons, jar)
            mod_id = raw.get("id")
            if isinstance(mod_id, str) and re.fullmatch(r"[a-z][a-z0-9_-]{1,63}", mod_id):
                if mod_id in mod_ids:
                    reasons.append(fail("DUPLICATE_MOD_ID", mod_id, jar))
                mod_ids.add(mod_id)
            else:
                reasons.append(fail("INVALID_MOD_ID", "expected a Fabric mod identifier", jar))
        jar_classes = {n[:-6] for n in names if n.endswith(".class")}
        command = [java, "-Djava.io.tmpdir=" + str(tmp), "-XX:-UsePerfData", "-cp",
                   os.pathsep.join([str(classes), cp]), "net.megu.probe.FabricShimCheck", str(path)]
        scan = subprocess.run(command, text=True, capture_output=True)
        if scan.returncode != 0:
            reasons.append(fail("INVALID_CLASS_FILE", "ASM could not scan archive class entries", jar))
            continue
        rows = sorted((json.loads(line) for line in scan.stdout.splitlines()), key=lambda r: r["class"])
        if args.runtime_plan:
            excluded = runtime_classes(rows, entries, reasons, jar)
            plan.append(f"jar\t{mod_id}\t{record['sha256']}")
            plan.extend(f"skip\t{name.replace('/', '.')}" for name in excluded)
            for kind in ("main", "server"):
                plan.extend(f"{kind}\t{name.replace('/', '.')}" for name in entries[kind])
        else:
            excluded = sorted({r["class"] for r in rows if r["client"]} | client_mixins)
        record["client_classes"] = excluded
        excluded = set(excluded)
        supported, unsupported, refs = set(), set(), set()
        for row in rows:
            owner = row["class"]
            if owner in excluded or owner not in jar_classes:
                continue
            for ref in row["refs"]:
                if ref.startswith(PLATFORM_PREFIXES):
                    refs.add(ref)
                    if ref not in classes_surface and ref not in jar_classes:
                        unsupported.add((ref, "<class>", ""))
            for annotation in row["annotations"]:
                for injection in ("Redirect", "ModifyVariable", "Overwrite"):
                    package = "" if injection == "Overwrite" else "injection/"
                    if f"Lorg/spongepowered/asm/mixin/{package}{injection};" in annotation:
                        reasons.append(fail("UNSUPPORTED_MIXIN_INJECTION", f"@{injection} is unsupported", jar, owner))
                if ("Lorg/spongepowered/asm/mixin/injection/At;" in annotation and
                        ("INVOKE" in annotation or "STORE" in annotation)):
                    reasons.append(fail("UNSUPPORTED_MIXIN_AT", "nested @At(INVOKE/STORE) is unsupported", jar, owner))
                if annotation.startswith(("MEMBER|", "FIELD|")):
                    kind, sym_owner, name, desc = annotation.split("|", 3)
                    if sym_owner.startswith(PLATFORM_PREFIXES):
                        triple = (sym_owner, name, desc)
                        allowed = (name + desc in methods.get(sym_owner, set()) if kind == "MEMBER"
                                   else name + ":" + desc in fields.get(sym_owner, set()))
                        (supported if allowed else unsupported).add(triple)
        for symbol in sorted(unsupported):
            reasons.append(fail("UNSUPPORTED_SYMBOL", "symbol absent from declared shim surface", jar, "|".join(symbol)))

        def members(symbols):
            return [{"owner": owner, "name": name, "descriptor": desc}
                    for owner, name, desc in sorted(symbols) if name != "<class>"]

        record.update(status="REJECTED" if reasons else "COMPATIBLE",
                      supported_classes=sorted(refs & classes_surface.keys()),
                      unsupported_classes=sorted(owner for owner, name, _ in unsupported if name == "<class>"),
                      supported_members=members(supported), unsupported_members=members(unsupported))
        reasons.sort(key=lambda d: (d["code"], d.get("resource", ""), d["message"]))
    rejected = any(r["status"] == "REJECTED" for r in records)
    if args.runtime_plan and not rejected:
        args.runtime_plan.write_text("\n".join(plan) + "\n", encoding="utf-8")
    return {"schema": "megu-fabric-shimcheck-v1", "minecraft": "1.21.1",
            "status": "REJECTED" if rejected else "COMPATIBLE", "jars": records}


def run(args):
    surface = json.loads(args.surface.read_text())
    profiles = surface.get("runtime", {})
    if args.runtime_plan:
        classes_surface = profiles[args.namespace]
    else:
        classes_surface = {}
        for profile in (surface.get("classes", {}), *profiles.values()):
            for owner, spec in profile.items():
                merged = classes_surface.setdefault(owner, {"methods": set(), "fields": set()})
                for kind in merged:
                    merged[kind].update(spec.get(kind, []))
    tmp = Path(os.environ.get("TMPDIR", str(ROOT.parent.parent / "target/tmp"))).resolve()
    tmp.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(prefix="fabric-shimcheck-", dir=tmp) as work:
        classes = Path(work)
        cp = scanner(args.java_home, deps(), tmp, classes)
        return audit(args, classes_surface, tmp, classes, cp)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--surface", type=Path, default=ROOT / "shim-surface.json")
    p.add_argument("--java-home", type=Path)
    p.add_argument("--runtime-plan", type=Path, help=argparse.SUPPRESS)
    p.add_argument("--namespace", choices=["named", "intermediary"], help=argparse.SUPPRESS)
    p.add_argument("jars", nargs="+", type=Path)
    args = p.parse_args()
    try:
        report = run(args)
    except Exception as exc:
        print(json.dumps({"schema": "megu-fabric-shimcheck-v1", "status": "REJECTED", "error": str(exc)}, sort_keys=True, separators=(",", ":")))
        return 1
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0 if report["status"] == "COMPATIBLE" else 2


if __name__ == "__main__":
    sys.exit(main())
