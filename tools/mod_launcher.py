#!/usr/bin/env python3
"""4-tier Mod Startup Classifier & Fallback Launcher.

Mirrors `mod-adapter-core/src/fallback.rs` (`plan_mod_startup`) and
`specs/mod-compatibility/universal-mod-adapter.md`: every JAR, pure-data
zip, or datapack directory in `--mods-dir` is classified into exactly one
execution tier, then one of three fallback policies decides whether to
launch `mc-spine` natively, quarantine unsupported mods, or delegate to a
pinned upstream NeoForge 1.21.1 runtime.

  Tier 0 NativeFlagship   : mod_id in --flagship-mods (dedicated Rust adapter, zero JVM)
  Tier 1 UniversalNative  : pure-data (no .class bytecode) or mod_id has a
                            declarative offload profile in --profiles-dir (zero JVM)
  Tier 2 BoundedJvmShim   : Java mod verified COMPATIBLE by ShimCheck
  Tier 3 RequiresFallback : Java mod rejected by ShimCheck (Mixin/AT/missing API)

Fallback policies when >= 1 Tier 3 mod exists:

  strict     : refuse startup before binding TCP (exit 2) with reason codes
  quarantine : run Tier 0/1/2 natively; refuse (exit 2) if a Tier 3 mod's
               namespace exists in the saved world unless
               --force-drop-saved-blocks; otherwise stage active mods
               (plus optional data-only extracts with --quarantine-load-data)
               and launch --mc-spine-cmd
  delegate   : hand off to --neoforge-fallback-cmd with exact triggering
               mods + reason codes logged (0 Tier 3 mods => pure native,
               NeoForge is never started)

Exit status: 0 launched (or --dry-run plan computed), 1 launcher error,
2 startup refused / delegation impossible.
"""

import argparse
import gzip
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SHIMCHECK = ROOT / "tools" / "shimcheck.py"
DEFAULT_STAGING = ROOT / "target" / "tmp" / "mod-launcher-staging"
MAX_SAVE_FILE_BYTES = 512 * 1024 * 1024
MAX_SAVE_FILE_COUNT = 4096
MAX_DATA_ENTRY_BYTES = 8 * 1024 * 1024

TIER_NAMES = {
    0: "NativeFlagship",
    1: "UniversalNative",
    2: "BoundedJvmShim",
    3: "RequiresFallback",
}

# Exact strings from mod-adapter-core/src/fallback.rs so CLI output matches
# the Rust startup report 1:1.
REASON_FLAGSHIP = "handled by flagship native Rust adapter"
REASON_UNIVERSAL = "handled by zero-JVM universal adapter / offload profile"
REASON_SHIM = "verified compatible with bounded S0-S4 JVM shim"
REASON_FALLBACK_DEFAULT = "unsupported Mixin/AT or Minecraft API surface"
REASON_REFUSE_STRICT = "strict policy rejected unsupported Tier 3 mod(s)"
REASON_REFUSE_QUARANTINE = (
    "refusing to quarantine mod(s) whose blocks exist in the saved world; "
    "switch to delegate fallback or set force_drop_saved_blocks=true"
)

CODE_MISSING_SHIMCHECK = "MISSING_SHIMCHECK_REPORT"
CODE_UNSCANNABLE = "UNSCANNABLE_JAVA_MOD"

IDENTIFIER = re.compile(rb"[a-z0-9_.\-]{1,64}:[a-z0-9_/.\-]{1,128}")
TABLE_HEADER = re.compile(r"^\s*\[\[?\s*([^\]]+)\s*\]?\]")
MODID_LINE = re.compile(r"""^\s*modId\s*=\s*['"]([^'"]+)['"]""")


class LauncherError(Exception):
    """Operational failure (bad input, unreadable tool output)."""


