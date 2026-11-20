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
from urllib.request import urlopen
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parent
SCANNER = ROOT / "java/net/mcspine/probe/FabricShimCheck.java"
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


def scanner(java_home, asm, tmpdir):
    build = ROOT / ".shimcheck-build"
    classes = build / "classes"
    classes.mkdir(parents=True, exist_ok=True)
    cp = os.pathsep.join(map(str, asm))
    javac = str(java_home / "bin/javac") if java_home else "javac"
    command = [javac, "-J-Djava.io.tmpdir=" + str(tmpdir), "-J-XX:-UsePerfData", "--release", "21", "-proc:none", "-cp", cp, "-d", str(classes), str(SCANNER)]
    subprocess.run(command, check=True, env={**os.environ, "TMPDIR": str(tmpdir)})
    return classes, cp


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


def run(args):
    surface = json.loads(args.surface.read_text())
    classes_surface = surface.get("classes", {})
    methods = {owner: set(spec.get("methods", [])) for owner, spec in classes_surface.items()}
    fields = {owner: set(spec.get("fields", [])) for owner, spec in classes_surface.items()}
    jars = [Path(p).resolve() for p in args.jars]
    records = []
    tmp = Path(os.environ.get("TMPDIR", str(ROOT.parent.parent / "target/tmp"))).resolve()
    tmp.mkdir(parents=True, exist_ok=True)
    asm = deps()
    classes, cp = scanner(args.java_home, asm, tmp)
    for path in jars:
        reasons, supported, unsupported, client_classes, refs = [], [], [], [], []
        if not path.is_file() or path.stat().st_size > MAX_JAR:
            records.append({"jar": str(path), "sha256": None, "status": "REJECTED", "supported_classes": [], "unsupported_classes": [], "supported_members": [], "unsupported_members": [], "client_classes": [], "diagnostics": [fail("INVALID_JAR", "missing or exceeds size limit", str(path))]}); continue
        payload = path.read_bytes()
        try:
            with ZipFile(path) as zf:
                names = zf.namelist(); _, reasons, client_mixins = metadata_for(path, zf, names)
        except Exception as exc:
            records.append({"jar": str(path), "sha256": digest(payload), "status": "REJECTED", "supported_classes": [], "unsupported_classes": [], "supported_members": [], "unsupported_members": [], "client_classes": [], "diagnostics": [fail("INVALID_JAR", str(exc), str(path))]}); continue
        jar_classes = {n[:-6] for n in names if n.endswith(".class")}
        command = [str((args.java_home / "bin/java") if args.java_home else Path("java")), "-Djava.io.tmpdir=" + str(tmp), "-XX:-UsePerfData", "-cp", os.pathsep.join([str(classes), cp]), "net.mcspine.probe.FabricShimCheck", str(path)]
        scan = subprocess.run(command, check=True, text=True, capture_output=True, env={**os.environ, "TMPDIR": str(tmp)})
        jar_rows = [json.loads(line) for line in scan.stdout.splitlines()]
        for row in sorted((r for r in jar_rows if r["class"] in jar_classes), key=lambda r: r["class"]):
            owner = row["class"]
            if row["client"] or owner in client_mixins:
                client_classes.append(owner)
                continue
            for ref in row["refs"]:
                if ref.startswith("net/minecraft/class_") or ref.startswith("net/fabricmc/"):
                    refs.append(ref)
                    if ref not in classes_surface and ref not in jar_classes:
                        unsupported.append({"owner": ref, "name": "<class>", "descriptor": ""})
            for annotation in row["annotations"]:
                if "Lorg/spongepowered/asm/mixin/injection/Redirect;" in annotation:
                    reasons.append(fail("UNSUPPORTED_MIXIN_INJECTION", "@Redirect is unsupported", str(path), owner))
                if "Lorg/spongepowered/asm/mixin/injection/ModifyVariable;" in annotation:
                    reasons.append(fail("UNSUPPORTED_MIXIN_INJECTION", "@ModifyVariable is unsupported", str(path), owner))
                if "Lorg/spongepowered/asm/mixin/Overwrite;" in annotation:
                    reasons.append(fail("UNSUPPORTED_MIXIN_INJECTION", "@Overwrite is unsupported", str(path), owner))
                if ("Lorg/spongepowered/asm/mixin/injection/At;" in annotation and
                    ("INVOKE" in annotation or "STORE" in annotation)):
                    reasons.append(fail("UNSUPPORTED_MIXIN_AT", "nested @At(INVOKE/STORE) is unsupported", str(path), owner))
                if annotation.startswith("MEMBER|"):
                    _, sym_owner, name, desc = annotation.split("|", 3)
                    if sym_owner.startswith("net/minecraft/class_") or sym_owner.startswith("net/fabricmc/"):
                        triple = {"owner": sym_owner, "name": name, "descriptor": desc}
                        (supported if name + desc in methods.get(sym_owner, set()) else unsupported).append(triple)
                elif annotation.startswith("FIELD|"):
                    _, sym_owner, name, desc = annotation.split("|", 3)
                    if sym_owner.startswith("net/minecraft/class_") or sym_owner.startswith("net/fabricmc/"):
                        triple = {"owner": sym_owner, "name": name, "descriptor": desc}
                        (supported if name + ":" + desc in fields.get(sym_owner, set()) else unsupported).append(triple)
        for ref in sorted(set(refs)):
            if ref in classes_surface: supported.append({"owner": ref, "name": "<class>", "descriptor": ""})
        if unsupported:
            for symbol in sorted({(x["owner"], x["name"], x["descriptor"]) for x in unsupported}):
                reasons.append(fail("UNSUPPORTED_SYMBOL", "symbol absent from declared shim surface", str(path), "|".join(symbol)))
        records.append({"jar": str(path), "sha256": digest(payload), "status": "REJECTED" if reasons else "COMPATIBLE",
                        "supported_classes": sorted({x["owner"] for x in supported if x["name"] == "<class>"}),
                        "unsupported_classes": sorted({x["owner"] for x in unsupported if x["name"] == "<class>"}),
                        "supported_members": sorted([x for x in supported if x["name"] != "<class>"], key=lambda x: (x["owner"],x["name"],x["descriptor"])),
                        "unsupported_members": sorted([x for x in unsupported if x["name"] != "<class>"], key=lambda x: (x["owner"],x["name"],x["descriptor"])),
                        "client_classes": sorted(set(client_classes)), "diagnostics": sorted(reasons, key=lambda x: (x.get("code", ""), x.get("resource", ""), x.get("message", "")))})
    status = "REJECTED" if any(r["status"] == "REJECTED" for r in records) else "COMPATIBLE"
    return {"schema": "mc-spine-fabric-shimcheck-v1", "minecraft": "1.21.1", "status": status, "jars": records}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--surface", type=Path, default=ROOT / "shim-surface.json")
    p.add_argument("--java-home", type=Path)
    p.add_argument("jars", nargs="+", type=Path)
    args = p.parse_args()
    try:
        report = run(args)
    except Exception as exc:
        print(json.dumps({"schema": "mc-spine-fabric-shimcheck-v1", "status": "REJECTED", "error": str(exc)}, sort_keys=True, separators=(",", ":")))
        return 1
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0 if report["status"] == "COMPATIBLE" else 2


if __name__ == "__main__":
    sys.exit(main())