def _entries_of(source):
    """Yield (name, reader) for a zip/jar or a datapack directory."""
    if source.is_dir():
        base = source
        for path in sorted(p for p in base.rglob("*") if p.is_file()):
            rel = path.relative_to(base).as_posix()
            yield rel, lambda p=path: p.read_bytes()
        return
    try:
        with zipfile.ZipFile(source) as archive:
            for name in sorted(archive.namelist()):
                if name.endswith("/"):
                    continue
                yield name, lambda n=name: archive.read(n)
    except zipfile.BadZipFile as error:
        raise LauncherError(f"not a readable jar/zip: {source} ({error})")


def _mod_ids_from_toml(text):
    """Collect `modId` values from `[[mods]]` tables only."""
    try:
        import tomllib

        document = tomllib.loads(text)
        mods = document.get("mods")
        if isinstance(mods, list):
            ids = [m["modId"] for m in mods
                   if isinstance(m, dict) and isinstance(m.get("modId"), str)]
            if ids:
                return ids
    except Exception:
        pass
    ids = []
    in_mods = False
    for line in text.splitlines():
        header = TABLE_HEADER.match(line)
        if header:
            in_mods = header.group(1).strip() == "mods" and "[[" in line
            continue
        if in_mods:
            match = MODID_LINE.match(line)
            if match:
                ids.append(match.group(1))
    return ids


class DiscoveredMod:
    """Audit summary of one mods/ entry (mirrors `DiscoveredMod` in fallback.rs)."""

    def __init__(self, source):
        self.source = source
        self.has_java_bytecode = False
        self.shimcheck_compatible = False
        self.has_unsupported_mixin_or_at = False
        # List of {"code", "symbol", "detail"} dicts (ShimCheck reasons or
        # launcher-generated codes when ShimCheck never ran on this entry).
        self.rejection_reasons = []
        declared = []
        namespaces = []
        toml_text = None
        for name, read in _entries_of(source):
            lowered = name.lower()
            if lowered.endswith(".class"):
                self.has_java_bytecode = True
            if lowered in ("meta-inf/neoforge.mods.toml", "meta-inf/mods.toml"):
                try:
                    toml_text = read().decode("utf-8", errors="replace")
                except OSError:
                    pass
            elif lowered == "fabric.mod.json":
                try:
                    doc = json.loads(read())
                    if isinstance(doc.get("id"), str):
                        declared.append(doc["id"])
                except (OSError, ValueError):
                    pass
            elif lowered == "quilt.mod.json":
                try:
                    doc = json.loads(read())
                    loader = doc.get("quilt_loader", {})
                    if isinstance(loader.get("id"), str):
                        declared.append(loader["id"])
                except (OSError, ValueError):
                    pass
            parts = name.split("/")
            if len(parts) >= 3 and parts[0] == "data":
                namespaces.append(parts[1])
        if toml_text is not None:
            declared = _mod_ids_from_toml(toml_text) + declared
        unique_ns = sorted(set(namespaces))
        if declared:
            self.mod_ids = sorted(set(declared))
        elif self.has_java_bytecode:
            self.mod_ids = [source.stem]
        elif unique_ns:
            # A pure-data pack registers content only under its data/
            # namespaces, so those are exactly what the saved-world guard
            # must compare against.
            self.mod_ids = unique_ns
        else:
            self.mod_ids = [source.stem if source.is_file() else source.name]
        self.mod_id = self.mod_ids[0]

    def reject(self, code, symbol, detail):
        self.rejection_reasons.append({"code": code, "symbol": symbol, "detail": detail})

    def reason_codes(self):
        return [r["code"] for r in self.rejection_reasons]


def discover_mods(mods_dir):
    if not mods_dir.is_dir():
        raise LauncherError(f"--mods-dir is not a directory: {mods_dir}")
    entries = [
        p for p in sorted(mods_dir.iterdir(), key=lambda p: p.name)
        if not p.name.startswith(".")
        and (p.is_dir() or p.suffix.lower() in (".jar", ".zip"))
    ]
    return [DiscoveredMod(p) for p in entries]


def load_profiles(profiles_dir):
    """Return ({mod_id: profile_path}, warnings) for declarative offload profiles."""
    ids = {}
    warnings = []
    if profiles_dir is None:
        return ids, warnings
    if not profiles_dir.is_dir():
        raise LauncherError(f"--profiles-dir is not a directory: {profiles_dir}")
    for path in sorted(profiles_dir.glob("*.json")):
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
            mod_id = doc.get("mod_id")
            if isinstance(mod_id, str) and mod_id:
                ids.setdefault(mod_id, path)
            else:
                warnings.append(f"{path.name}: missing string 'mod_id', ignored")
        except (OSError, ValueError) as error:
            warnings.append(f"{path.name}: invalid profile JSON ({error}), ignored")
    return ids, warnings


def _scan_bytes_for_namespaces(data):
    return {m.group(0).split(b":", 1)[0].decode("ascii") for m in IDENTIFIER.finditer(data)}


def saved_namespaces_from_path(path):
    """Extract mod namespaces from a saved world (dir, .dat/.mca, or json/text)."""
    namespaces = set()
    if path.is_dir():
        candidates = [
            p for p in sorted(path.rglob("*"))
            if p.is_file() and p.suffix.lower() in (".dat", ".mca", ".nbt", ".json", ".txt")
        ]
    elif path.is_file():
        candidates = [path]
    else:
        raise LauncherError(f"--save-file does not exist: {path}")
    if len(candidates) > MAX_SAVE_FILE_COUNT:
        candidates = candidates[:MAX_SAVE_FILE_COUNT]
    for candidate in candidates:
        try:
            if candidate.stat().st_size > MAX_SAVE_FILE_BYTES:
                continue
            data = candidate.read_bytes()
        except OSError:
            continue
        if data[:2] == b"\x1f\x8b":
            try:
                data = gzip.decompress(data)
            except (OSError, EOFError):
                continue
            if len(data) > MAX_SAVE_FILE_BYTES:
                continue
        namespaces.update(_scan_bytes_for_namespaces(data))
    return namespaces


def _shimcheck_command(args, jars):
    raw = args.shimcheck_cmd or str(DEFAULT_SHIMCHECK)
    command = shlex.split(raw)
    if not command:
        raise LauncherError("--shimcheck-cmd is empty")
    if command[0].endswith(".py"):
        command = [sys.executable] + command
    return command + ["--json"] + [str(j) for j in jars]


def run_shimcheck(jars, args):
    """Return {normalized_jar_path: jar_report} for every scanned jar."""
    if not jars:
        return {}
    if args.shimcheck_json is not None:
        try:
            report = json.loads(args.shimcheck_json.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise LauncherError(f"cannot read --shimcheck-json: {error}")
    else:
        command = _shimcheck_command(args, jars)
        try:
            proc = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
        except OSError as error:
            raise LauncherError(f"shimcheck failed to start: {error}")
        if proc.returncode not in (0, 2) or not proc.stdout.strip():
            raise LauncherError(
                f"shimcheck failed (exit {proc.returncode}): {proc.stderr.strip()}"
            )
        try:
            report = json.loads(proc.stdout)
        except ValueError as error:
            raise LauncherError(f"shimcheck emitted invalid JSON: {error}")
    reports = {}
    for item in report.get("jars", []):
        jar = item.get("jar")
        if isinstance(jar, str):
            reports[os.path.normpath(jar)] = item
            reports[os.path.normpath(os.path.abspath(jar))] = item
    return reports


def _report_for(reports, path):
    return (
        reports.get(os.path.normpath(str(path)))
        or reports.get(os.path.normpath(str(path.resolve())))
        or reports.get(os.path.normpath(path.name))
    )


def apply_shimcheck(mods, reports):
    """Fill ShimCheck verdicts into bytecode mods; fail closed when absent."""
    for mod in mods:
        if not mod.has_java_bytecode:
            continue
        if mod.source.is_dir():
            mod.reject(CODE_UNSCANNABLE, str(mod.source),
                       "unpacked Java mod cannot be ShimCheck-verified")
            continue
        item = _report_for(reports, mod.source)
        if item is None:
            mod.reject(CODE_MISSING_SHIMCHECK, str(mod.source),
                       "no ShimCheck report for this jar")
            continue
        reasons = item.get("reasons", [])
        for reason in reasons:
            mod.reject(str(reason.get("code", "UNKNOWN")),
                       str(reason.get("symbol", "")),
                       str(reason.get("detail", "")))
        mod.shimcheck_compatible = item.get("status") == "COMPATIBLE" and not reasons
        mod.has_unsupported_mixin_or_at = any(
            "MIXIN" in r["code"] or "ACCESS_TRANSFORMER" in r["code"]
            for r in mod.rejection_reasons
        )


class Assignment:
    def __init__(self, mod, tier, reasons):
        self.mod_id = mod.mod_id
        self.mod_ids = list(mod.mod_ids)
        self.tier = tier
        self.reasons = reasons
        self.source = str(mod.source)

    def as_dict(self):
        return {
            "mod_id": self.mod_id,
            "tier": self.tier,
            "tier_name": TIER_NAMES[self.tier],
            "source": self.source,
            "reasons": self.reasons,
        }


def classify(mod, flagship_ids, profile_ids):
    """Exact port of the tier cascade in fallback.rs `plan_mod_startup`."""
    owned = set(mod.mod_ids)
    if owned & flagship_ids:
        return 0, [REASON_FLAGSHIP]
    if not mod.has_java_bytecode or owned & profile_ids:
        return 1, [REASON_UNIVERSAL]
    if mod.shimcheck_compatible and not mod.has_unsupported_mixin_or_at:
        return 2, [REASON_SHIM]
    reasons = mod.reason_codes() or [REASON_FALLBACK_DEFAULT]
    return 3, reasons


def plan_startup(mods, flagship_ids, profile_ids, saved_namespaces, policy,
                 load_safe_data_only=False, force_drop_saved_blocks=False):
    """Exact port of `plan_mod_startup`; returns a dict plan."""
    assignments = []
    tier3 = []
    needs_bounded_jvm = False
    for mod in mods:
        tier, reasons = classify(mod, flagship_ids, profile_ids)
        if tier == 2:
            needs_bounded_jvm = True
        assignment = Assignment(mod, tier, reasons)
        assignments.append(assignment)
        if tier == 3:
            tier3.append(assignment)

    if not tier3:
        return {
            "plan": "RunNative",
            "assignments": assignments,
            "needs_bounded_jvm": needs_bounded_jvm,
            "quarantined_mods": [],
            "load_quarantined_data_only": False,
        }
    if policy == "strict":
        return {
            "plan": "RefuseStartup",
            "reason": REASON_REFUSE_STRICT,
            "blocking_mods": tier3,
            "assignments": assignments,
        }
    if policy == "quarantine":
        if not force_drop_saved_blocks:
            endangered = [a for a in tier3 if set(a.mod_ids) & saved_namespaces]
            if endangered:
                return {
                    "plan": "RefuseStartup",
                    "reason": REASON_REFUSE_QUARANTINE,
                    "blocking_mods": endangered,
                    "assignments": assignments,
                }
        return {
            "plan": "RunNative",
            "assignments": [a for a in assignments if a.tier != 3],
            "needs_bounded_jvm": needs_bounded_jvm,
            "quarantined_mods": tier3,
            "load_quarantined_data_only": load_safe_data_only,
        }
    if policy == "delegate":
        return {
            "plan": "DelegateToNeoForge",
            "triggering_mods": tier3,
            "assignments": assignments,
        }
    raise LauncherError(f"unknown fallback policy: {policy}")


def _reset_staging(path):
    """Empty a launcher-owned staging dir. Never rm -rf; only unlink contents."""
    if not path.exists():
        path.mkdir(parents=True)
        return
    resolved = path.resolve()
    for forbidden in (Path("/"), Path.home(), ROOT):
        if resolved == forbidden:
            raise LauncherError(f"refusing to stage into {resolved}")
    for entry in sorted(path.rglob("*"), key=lambda p: len(p.parts), reverse=True):
        if entry.is_symlink() or entry.is_file():
            entry.unlink()
        elif entry.is_dir():
            entry.rmdir()


def stage_mods(plan, staging_dir, mods_dir, log):
    """Stage active mods (+ optional quarantined data-only extracts)."""
    resolved = staging_dir.resolve()
    mods_resolved = mods_dir.resolve()
    if (resolved == mods_resolved or resolved in mods_resolved.parents
            or mods_resolved in resolved.parents):
        raise LauncherError("--staging-dir must not overlap --mods-dir")
    _reset_staging(staging_dir)
    active_dir = staging_dir / "mods"
    active_dir.mkdir(parents=True, exist_ok=True)
    for assignment in plan["assignments"]:
        source = Path(assignment.source)
        target = active_dir / source.name
        if source.is_dir():
            shutil.copytree(source, target)
        else:
            shutil.copy2(source, target)
    quarantined_dir = staging_dir / "quarantined_data"
    quarantine_report = []
    for assignment in plan["quarantined_mods"]:
        quarantine_report.append(assignment.as_dict())
        if not plan["load_quarantined_data_only"]:
            continue
        source = Path(assignment.source)
        target_root = quarantined_dir / assignment.mod_id
        try:
            for name, read in _entries_of(source):
                if not name.startswith("data/") or not name.endswith(".json"):
                    continue
                destination = target_root / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                payload = read()
                if len(payload) > MAX_DATA_ENTRY_BYTES:
                    log(f"  skip oversized data entry {name} in {source.name}")
                    continue
                destination.write_bytes(payload)
        except LauncherError as error:
            log(f"  {error}")
    (staging_dir / "quarantine_report.json").write_text(
        json.dumps({"quarantined_mods": quarantine_report}, indent=2) + "\n",
        encoding="utf-8")
    return active_dir


def _expand_command(command, mods_dir, staging_dir):
    argv = []
    for token in shlex.split(command):
        argv.append(token.replace("{mods_dir}", str(mods_dir))
                    .replace("{staging_dir}", str(staging_dir)))
    if argv and argv[0].endswith(".py"):
        argv = [sys.executable] + argv
    return argv


def _child_env(plan, mods_dir, staging_dir, policy):
    env = dict(os.environ)
    env["MC_SPINE_LAUNCHER_PLAN"] = plan["plan"]
    env["MC_SPINE_FALLBACK_POLICY"] = policy
    env["MC_SPINE_MODS_DIR"] = str(mods_dir)
    env["MC_SPINE_STAGING_DIR"] = str(staging_dir)
    env["MC_SPINE_NEEDS_BOUNDED_JVM"] = (
        "true" if plan.get("needs_bounded_jvm") else "false")
    env["MC_SPINE_QUARANTINED_MODS"] = ",".join(
        a.mod_id for a in plan.get("quarantined_mods", []))
    env["MC_SPINE_TIER3_MODS"] = ",".join(
        a.mod_id for a in plan.get("triggering_mods", []))
    return env


def _run_command(argv, env):
    try:
        proc = subprocess.run(argv, cwd=ROOT, env=env, check=False)
    except OSError as error:
        raise LauncherError(f"failed to launch {argv[0]}: {error}")
    return proc.returncode


def build_report(args, plan, launch):
    return {
        "schema": "mc-spine-mod-launcher/1",
        "mods_dir": str(args.mods_dir),
        "policy": args.fallback,
        "plan": plan["plan"],
        "needs_bounded_jvm": bool(plan.get("needs_bounded_jvm")),
        "assignments": [a.as_dict() for a in plan["assignments"]],
        "quarantined_mods": [a.as_dict() for a in plan.get("quarantined_mods", [])],
        "triggering_mods": [a.as_dict() for a in plan.get("triggering_mods", [])],
        "blocking_mods": [a.as_dict() for a in plan.get("blocking_mods", [])],
        "refusal_reason": plan.get("reason"),
        "saved_world_namespaces": sorted(plan.get("saved_namespaces", [])),
        "staging_dir": str(plan.get("staging_dir") or ""),
        "dry_run": bool(args.dry_run),
        "launch": launch,
    }


def _print_table(assignments):
    rows = [(a["mod_id"], f"Tier {a['tier']} {a['tier_name']}",
             Path(a["source"]).name, "; ".join(a["reasons"]))
            for a in assignments]
    width = [max(len(r[i]) for r in rows + [("mod_id", "tier", "source", "reasons")])
             for i in range(4)]
    print("  " + "  ".join(h.ljust(width[i]) for i, h in
                           enumerate(("mod_id", "tier", "source", "reasons"))))
    print("  " + "  ".join("-" * width[i] for i in range(4)))
    for row in rows:
        print("  " + "  ".join(row[i].ljust(width[i]) for i in range(4)))


def _print_human(report):
    print("== mc-spine mod startup classification ==")
    print(f"policy      : {report['policy']}")
    print(f"mods_dir    : {report['mods_dir']}")
    if report["saved_world_namespaces"]:
        print(f"saved ns    : {','.join(report['saved_world_namespaces'])}")
    _print_table(report["assignments"])
    plan = report["plan"]
    if plan == "RunNative":
        suffix = ""
        if report["quarantined_mods"]:
            suffix = (" (quarantined: "
                      + ", ".join(a["mod_id"] for a in report["quarantined_mods"]) + ")")
        print(f"plan        : RunNative needs_bounded_jvm="
              f"{str(report['needs_bounded_jvm']).lower()}{suffix}")
    elif plan == "DelegateToNeoForge":
        print("plan        : DelegateToNeoForge")
        for a in report["triggering_mods"]:
            print(f"  trigger   : {a['mod_id']} -> {', '.join(a['reasons'])}")
    else:
        print("plan        : RefuseStartup")
        print(f"  reason    : {report['refusal_reason']}")
        for a in report["blocking_mods"]:
            print(f"  blocking  : {a['mod_id']} -> {', '.join(a['reasons'])}")
    launch = report["launch"]
    if launch.get("command"):
        prefix = "would launch" if report["dry_run"] or not launch.get("executed") else "launching"
        print(f"{prefix}: {' '.join(launch['command'])}")
    if launch.get("executed") and launch.get("exit_code") is not None:
        print(f"launch exit : {launch['exit_code']}")


def main():
    parser = argparse.ArgumentParser(
        description="4-tier mod classifier + strict/quarantine/delegate fallback launcher")
    parser.add_argument("--mods-dir", type=Path, required=True,
                        help="directory of mod .jar/.zip files and datapack dirs")
    parser.add_argument("--profiles-dir", type=Path,
                        help="directory of declarative offload profile JSONs")
    parser.add_argument("--world-namespaces", type=str, default="",
                        help="comma-separated namespaces present in the saved world")
    parser.add_argument("--save-file", type=Path,
                        help="saved world file/dir scanned for namespaces")
    parser.add_argument("--flagship-mods", type=str, default="create",
                        help="comma-separated flagship native mod ids (default: create)")
    parser.add_argument("--fallback", choices=("strict", "quarantine", "delegate"),
                        default="delegate", help="Tier 3 fallback policy (default: delegate)")
    parser.add_argument("--quarantine-load-data", action="store_true",
                        help="extract data-only JSON from quarantined mods into staging")
    parser.add_argument("--force-drop-saved-blocks", action="store_true",
                        help="allow quarantining mods whose blocks exist in the save")
    parser.add_argument("--shimcheck-cmd", type=str,
                        default=str(DEFAULT_SHIMCHECK),
                        help="ShimCheck command for Java mods (default: tools/shimcheck.py)")
    parser.add_argument("--shimcheck-json", type=Path,
                        help="precomputed ShimCheck JSON report (offline/CI)")
    parser.add_argument("--mc-spine-cmd", type=str,
                        help="native mc-spine launch command ({mods_dir} {staging_dir} expand)")
    parser.add_argument("--neoforge-fallback-cmd", type=str,
                        help="pinned upstream NeoForge 1.21.1 server command")
    parser.add_argument("--staging-dir", type=Path, default=DEFAULT_STAGING,
                        help="quarantine staging directory (default: target/tmp/mod-launcher-staging)")
    parser.add_argument("--dry-run", action="store_true",
                        help="compute and print the plan without launching anything")
    parser.add_argument("--json", action="store_true", help="emit deterministic JSON report")
    args = parser.parse_args()

    def log(message):
        print(message, file=sys.stderr if args.json else sys.stdout)

    try:
        mods = discover_mods(args.mods_dir)
        profile_ids, warnings = load_profiles(args.profiles_dir)
        for warning in warnings:
            log(f"profiles: {warning}")
        saved = {ns for ns in args.world_namespaces.split(",") if ns}
        if args.save_file is not None:
            saved |= saved_namespaces_from_path(args.save_file)
        flagship_ids = {s for s in args.flagship_mods.split(",") if s}

        bytecode_jars = [m.source for m in mods
                         if m.has_java_bytecode and m.source.is_file()]
        reports = run_shimcheck(bytecode_jars, args)
        apply_shimcheck(mods, reports)

        plan = plan_startup(
            mods, flagship_ids, set(profile_ids), saved, args.fallback,
            load_safe_data_only=args.quarantine_load_data,
            force_drop_saved_blocks=args.force_drop_saved_blocks)
        plan["saved_namespaces"] = saved

        launch = {"kind": None, "command": [], "executed": False,
                  "exit_code": None, "dry_run": bool(args.dry_run)}
        exit_code = 0
        staging_dir = args.staging_dir
        active_mods_dir = args.mods_dir

        if plan["plan"] == "RefuseStartup":
            exit_code = 2
        elif plan["plan"] == "RunNative":
            plan["staging_dir"] = staging_dir
            if plan["quarantined_mods"] and not args.dry_run:
                active_mods_dir = stage_mods(plan, staging_dir, args.mods_dir, log)
            elif plan["quarantined_mods"]:
                active_mods_dir = staging_dir / "mods"
            command = _expand_command(args.mc_spine_cmd, active_mods_dir, staging_dir) \
                if args.mc_spine_cmd else []
            launch["kind"] = "mc_spine"
            launch["command"] = command
            if not command:
                if not args.dry_run:
                    log("mod-launcher: --mc-spine-cmd is required to launch natively")
                    exit_code = 2
            elif args.dry_run:
                pass
            else:
                env = _child_env(plan, active_mods_dir, staging_dir, args.fallback)
                launch["executed"] = True
                launch["exit_code"] = _run_command(command, env)
                exit_code = launch["exit_code"]
        elif plan["plan"] == "DelegateToNeoForge":
            for trigger in plan["triggering_mods"]:
                log(f"fallback  : {trigger.mod_id} -> {', '.join(trigger.reasons)}")
            command = _expand_command(args.neoforge_fallback_cmd,
                                      active_mods_dir, staging_dir) \
                if args.neoforge_fallback_cmd else []
            launch["kind"] = "neoforge_fallback"
            launch["command"] = command
            if not command:
                if not args.dry_run:
                    log("mod-launcher: Tier 3 mod(s) require upstream NeoForge 1.21.1 "
                        "runtime; pass --neoforge-fallback-cmd (or use --fallback "
                        "quarantine/strict)")
                    exit_code = 2
            elif args.dry_run:
                pass
            else:
                env = _child_env(plan, active_mods_dir, staging_dir, args.fallback)
                launch["executed"] = True
                launch["exit_code"] = _run_command(command, env)
                exit_code = launch["exit_code"]

        report = build_report(args, plan, launch)
        if args.json:
            print(json.dumps(report, indent=2))
        else:
            _print_human(report)
        return exit_code
    except LauncherError as error:
        print(f"mod-launcher: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
