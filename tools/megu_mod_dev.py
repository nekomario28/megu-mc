#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Megu mod developer kit: scaffold, validate, pack, and hot-reload `.mmod` packages.

A native Megu mod is plain data: a `megu-mod.toml` package manifest plus a
`megu-mod-bundle.json` document accepted by
`mod-adapter-core/src/unified_bundle.rs` (`DeclarativeModBundle::from_json`),
with standard datapack `data/` and resource-pack `assets/` trees beside it.
No Gradle, no JDK, no Mixin, and no Java manifest is required. The validator in
this tool mirrors the Rust parser fail-closed rules (schema tag, resource
locations, section caps, unknown keys, version ranges, id uniqueness) so a
bundle that passes here is one `from_json` accepts.

  tools/megu_mod_dev.py init <mod_id> --out <dir> [--template starter|machine|creature|shader]
                                       [--mc-version 1.21.1] [--compat-jar] [--force]
  tools/megu_mod_dev.py validate <dir> [--json]
  tools/megu_mod_dev.py pack <dir> [--out dist/<mod_id>-<version>.mmod] [--deterministic]
                                   [--compat-jar] [--json]
  tools/megu_mod_dev.py watch <dir> --mods-dir <server_mods_dir> [--interval 0.5] [--once]
                                    [--compat-jar] [--json]
  tools/megu_mod_dev.py doctor [--json]

Packages
  `.mmod` (default) is a deterministic ZIP of `megu-mod.toml`,
  `megu-mod-bundle.json`, `pack.mcmeta`, `README.md`, `data/`, `assets/`, and
  `shaderpacks/`. `--compat-jar` additionally emits a `.jar` (default suffix) and
  generates `META-INF/neoforge.mods.toml` + `fabric.mod.json` when the project
  has none, so the same source tree can also be loaded by a Java loader.
  `watch` repacks atomically (`<name>.part` then `os.replace`) into
  `<mods-dir>/<mod_id>-dev.mmod` for restart-free iteration.

AI agent contract
  `validate --json` prints exactly
  `{"ok": bool, "mod_id": str|null, "diagnostics": [{"code", "path",
  "message", "fix_hint", "severity"}]}`. `ok` is true when no diagnostic has
  `severity == "error"`, so an agent can edit only the paths named by the
  diagnostics and re-run the same command to converge. Exit status matches:
  0 clean, 1 errors, 2 usage/root failure.

Safety contract
  * Path traversal is rejected everywhere: ids, manifest `bundle.file`, archive
    members, and symlinks that resolve outside the project root.
  * Executable/native payload extensions (`.exe`, `.dll`, `.so`, `.dylib`,
    `.bat`, `.class`, `.jar`, ...) are flagged, never packed silently.
  * The packer refuses to write outside the requested output path, writes via a
    `.part` sibling, and never follows an escaping symlink.
  * Only the Python standard library is used. No network access, no subprocess
    execution; `doctor` probes binaries with `shutil.which` and runs nothing.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import time
import zipfile
from dataclasses import dataclass
from pathlib import Path

try:  # Python >= 3.11; a bounded fallback parser keeps older Pythons working.
    import tomllib as _tomllib
except ModuleNotFoundError:  # pragma: no cover - exercised only on <=3.10
    _tomllib = None

TOOL_VERSION = "1.0.0"
DIAGNOSTIC_SCHEMA = "megu-mod-dev/1"

# --- Schema tags mirrored from mod-adapter-core -----------------------------
PACKAGE_SCHEMA = "megu-mod/1"
BUNDLE_SCHEMA = "megu-mod-bundle/1"
BUNDLE_FILE = "megu-mod-bundle.json"
MANIFEST_FILE = "megu-mod.toml"

# --- Bounds mirrored from mod-adapter-core/src/unified_bundle.rs ------------
MAX_SECTION_ENTRIES = 1024
MAX_INNER_ENTRIES = 512
MAX_SPELL_STEPS = 16
MAX_EFFECTS_PER_STEP = 8
MAX_RITUAL_PATTERN_BLOCKS = 256
MAX_RITUAL_OFFSET = 16
MAX_REAGENTS = 64
MAX_ATTRIBUTE_MODIFIERS = 16
MAX_DURATION_TICKS = 72_000
MAX_PIERCE_CAP = 64
MAX_AOE_RADIUS = 32.0
MAX_CHAIN_HOPS = 8
MAX_ORBIT_COUNT = 32
MAX_ACTIVE_SUMMONS = 16
MAX_BUNDLE_BYTES = 4 * 1024 * 1024
MAX_JSON_BYTES = 4 * 1024 * 1024
MAX_PROJECT_FILES = 20_000
DEFAULT_BABY_GROWTH_TICKS = 24_000
DEFAULT_BREED_COOLDOWN_TICKS = 6_000

TOP_LEVEL_KEYS = (
    "schema",
    "mod_id",
    "mc_version_range",
    "portals",
    "progression_barriers",
    "biome_regions",
    "creatures",
    "custom_channels",
    "spells",
    "custom_statuses",
    "rituals",
)
REQUIRED_TOP_LEVEL_KEYS = (
    "schema",
    "mod_id",
    "mc_version_range",
    "portals",
    "progression_barriers",
    "biome_regions",
    "creatures",
    "custom_channels",
)

SKIP_DIRS = frozenset(
    {".git", ".vscode", ".idea", ".megu", "schemas", "dist", "target", "node_modules", "__pycache__"}
)
PACK_ROOTS = ("data", "assets", "shaderpacks")
PACK_FILES = (MANIFEST_FILE, BUNDLE_FILE, "pack.mcmeta", "README.md")
UNSAFE_EXTENSIONS = frozenset(
    {
        ".exe",
        ".dll",
        ".so",
        ".dylib",
        ".bat",
        ".cmd",
        ".com",
        ".msi",
        ".scr",
        ".class",
        ".jar",
        ".o",
        ".a",
        ".bin",
        ".elf",
        ".ps1",
        ".sh",
    }
)
SAFE_MOD_ID = re.compile(r"^[a-z][a-z0-9_.-]{0,63}$")
SEMVER = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$")
NAMESPACE = re.compile(r"^[a-z0-9_.-]+$")
PATH_CHARS = re.compile(r"^[a-z0-9_./-]+$")

PACK_FORMATS = {
    "1.20.4": 26,
    "1.20.6": 41,
    "1.21": 48,
    "1.21.0": 48,
    "1.21.1": 48,
    "1.21.3": 57,
    "1.21.4": 61,
    "1.21.5": 71,
    "26.3": 121,
    "26.3.0": 121,
}

# Default `.mmod` archive timestamp: 1980-01-01T00:00:00Z, the ZIP epoch.
FIXED_ZIP_TIME = (1980, 1, 1, 0, 0, 0)


# ---------------------------------------------------------------------------
# Diagnostics
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Diagnostic:
    """One actionable finding, shaped for both humans and editing agents."""

    code: str
    path: str
    message: str
    fix_hint: str
    severity: str = "error"

    def to_dict(self):
        return {
            "code": self.code,
            "path": self.path,
            "message": self.message,
            "fix_hint": self.fix_hint,
            "severity": self.severity,
        }


class Report:
    """Collects diagnostics for one run; `ok` is true with zero errors."""

    def __init__(self):
        self.diagnostics = []
        self.mod_id = None

    def error(self, code, path, message, fix_hint):
        self.diagnostics.append(Diagnostic(code, path, message, fix_hint, "error"))

    def warning(self, code, path, message, fix_hint):
        self.diagnostics.append(Diagnostic(code, path, message, fix_hint, "warning"))

    @property
    def errors(self):
        return [d for d in self.diagnostics if d.severity == "error"]

    @property
    def warnings(self):
        return [d for d in self.diagnostics if d.severity == "warning"]

    @property
    def ok(self):
        return not self.errors

    def to_dict(self):
        return {
            "ok": self.ok,
            "mod_id": self.mod_id,
            "diagnostics": [d.to_dict() for d in self.diagnostics],
        }

    def render(self, header):
        lines = [header]
        for diagnostic in self.diagnostics:
            lines.append(
                f"  {diagnostic.severity}: [{diagnostic.code}] {diagnostic.path}: "
                f"{diagnostic.message}"
            )
            lines.append(f"      fix: {diagnostic.fix_hint}")
        counts = f"{len(self.errors)} error(s), {len(self.warnings)} warning(s)"
        lines.append(f"  {counts}")
        return "\n".join(lines)


# ---------------------------------------------------------------------------
# Resource locations, versions, and TOML
# ---------------------------------------------------------------------------


def resource_location_error(text):
    """Mirror `megu_core::id::ResourceLocation::parse`; return a reason or None."""
    if not isinstance(text, str):
        return "must be a string"
    if text.count(":") > 1:
        return "more than one ':' separator"
    namespace, path = text.split(":", 1) if ":" in text else ("minecraft", text)
    if not namespace:
        return "empty namespace"
    if not path:
        return "empty path"
    if not NAMESPACE.match(namespace):
        return "character outside [a-z0-9_.-] in namespace"
    if not PATH_CHARS.match(path):
        return "character outside [a-z0-9_./-] in path"
    if any(segment == ".." for segment in path.split("/")):
        return "'..' traversal in path"
    return None


def tag_or_resource_error(text):
    """Mirror `check_tag_or_rl`: one optional `#` then a resource location."""
    if not isinstance(text, str):
        return "must be a string"
    body = text[1:] if text.startswith("#") else text
    return resource_location_error(body)


def _version_key(major, minor, patch):
    return (major << 32) | (minor << 16) | patch


def _parse_numeric_version(text):
    if not text:
        raise ValueError("empty version")
    parts = text.split(".")
    if len(parts) > 3:
        raise ValueError("more than three components")
    numbers = []
    for part in parts:
        if not part or not part.isdigit():
            raise ValueError(f"non-numeric component '{part}'")
        value = int(part)
        if value > 0xFFFF:
            raise ValueError(f"component '{part}' out of range")
        numbers.append(value)
    while len(numbers) < 3:
        numbers.append(0)
    return numbers


def _parse_version_spec(text):
    """Mirror `parse_spec`: (lo[3], specified, wildcard_only)."""
    spec = text.strip()
    if spec[:1] in ("v", "V"):
        spec = spec[1:]
    spec = spec.split("+", 1)[0]
    if not spec:
        raise ValueError("empty version spec")
    if "-" in spec:
        raise ValueError("prerelease/hyphenated spec is unsupported")
    parts = spec.split(".")
    if len(parts) > 3:
        raise ValueError("more than three components")
    lo = [0, 0, 0]
    specified = len(parts)
    saw_wildcard = False
    for index, part in enumerate(parts):
        if not part:
            raise ValueError("empty component")
        if part in ("x", "X", "*"):
            saw_wildcard = True
            if specified == len(parts):
                specified = index
            continue
        if saw_wildcard:
            raise ValueError("numeric component after a wildcard")
        if not part.isdigit():
            raise ValueError(f"non-numeric component '{part}'")
        value = int(part)
        if value > 0xFFFF:
            raise ValueError(f"component '{part}' out of range")
        lo[index] = value
    return lo, specified


def _band_hi(lo, specified):
    if specified == 0:
        return 1 << 64
    if specified == 1:
        return (lo[0] + 1) << 32
    if specified == 2:
        return (lo[0] << 32) | ((lo[1] + 1) << 16)
    return _version_key(*lo) + 1


def _predicate_interval(token):
    if token in ("*", "x", "X"):
        return 0, 1 << 64
    operator = ""
    body = token
    for candidate in ("~", "^", ">=", "<=", ">", "<", "="):
        if token.startswith(candidate):
            operator, body = candidate, token[len(candidate):]
            break
    if not body:
        raise ValueError(f"predicate '{token}' is missing a version")
    lo, specified = _parse_version_spec(body)
    if specified == 0:
        return 0, 1 << 64
    lo_key = _version_key(*lo)
    hi_band = _band_hi(lo, specified)
    if operator == ">=":
        return lo_key, 1 << 64
    if operator == ">":
        return hi_band, 1 << 64
    if operator == "<=":
        return (0, lo_key) if specified == 3 else (0, hi_band - 1)
    if operator == "<":
        return (1, 0) if lo_key == 0 else (0, lo_key - 1)
    if operator == "=":
        return lo_key, hi_band - 1
    if operator == "~":
        high = (lo[0] << 32) | ((lo[1] + 1) << 16) if specified >= 2 else (lo[0] + 1) << 32
        return lo_key, high - 1
    if operator == "^":
        if lo[0] > 0:
            high = (lo[0] + 1) << 32
        elif specified >= 2 and lo[1] > 0:
            high = (lo[1] + 1) << 16
        elif specified == 3:
            high = lo[2] + 1
        elif specified == 2:
            high = 1 << 16
        else:
            high = 1 << 32
        return lo_key, high - 1
    return lo_key, hi_band - 1


def _parse_maven_member(text, index):
    open_inclusive = text[index] == "["
    closer = index + 1
    while closer < len(text) and text[closer] not in "])":
        closer += 1
    if closer >= len(text):
        raise ValueError("unterminated interval")
    close_inclusive = text[closer] == "]"
    inner = text[index + 1:closer]
    parts = inner.split(",")
    if len(parts) == 1:
        value = parts[0].strip()
        if not value:
            raise ValueError("empty interval")
        if not open_inclusive:
            raise ValueError("exact pin must use square brackets")
        key = _version_key(*_parse_numeric_version(value))
        return key, key, closer + 1
    if len(parts) == 2:
        low_text, high_text = parts[0].strip(), parts[1].strip()
        if low_text:
            key = _version_key(*_parse_numeric_version(low_text))
            low = key if open_inclusive else key + 1
        else:
            low = 0
        if high_text:
            key = _version_key(*_parse_numeric_version(high_text))
            high = key if close_inclusive else key - 1
        else:
            high = 1 << 64
        return low, high, closer + 1
    raise ValueError("interval has more than two bounds")


def version_range_error(text):
    """Mirror `VersionRange::parse` (Maven intervals or Fabric SemVer)."""
    if not isinstance(text, str):
        return "must be a string"
    declared = text.strip()
    if not declared:
        return "empty version range"
    intervals = 0
    try:
        if declared[0] in "[(":
            index = 0
            while True:
                while index < len(declared) and declared[index].isspace():
                    index += 1
                if index >= len(declared):
                    if intervals == 0:
                        return "empty version range"
                    return "trailing separator"
                if declared[index] in "[(":
                    _, _, index = _parse_maven_member(declared, index)
                    intervals += 1
                else:
                    end = declared.find(",", index)
                    end = len(declared) if end < 0 else end
                    token = declared[index:end].strip()
                    if not token:
                        return "empty list member"
                    _parse_numeric_version(token)
                    intervals += 1
                    index = end
                while index < len(declared) and declared[index].isspace():
                    index += 1
                if index >= len(declared):
                    return None
                if declared[index] != ",":
                    return "expected ',' between intervals"
                index += 1
        for arm in declared.split("||"):
            arm = arm.strip()
            if not arm:
                return "empty '||' arm"
            tokens = arm.split()
            position = 0
            while position < len(tokens):
                token = tokens[position]
                if token in ("~", "^", ">=", "<=", ">", "<", "=") and position + 1 < len(tokens):
                    token += tokens[position + 1]
                    position += 1
                _predicate_interval(token)
                position += 1
    except ValueError as error:
        return str(error)
    return None


def _mini_toml(text):
    """Bounded TOML subset used only when `tomllib` is absent (Python <= 3.10)."""
    root = {}
    current = root
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[[") and line.endswith("]]"):
            table = line[2:-2].strip()
            parent = root
            parts = table.split(".")
            for part in parts[:-1]:
                parent = parent.setdefault(part, {})
            array = parent.setdefault(parts[-1], [])
            if not isinstance(array, list):
                raise ValueError(f"line {number}: '{table}' is not an array table")
            current = {}
            array.append(current)
            continue
        if line.startswith("[") and line.endswith("]"):
            current = root
            for part in line[1:-1].strip().split("."):
                value = current.setdefault(part, {})
                if not isinstance(value, dict):
                    raise ValueError(f"line {number}: '{part}' is not a table")
                current = value
            continue
        if "=" not in line:
            raise ValueError(f"line {number}: expected key = value")
        key, value = line.split("=", 1)
        current[key.strip().strip('"')] = _mini_toml_value(value.strip(), number)
    return root


def _mini_toml_value(text, number):
    if text in ("true", "false"):
        return text == "true"
    if text.startswith('"') and text.endswith('"') and len(text) >= 2:
        body = text[1:-1]
        return body.replace('\\"', '"').replace("\\\\", "\\").replace("\\n", "\n").replace("\\t", "\t")
    if text.startswith("'") and text.endswith("'") and len(text) >= 2:
        return text[1:-1]
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        if not inner:
            return []
        return [_mini_toml_value(item.strip(), number) for item in inner.split(",")]
    for cast in (int, float):
        try:
            return cast(text.replace("_", ""))
        except ValueError:
            continue
    raise ValueError(f"line {number}: unsupported TOML value {text!r}")


def parse_toml(text):
    if _tomllib is not None:
        return _tomllib.loads(text)
    return _mini_toml(text)


def dump_toml_string(value):
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


# ---------------------------------------------------------------------------
# Bundle validation (mirrors DeclarativeModBundle::from_json)
# ---------------------------------------------------------------------------


class BundleValidator:
    """Fail-closed `megu-mod-bundle/1` validation with JSON-pointer paths."""

    def __init__(self, report, where=BUNDLE_FILE):
        self.report = report
        self.where = where
        self.mod_id = None

    # -- primitives ---------------------------------------------------------
    def _fail(self, pointer, code, message, fix_hint):
        self.report.error(code, f"{self.where}#{pointer}", message, fix_hint)

    @staticmethod
    def _is_number(value):
        return isinstance(value, (int, float)) and not isinstance(value, bool)

    @staticmethod
    def _is_int(value):
        return BundleValidator._is_number(value) and float(value).is_integer()

    def _expect_object(self, value, pointer):
        if not isinstance(value, dict):
            self._fail(pointer, "bundle.field", "must be a JSON object", f"replace {pointer} with an object")
            return None
        return value

    def _check_keys(self, obj, allowed, pointer):
        for key in obj:
            if key not in allowed:
                self._fail(
                    f"{pointer}/{key}",
                    "bundle.unknown_key",
                    f"unknown key '{key}' (rejected by DeclarativeModBundle::from_json)",
                    f"remove '{key}'; the runtime accepts only {list(allowed)}",
                )

    def _resource_location(self, obj, key, pointer, required=True, nullable=False):
        target = f"{pointer}/{key}"
        if key not in obj or (nullable and obj.get(key) is None):
            if required and not nullable:
                self._fail(target, "bundle.missing", f"missing required '{key}'", f"add \"{key}\" as a resource location")
            return None
        value = obj[key]
        reason = resource_location_error(value)
        if reason:
            self._fail(target, "bundle.id", f"invalid resource location {value!r}: {reason}", f"use lowercase [a-z0-9_./-] with one optional ':' namespace")
            return None
        return value

    def _tag_or_resource(self, obj, key, pointer, required=True):
        target = f"{pointer}/{key}"
        if key not in obj:
            if required:
                self._fail(target, "bundle.missing", f"missing required '{key}'", f"add \"{key}\" as an id or '#tag'")
            return None
        value = obj[key]
        reason = tag_or_resource_error(value)
        if reason:
            self._fail(target, "bundle.id", f"invalid tag/id {value!r}: {reason}", "use '#namespace:path' or a resource location")
            return None
        return value

    def _integer(self, obj, key, pointer, low, high, required=True, default=None):
        target = f"{pointer}/{key}"
        if key not in obj:
            if required:
                self._fail(target, "bundle.missing", f"missing required '{key}'", f"add an integer {low}..{high}")
                return None
            return default
        value = obj[key]
        if not self._is_int(value) or not (low <= value <= high):
            self._fail(target, "bundle.integer", f"{value!r} is not an integer in {low}..{high}", f"use an integer between {low} and {high}")
            return None
        return int(value)

    def _number(self, obj, key, pointer, minimum=None, exclusive_minimum=None, maximum=None, required=True, default=None):
        target = f"{pointer}/{key}"
        if key not in obj:
            if required:
                self._fail(target, "bundle.missing", f"missing required '{key}'", "add a finite number")
                return None
            return default
        value = obj[key]
        if not self._is_number(value) or value != value or value in (float("inf"), float("-inf")):
            self._fail(target, "bundle.number", f"{value!r} is not a finite number", "use a finite JSON number")
            return None
        value = float(value)
        if minimum is not None and value < minimum:
            self._fail(target, "bundle.number", f"{value} is below the minimum {minimum}", f"use a value >= {minimum}")
            return None
        if exclusive_minimum is not None and value <= exclusive_minimum:
            self._fail(target, "bundle.number", f"{value} must be > {exclusive_minimum}", f"use a value > {exclusive_minimum}")
            return None
        if maximum is not None and value > maximum:
            self._fail(target, "bundle.number", f"{value} exceeds the maximum {maximum}", f"use a value <= {maximum}")
            return None
        return value

    def _boolean(self, obj, key, pointer, default=False):
        target = f"{pointer}/{key}"
        if key not in obj:
            return default
        value = obj[key]
        if not isinstance(value, bool):
            self._fail(target, "bundle.boolean", f"{value!r} is not a boolean", "use true or false")
            return default
        return value

    def _string(self, obj, key, pointer, required=True, non_empty=False, default=None):
        target = f"{pointer}/{key}"
        if key not in obj:
            if required:
                self._fail(target, "bundle.missing", f"missing required '{key}'", f"add a string '{key}'")
                return None
            return default
        value = obj[key]
        if not isinstance(value, str):
            self._fail(target, "bundle.string", f"{value!r} is not a string", "use a JSON string")
            return None
        if non_empty and not value:
            self._fail(target, "bundle.string", "must be non-empty", "add a non-empty string")
            return None
        return value

    def _array(self, obj, key, pointer, required=True, cap=MAX_SECTION_ENTRIES):
        target = f"{pointer}/{key}"
        if key not in obj:
            if required:
                self._fail(target, "bundle.missing", f"missing required '{key}'", f"add \"{key}\": []")
                return None
            return None
        value = obj[key]
        if not isinstance(value, list):
            self._fail(target, "bundle.array", f"{key} must be an array", f"use \"{key}\": []")
            return None
        if len(value) > cap:
            self._fail(target, "bundle.bound", f"{len(value)} entries exceeds the {cap} cap", "split the mod or remove entries")
            return None
        return value

    def _string_list(self, obj, key, pointer, kind, required=True, non_empty=False):
        items = self._array(obj, key, pointer, required=required, cap=MAX_INNER_ENTRIES)
        if items is None:
            return None
        check = resource_location_error if kind == "rl" else tag_or_resource_error
        out = []
        for index, value in enumerate(items):
            reason = check(value)
            if reason:
                self._fail(f"{pointer}/{key}/{index}", "bundle.id", f"invalid {kind} {value!r}: {reason}", "use a lowercase resource location")
                continue
            out.append(value)
        if non_empty and not items:
            self._fail(f"{pointer}/{key}", "bundle.empty", f"{key} must be non-empty", "add at least one entry")
        return out

    # -- entry point --------------------------------------------------------
    def validate(self, root):
        if not isinstance(root, dict):
            self._fail("", "bundle.type", "bundle root must be a JSON object", "wrap the document in an object")
            return None
        self._check_keys(root, TOP_LEVEL_KEYS, "")
        for key in REQUIRED_TOP_LEVEL_KEYS:
            if key not in root:
                self._fail(f"/{key}", "bundle.missing", f"missing required key '{key}'", f"add \"{key}\" (arrays may be empty)")
        schema = root.get("schema")
        if schema != BUNDLE_SCHEMA:
            self._fail("/schema", "bundle.schema", f"schema must be exactly {BUNDLE_SCHEMA!r}, found {schema!r}", f"set \"schema\": \"{BUNDLE_SCHEMA}\"")
        mod_id = self._resource_location(root, "mod_id", "")
        if mod_id:
            self.mod_id = mod_id
        range_text = self._string(root, "mc_version_range", "", non_empty=True)
        if range_text is not None:
            reason = version_range_error(range_text)
            if reason:
                self._fail("/mc_version_range", "bundle.version_range", f"{range_text!r} is not a version range: {reason}", "use e.g. \"[1.21.1,)\" or \">=1.21.1 <1.22\"")
        portals = self._array(root, "portals", "")
        for index, entry in enumerate(portals or []):
            self._portal(entry, f"/portals/{index}")
        barriers = self._array(root, "progression_barriers", "")
        for index, entry in enumerate(barriers or []):
            self._progression(entry, f"/progression_barriers/{index}")
        regions = self._array(root, "biome_regions", "")
        for index, entry in enumerate(regions or []):
            self._region(entry, f"/biome_regions/{index}")
        creatures = self._array(root, "creatures", "")
        for index, entry in enumerate(creatures or []):
            self._creature(entry, f"/creatures/{index}")
        self._string_list(root, "custom_channels", "", "rl")
        spells = self._array(root, "spells", "", required=False)
        seen_spells = set()
        for index, entry in enumerate(spells or []):
            pointer = f"/spells/{index}"
            spell = self._spell(entry, pointer)
            if spell:
                if spell in seen_spells:
                    self._fail(f"{pointer}/id", "bundle.duplicate_id", f"duplicate spell id '{spell}'", "give every spell a unique id")
                seen_spells.add(spell)
        statuses = self._array(root, "custom_statuses", "", required=False)
        seen_statuses = set()
        for index, entry in enumerate(statuses or []):
            pointer = f"/custom_statuses/{index}"
            status = self._status(entry, pointer)
            if status:
                if status in seen_statuses:
                    self._fail(f"{pointer}/id", "bundle.duplicate_id", f"duplicate status id '{status}'", "give every status a unique id")
                seen_statuses.add(status)
        rituals = self._array(root, "rituals", "", required=False)
        seen_rituals = set()
        for index, entry in enumerate(rituals or []):
            pointer = f"/rituals/{index}"
            ritual = self._ritual(entry, pointer)
            if ritual:
                if ritual in seen_rituals:
                    self._fail(f"{pointer}/id", "bundle.duplicate_id", f"duplicate ritual id '{ritual}'", "give every ritual a unique id")
                seen_rituals.add(ritual)
        return None

    # -- sections -----------------------------------------------------------
    def _portal(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return
        self._check_keys(obj, ("name", "target_dim", "teleport_grace_ticks", "trigger"), pointer)
        self._string(obj, "name", pointer, non_empty=True)
        self._resource_location(obj, "target_dim", pointer)
        if "teleport_grace_ticks" in obj:
            self._integer(obj, "teleport_grace_ticks", pointer, 0, 0xFFFFFFFF, required=False)
        trigger = obj.get("trigger")
        if not isinstance(trigger, dict):
            self._fail(f"{pointer}/trigger", "bundle.missing", "trigger must be an object", "add a trigger object with a 'kind'")
            return
        kind = trigger.get("kind")
        table = {
            "vertical_frame": (
                ("kind", "frame_block", "igniter_item", "min_width", "min_height", "max_width", "max_height", "portal_block"),
                self._trigger_vertical_frame,
            ),
            "horizontal_pool": (
                ("kind", "border_tags_or_blocks", "fluid_block", "min_size", "drop_item", "portal_block"),
                self._trigger_horizontal_pool,
            ),
            "altitude_threshold": (("kind", "min_y_trigger", "required_vehicle_or_item"), self._trigger_altitude),
            "direct_block_interact": (("kind", "block_id", "required_item"), self._trigger_direct),
        }
        if kind not in table:
            self._fail(
                f"{pointer}/trigger/kind",
                "bundle.enum",
                f"unknown portal trigger kind {kind!r}",
                "use vertical_frame, horizontal_pool, altitude_threshold, or direct_block_interact",
            )
            return
        allowed, handler = table[kind]
        self._check_keys(trigger, allowed, f"{pointer}/trigger")
        handler(trigger, f"{pointer}/trigger")

    def _trigger_vertical_frame(self, trigger, pointer):
        self._resource_location(trigger, "frame_block", pointer)
        self._resource_location(trigger, "igniter_item", pointer)
        self._resource_location(trigger, "portal_block", pointer)
        minimums = []
        for key in ("min_width", "min_height"):
            value = self._integer(trigger, key, pointer, 1, 0xFFFFFFFF)
            minimums.append(value)
        for key, minimum in zip(("max_width", "max_height"), minimums):
            value = self._integer(trigger, key, pointer, 0, 0xFFFFFFFF)
            if value is not None and minimum is not None and value < minimum:
                self._fail(f"{pointer}/{key}", "bundle.range", f"{key} {value} is below its minimum {minimum}", "make every maximum >= its minimum")

    def _trigger_horizontal_pool(self, trigger, pointer):
        borders = self._string_list(trigger, "border_tags_or_blocks", pointer, "tag", non_empty=True)
        self._resource_location(trigger, "fluid_block", pointer)
        self._integer(trigger, "min_size", pointer, 1, 0xFFFFFFFF)
        self._resource_location(trigger, "drop_item", pointer)
        self._resource_location(trigger, "portal_block", pointer)
        if borders is not None and not borders:
            self._fail(f"{pointer}/border_tags_or_blocks", "bundle.empty", "border list resolved to no valid entries", "add at least one block id or #tag")

    def _trigger_altitude(self, trigger, pointer):
        self._integer(trigger, "min_y_trigger", pointer, -0x80000000, 0x7FFFFFFF)
        if "required_vehicle_or_item" in trigger:
            self._resource_location(trigger, "required_vehicle_or_item", pointer, required=False, nullable=True)

    def _trigger_direct(self, trigger, pointer):
        self._resource_location(trigger, "block_id", pointer)
        if "required_item" in trigger:
            self._resource_location(trigger, "required_item", pointer, required=False, nullable=True)

    def _progression(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return
        self._check_keys(
            obj,
            (
                "id",
                "dimension",
                "target_biomes",
                "required_advancements",
                "block_break_protected",
                "block_place_protected",
                "enforce_effects",
                "periodic_damage",
            ),
            pointer,
        )
        self._resource_location(obj, "id", pointer)
        self._resource_location(obj, "dimension", pointer)
        self._string_list(obj, "target_biomes", pointer, "rl", non_empty=True)
        self._string_list(obj, "required_advancements", pointer, "rl", non_empty=True)
        self._boolean(obj, "block_break_protected", pointer, True)
        self._boolean(obj, "block_place_protected", pointer, True)
        effects = self._array(obj, "enforce_effects", pointer, required=False, cap=MAX_INNER_ENTRIES)
        for index, effect in enumerate(effects or []):
            target = f"{pointer}/enforce_effects/{index}"
            effect_obj = self._expect_object(effect, target)
            if effect_obj is None:
                continue
            self._check_keys(effect_obj, ("effect", "duration_ticks", "amplifier"), target)
            self._resource_location(effect_obj, "effect", target)
            self._integer(effect_obj, "duration_ticks", target, 0, 0xFFFFFFFF)
            self._integer(effect_obj, "amplifier", target, 0, 255)
        damage = obj.get("periodic_damage")
        if damage is not None:
            target = f"{pointer}/periodic_damage"
            damage_obj = self._expect_object(damage, target)
            if damage_obj is not None:
                self._check_keys(damage_obj, ("hearts", "interval_ticks"), target)
                self._number(damage_obj, "hearts", target, exclusive_minimum=0)
                self._integer(damage_obj, "interval_ticks", target, 1, 0xFFFFFFFF)

    def _region(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return
        self._check_keys(obj, ("mod_id", "dimension", "weight", "biome_overrides"), pointer)
        self._resource_location(obj, "mod_id", pointer)
        self._resource_location(obj, "dimension", pointer)
        self._integer(obj, "weight", pointer, 0, 0xFFFFFFFF)
        overrides = obj.get("biome_overrides")
        if not isinstance(overrides, dict):
            self._fail(f"{pointer}/biome_overrides", "bundle.field", "biome_overrides must be an object", "map biome id to replacement biome id")
            return
        if not overrides:
            self._fail(f"{pointer}/biome_overrides", "bundle.empty", "biome_overrides must be non-empty", "add at least one biome mapping")
        if len(overrides) > MAX_INNER_ENTRIES:
            self._fail(f"{pointer}/biome_overrides", "bundle.bound", f"exceeds the {MAX_INNER_ENTRIES} entry cap", "remove entries")
        for key, value in overrides.items():
            reason = resource_location_error(key)
            if reason:
                self._fail(f"{pointer}/biome_overrides/{key}", "bundle.id", f"invalid biome id {key!r}: {reason}", "use a resource location key")
            reason = resource_location_error(value)
            if reason:
                self._fail(f"{pointer}/biome_overrides/{key}", "bundle.id", f"invalid replacement biome {value!r}: {reason}", "use a resource location value")

    def _creature(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return
        self._check_keys(
            obj,
            (
                "entity_type",
                "max_health",
                "tame_items",
                "tame_chance",
                "breed_items",
                "heal_items",
                "baby_growth_ticks",
                "breed_cooldown_ticks",
                "supports_wander_mode",
                "can_shoulder_perch",
                "rideable_when_tamed",
                "movement",
            ),
            pointer,
        )
        self._resource_location(obj, "entity_type", pointer)
        self._number(obj, "max_health", pointer, exclusive_minimum=0)
        self._string_list(obj, "tame_items", pointer, "tag", required=False)
        self._number(obj, "tame_chance", pointer, minimum=0, maximum=1, required=False, default=0.0)
        self._string_list(obj, "breed_items", pointer, "tag", required=False)
        heal_items = self._array(obj, "heal_items", pointer, required=False, cap=MAX_INNER_ENTRIES)
        for index, item in enumerate(heal_items or []):
            target = f"{pointer}/heal_items/{index}"
            item_obj = self._expect_object(item, target)
            if item_obj is None:
                continue
            self._check_keys(item_obj, ("item", "heal"), target)
            self._tag_or_resource(item_obj, "item", target)
            self._number(item_obj, "heal", target, minimum=0)
        self._integer(obj, "baby_growth_ticks", pointer, 0, 0xFFFFFFFF, required=False, default=DEFAULT_BABY_GROWTH_TICKS)
        self._integer(obj, "breed_cooldown_ticks", pointer, 0, 0xFFFFFFFF, required=False, default=DEFAULT_BREED_COOLDOWN_TICKS)
        self._boolean(obj, "supports_wander_mode", pointer)
        self._boolean(obj, "can_shoulder_perch", pointer)
        self._boolean(obj, "rideable_when_tamed", pointer)
        movement = obj.get("movement")
        if not isinstance(movement, dict):
            self._fail(f"{pointer}/movement", "bundle.missing", "movement must be an object", "add {\"kind\": \"land\", \"ground_speed\": 0.25}")
            return
        self._movement(movement, f"{pointer}/movement")

    def _movement(self, movement, pointer):
        kind = movement.get("kind")
        table = {
            "land": (("kind", "ground_speed"), ("ground_speed",)),
            "amphibious": (("kind", "land_speed", "swim_speed"), ("land_speed", "swim_speed")),
            "aquatic": (("kind", "swim_speed", "suffocates_in_air_ticks"), ("swim_speed",)),
            "gliding_flight": (("kind", "ground_speed", "fly_speed", "glide_fall_scale"), ("ground_speed", "fly_speed", "glide_fall_scale")),
            "wall_climb": (("kind", "ground_speed", "climb_speed"), ("ground_speed", "climb_speed")),
        }
        if kind not in table:
            self._fail(f"{pointer}/kind", "bundle.enum", f"unknown movement profile kind {kind!r}", "use land, amphibious, aquatic, gliding_flight, or wall_climb")
            return
        allowed, speeds = table[kind]
        self._check_keys(movement, allowed, pointer)
        for key in speeds:
            self._number(movement, key, pointer, minimum=0)
        if kind == "aquatic":
            self._integer(movement, "suffocates_in_air_ticks", pointer, 0, 0xFFFFFFFF)

    # -- magic sections -----------------------------------------------------
    def _spell(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return None
        self._check_keys(
            obj,
            ("id", "mana_cost", "base_cooldown_ticks", "cast", "interrupt_on_damage", "interrupt_on_move", "steps"),
            pointer,
        )
        spell_id = self._resource_location(obj, "id", pointer)
        self._number(obj, "mana_cost", pointer, minimum=0)
        self._integer(obj, "base_cooldown_ticks", pointer, 0, 0xFFFFFFFF)
        self._boolean(obj, "interrupt_on_damage", pointer)
        self._boolean(obj, "interrupt_on_move", pointer)
        cast = obj.get("cast")
        if not isinstance(cast, dict):
            self._fail(f"{pointer}/cast", "bundle.missing", "cast must be an object", "add {\"kind\": \"instant\"}")
        else:
            self._spell_cast(cast, f"{pointer}/cast")
        steps = self._array(obj, "steps", pointer, cap=MAX_SPELL_STEPS)
        if steps is not None and not steps:
            self._fail(f"{pointer}/steps", "bundle.empty", "steps must be non-empty", "add at least one delivery step")
        for index, step in enumerate(steps or []):
            target = f"{pointer}/steps/{index}"
            step_obj = self._expect_object(step, target)
            if step_obj is None:
                continue
            self._check_keys(step_obj, ("form", "effects"), target)
            form = step_obj.get("form")
            if not isinstance(form, dict):
                self._fail(f"{target}/form", "bundle.missing", "form must be an object", "add a form object with a 'kind'")
            else:
                self._spell_form(form, f"{target}/form")
            effects = self._array(step_obj, "effects", target, cap=MAX_EFFECTS_PER_STEP)
            for effect_index, effect in enumerate(effects or []):
                effect_pointer = f"{target}/effects/{effect_index}"
                effect_obj = self._expect_object(effect, effect_pointer)
                if effect_obj is not None:
                    self._spell_effect(effect_obj, effect_pointer)
        return spell_id

    def _spell_cast(self, cast, pointer):
        kind = cast.get("kind")
        if kind == "instant":
            self._check_keys(cast, ("kind",), pointer)
        elif kind == "cast_time":
            self._check_keys(cast, ("kind", "total_ticks"), pointer)
            self._integer(cast, "total_ticks", pointer, 1, MAX_DURATION_TICKS)
        elif kind == "channeled":
            self._check_keys(cast, ("kind", "interval_ticks", "max_ticks", "mana_per_pulse"), pointer)
            self._integer(cast, "interval_ticks", pointer, 1, 0xFFFFFFFF)
            self._integer(cast, "max_ticks", pointer, 1, MAX_DURATION_TICKS)
            self._number(cast, "mana_per_pulse", pointer, minimum=0)
        else:
            self._fail(f"{pointer}/kind", "bundle.enum", f"unknown cast kind {kind!r}", "use instant, cast_time, or channeled")

    def _spell_form(self, form, pointer):
        kind = form.get("kind")
        if kind == "self_cast":
            self._check_keys(form, ("kind",), pointer)
        elif kind == "touch":
            self._check_keys(form, ("kind", "range"), pointer)
            self._number(form, "range", pointer, exclusive_minimum=0)
        elif kind == "projectile":
            self._check_keys(form, ("kind", "speed", "gravity", "max_ticks"), pointer)
            self._number(form, "speed", pointer, exclusive_minimum=0)
            self._number(form, "gravity", pointer)
            self._integer(form, "max_ticks", pointer, 1, MAX_DURATION_TICKS)
        elif kind == "raycast_beam":
            self._check_keys(form, ("kind", "range", "pierce_cap"), pointer)
            self._number(form, "range", pointer, exclusive_minimum=0)
            self._integer(form, "pierce_cap", pointer, 1, MAX_PIERCE_CAP)
        elif kind == "aoe_zone":
            self._check_keys(form, ("kind", "radius", "duration_ticks", "pulse_interval"), pointer)
            self._number(form, "radius", pointer, exclusive_minimum=0, maximum=MAX_AOE_RADIUS)
            self._integer(form, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
            self._integer(form, "pulse_interval", pointer, 1, 0xFFFFFFFF)
        elif kind == "chain":
            self._check_keys(form, ("kind", "max_hops", "hop_radius"), pointer)
            self._integer(form, "max_hops", pointer, 1, MAX_CHAIN_HOPS)
            self._number(form, "hop_radius", pointer, exclusive_minimum=0)
        elif kind == "orbit":
            self._check_keys(form, ("kind", "count", "radius", "duration_ticks"), pointer)
            self._integer(form, "count", pointer, 1, MAX_ORBIT_COUNT)
            self._number(form, "radius", pointer, exclusive_minimum=0)
            self._integer(form, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
        elif kind == "summon":
            self._check_keys(form, ("kind", "entity_type", "duration_ticks", "max_active"), pointer)
            self._resource_location(form, "entity_type", pointer)
            self._integer(form, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
            self._integer(form, "max_active", pointer, 1, MAX_ACTIVE_SUMMONS)
        else:
            self._fail(
                f"{pointer}/kind",
                "bundle.enum",
                f"unknown form kind {kind!r}",
                "use self_cast, touch, projectile, raycast_beam, aoe_zone, chain, orbit, or summon",
            )

    def _spell_effect(self, effect, pointer):
        kind = effect.get("kind")
        if kind == "damage":
            self._check_keys(effect, ("kind", "school", "base_amount"), pointer)
            self._resource_location(effect, "school", pointer)
            self._number(effect, "base_amount", pointer, minimum=0)
        elif kind == "heal":
            self._check_keys(effect, ("kind", "base_amount"), pointer)
            self._number(effect, "base_amount", pointer, minimum=0)
        elif kind == "shield":
            self._check_keys(effect, ("kind", "amount", "duration_ticks"), pointer)
            self._number(effect, "amount", pointer, minimum=0)
            self._integer(effect, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
        elif kind == "apply_status":
            self._check_keys(effect, ("kind", "status_id", "amplifier", "duration_ticks"), pointer)
            self._resource_location(effect, "status_id", pointer)
            self._integer(effect, "amplifier", pointer, 0, 255)
            self._integer(effect, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
        elif kind == "knockback":
            self._check_keys(effect, ("kind", "strength"), pointer)
            self._number(effect, "strength", pointer, minimum=0)
        elif kind == "blink":
            self._check_keys(effect, ("kind", "max_distance"), pointer)
            self._number(effect, "max_distance", pointer, minimum=0)
        elif kind == "conjure_light":
            self._check_keys(effect, ("kind", "level", "duration_ticks"), pointer)
            self._integer(effect, "level", pointer, 0, 15)
            self._integer(effect, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
        elif kind == "break_block":
            self._check_keys(effect, ("kind", "max_tier"), pointer)
            self._integer(effect, "max_tier", pointer, 0, 4)
        elif kind == "emit_vfx":
            self._check_keys(effect, ("kind", "vfx_id"), pointer)
            self._resource_location(effect, "vfx_id", pointer)
        else:
            self._fail(
                f"{pointer}/kind",
                "bundle.enum",
                f"unknown effect kind {kind!r}",
                "use damage, heal, shield, apply_status, knockback, blink, conjure_light, break_block, or emit_vfx",
            )

    def _status(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return None
        self._check_keys(
            obj,
            (
                "id",
                "max_amplifier",
                "pulse_interval_ticks",
                "damage_per_pulse",
                "heal_per_pulse",
                "absorption_per_level",
                "frozen",
                "rooted",
                "silenced",
                "disarmed",
                "attribute_modifiers",
                "vfx_aura",
            ),
            pointer,
        )
        status_id = self._resource_location(obj, "id", pointer)
        self._integer(obj, "max_amplifier", pointer, 0, 255, required=False, default=0)
        interval = self._integer(obj, "pulse_interval_ticks", pointer, 0, 0xFFFFFFFF, required=False, default=0)
        damage = self._number(obj, "damage_per_pulse", pointer, minimum=0, required=False, default=0.0)
        heal = self._number(obj, "heal_per_pulse", pointer, minimum=0, required=False, default=0.0)
        self._number(obj, "absorption_per_level", pointer, minimum=0, required=False, default=0.0)
        for key in ("frozen", "rooted", "silenced", "disarmed"):
            self._boolean(obj, key, pointer)
        modifiers = obj.get("attribute_modifiers")
        if modifiers is not None:
            target = f"{pointer}/attribute_modifiers"
            if not isinstance(modifiers, dict):
                self._fail(target, "bundle.field", "attribute_modifiers must be an object", "map attribute id to a number")
            else:
                if len(modifiers) > MAX_ATTRIBUTE_MODIFIERS:
                    self._fail(target, "bundle.bound", f"exceeds the {MAX_ATTRIBUTE_MODIFIERS} modifier cap", "remove modifiers")
                for key, value in modifiers.items():
                    reason = resource_location_error(key)
                    if reason:
                        self._fail(f"{target}/{key}", "bundle.id", f"invalid attribute id {key!r}: {reason}", "use a resource location key")
                    if not self._is_number(value) or value != value:
                        self._fail(f"{target}/{key}", "bundle.number", f"{value!r} is not a finite number", "use a finite number")
        if "vfx_aura" in obj:
            self._resource_location(obj, "vfx_aura", pointer, required=False, nullable=True)
        if (damage or 0.0) > 0.0 or (heal or 0.0) > 0.0:
            if (interval or 0) < 1:
                self._fail(
                    f"{pointer}/pulse_interval_ticks",
                    "bundle.range",
                    "damage_per_pulse/heal_per_pulse require pulse_interval_ticks >= 1",
                    "set pulse_interval_ticks to the tick interval between pulses",
                )
        return status_id

    def _ritual(self, entry, pointer):
        obj = self._expect_object(entry, pointer)
        if obj is None:
            return None
        self._check_keys(obj, ("id", "pattern", "reagents", "duration_ticks", "mana_per_tick"), pointer)
        ritual_id = self._resource_location(obj, "id", pointer)
        blocks = self._array(obj, "pattern", pointer, cap=MAX_RITUAL_PATTERN_BLOCKS)
        offsets = set()
        for index, block in enumerate(blocks or []):
            target = f"{pointer}/pattern/{index}"
            block_obj = self._expect_object(block, target)
            if block_obj is None:
                continue
            self._check_keys(block_obj, ("dx", "dy", "dz", "block"), target)
            offset = tuple(
                self._integer(block_obj, key, target, -MAX_RITUAL_OFFSET, MAX_RITUAL_OFFSET) for key in ("dx", "dy", "dz")
            )
            self._resource_location(block_obj, "block", target)
            if None not in offset:
                if offset in offsets:
                    self._fail(f"{target}", "bundle.duplicate_offset", f"duplicate pattern offset {offset}", "use one block per offset")
                offsets.add(offset)
        reagents = obj.get("reagents")
        if not isinstance(reagents, dict):
            self._fail(f"{pointer}/reagents", "bundle.field", "reagents must be an object", "map item id to required count")
        else:
            if len(reagents) > MAX_REAGENTS:
                self._fail(f"{pointer}/reagents", "bundle.bound", f"exceeds the {MAX_REAGENTS} reagent cap", "remove reagents")
            for key, value in reagents.items():
                reason = resource_location_error(key)
                if reason:
                    self._fail(f"{pointer}/reagents/{key}", "bundle.id", f"invalid item id {key!r}: {reason}", "use a resource location key")
                if not self._is_int(value) or value < 1:
                    self._fail(f"{pointer}/reagents/{key}", "bundle.integer", f"{value!r} must be an integer >= 1", "use a positive reagent count")
        self._integer(obj, "duration_ticks", pointer, 1, MAX_DURATION_TICKS)
        self._number(obj, "mana_per_tick", pointer, minimum=0)
        return ritual_id


# ---------------------------------------------------------------------------
# Project validation
# ---------------------------------------------------------------------------


def project_files(root):
    """Every regular file under `root`, skipping build/VCS/IDE directories."""
    for directory, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(name for name in dirnames if name not in SKIP_DIRS)
        for name in sorted(filenames):
            yield Path(directory) / name


def _read_json(path, report, rel):
    try:
        size = path.stat().st_size
    except OSError as error:
        report.error("file.unreadable", rel, f"cannot stat file: {error}", "check permissions")
        return None
    if size > MAX_JSON_BYTES:
        report.error("json.size", rel, f"{size} bytes exceeds the {MAX_JSON_BYTES} byte cap", "shrink the JSON file")
        return None
    try:
        with path.open("rb") as handle:
            return json.loads(handle.read().decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        report.error("json.parse", rel, f"invalid JSON: {error}", "fix the JSON syntax (trailing commas and comments are not allowed)")
        return None


def _check_result(value, rel, report):
    if not isinstance(value, dict):
        report.error("recipe.structure", rel, "result must be an object", "use {\"id\": \"<item>\", \"count\": 1}")
        return
    item = value.get("id", value.get("item"))
    if not isinstance(item, str) or resource_location_error(item):
        report.error("recipe.structure", rel, f"result id {item!r} is not a resource location", "use {\"id\": \"namespace:item\"}")
    count = value.get("count", 1)
    if not isinstance(count, int) or isinstance(count, bool) or count < 1:
        report.error("recipe.structure", rel, "result.count must be an integer >= 1", "set count to 1 or more")


def _check_recipe(data, rel, report):
    kind = data.get("type")
    if not isinstance(kind, str) or not kind:
        report.error("recipe.structure", rel, "recipe 'type' must be a non-empty string", "add \"type\": \"minecraft:crafting_shaped\"")
        return
    name = kind.split(":", 1)[1] if ":" in kind else kind
    if name == "crafting_shaped":
        pattern = data.get("pattern")
        if not isinstance(pattern, list) or not pattern or not all(isinstance(row, str) for row in pattern):
            report.error("recipe.structure", rel, "crafting_shaped requires a non-empty 'pattern' array of strings", "add \"pattern\": [\"RRR\", \"RRR\", \"RRR\"]")
        if not isinstance(data.get("key"), dict) or not data.get("key"):
            report.error("recipe.structure", rel, "crafting_shaped requires a non-empty 'key' object", "map every pattern symbol to an ingredient")
    elif name == "crafting_shapeless":
        ingredients = data.get("ingredients")
        if not isinstance(ingredients, list) or not ingredients:
            report.error("recipe.structure", rel, "crafting_shapeless requires a non-empty 'ingredients' array", "add at least one ingredient")
    elif name in ("smelting", "blasting", "smoking", "campfire_cooking"):
        if "ingredient" not in data:
            report.error("recipe.structure", rel, f"{name} requires an 'ingredient'", "add \"ingredient\": {\"item\": \"minecraft:redstone_block\"}")
        for key in ("experience", "cookingtime"):
            if key not in data:
                report.error("recipe.structure", rel, f"{name} requires '{key}'", "add experience (number) and cookingtime (ticks)")
        cookingtime = data.get("cookingtime")
        if not isinstance(cookingtime, int) or isinstance(cookingtime, bool) or cookingtime < 1:
            if "cookingtime" in data:
                report.error("recipe.structure", rel, "cookingtime must be an integer >= 1", "use a positive tick count")
    elif name == "stonecutting":
        if "ingredient" not in data:
            report.error("recipe.structure", rel, "stonecutting requires an 'ingredient'", "add an ingredient")
    if "result" in data:
        _check_result(data["result"], rel, report)


def _check_tag(data, rel, report):
    values = data.get("values")
    if not isinstance(values, list):
        report.error("tag.structure", rel, "tag requires a 'values' array", "add {\"replace\": false, \"values\": [\"...\"]}")
        return
    for index, entry in enumerate(values):
        if isinstance(entry, str):
            body = entry[1:] if entry.startswith("#") else entry
            reason = resource_location_error(body)
            if reason:
                report.error("tag.structure", rel, f"values[{index}] {entry!r} is not a resource location: {reason}", "use lowercase ids or #tags")
        elif isinstance(entry, dict):
            identifier = entry.get("id", entry.get("value"))
            if not isinstance(identifier, str):
                report.error("tag.structure", rel, f"values[{index}] object requires a string 'id'", "use {\"id\": \"namespace:item\", \"required\": false}")
            if "required" in entry and not isinstance(entry["required"], bool):
                report.error("tag.structure", rel, f"values[{index}].required must be a boolean", "use true or false")
        else:
            report.error("tag.structure", rel, f"values[{index}] must be a string or an object", "use a string id or an {\"id\": ...} object")
    if "replace" in data and not isinstance(data["replace"], bool):
        report.error("tag.structure", rel, "'replace' must be a boolean", "use true or false")


def _check_lang(data, rel, report):
    if not isinstance(data, dict):
        report.error("lang.structure", rel, "language file must be a JSON object", "map translation keys to strings")
        return
    for key, value in data.items():
        if not isinstance(value, str):
            report.error("lang.structure", rel, f"translation {key!r} must be a string", "quote the translation value")


def _check_blockstate(data, rel, report):
    if not isinstance(data, dict):
        report.error("blockstate.structure", rel, "blockstate file must be a JSON object", "use {\"variants\": {...}}")
        return
    if "variants" not in data and "multipart" not in data:
        report.error("blockstate.structure", rel, "blockstate requires 'variants' or 'multipart'", "add \"variants\": {\"\": {\"model\": \"<mod>:block/<name>\"}}")
        return
    if "variants" in data and not isinstance(data["variants"], dict):
        report.error("blockstate.structure", rel, "'variants' must be an object", "map variant keys to model objects")
    if "multipart" in data and not isinstance(data["multipart"], list):
        report.error("blockstate.structure", rel, "'multipart' must be an array", "use an array of part objects")


def _check_pack_mcmeta(data, rel, report):
    pack = data.get("pack") if isinstance(data, dict) else None
    if not isinstance(pack, dict):
        report.error("pack_mcmeta.structure", rel, "pack.mcmeta requires a 'pack' object", "use {\"pack\": {\"pack_format\": 48, \"description\": \"...\"}}")
        return
    if not isinstance(pack.get("pack_format"), int) or isinstance(pack.get("pack_format"), bool):
        report.error("pack_mcmeta.structure", rel, "'pack.pack_format' must be an integer", "use 48 for 1.21.1 or 121 for 26.3")
    if "description" not in pack:
        report.error("pack_mcmeta.structure", rel, "'pack.description' is required", "add a short description string")


def _check_fabric_manifest(data, rel, report, mod_id):
    if not isinstance(data, dict):
        report.error("compat.manifest", rel, "fabric.mod.json must be a JSON object", "use a schemaVersion 1 object")
        return
    if data.get("schemaVersion") != 1:
        report.error("compat.manifest", rel, "'schemaVersion' must be 1", "set \"schemaVersion\": 1")
    declared = data.get("id")
    if not isinstance(declared, str):
        report.error("compat.manifest", rel, "'id' must be a string", "set the mod id")
    elif mod_id and declared != mod_id:
        report.error("compat.mismatch", rel, f"fabric id {declared!r} != bundle mod_id {mod_id!r}", "make both ids identical")
    if not isinstance(data.get("version"), str):
        report.error("compat.manifest", rel, "'version' must be a string", "set the mod version")
    depends = data.get("depends")
    if depends is not None and not isinstance(depends, dict):
        report.error("compat.manifest", rel, "'depends' must be an object", "map dependency id to a version range")


def _check_neoforge_manifest(data, rel, report, mod_id):
    mods = data.get("mods") if isinstance(data, dict) else None
    if not isinstance(mods, list) or not mods:
        report.error("compat.manifest", rel, "neoforge.mods.toml requires at least one [[mods]] table", "add a [[mods]] table with modId")
        return
    declared = mods[0].get("modId") if isinstance(mods[0], dict) else None
    if not isinstance(declared, str):
        report.error("compat.manifest", rel, "[[mods]].modId must be a string", "set modId")
    elif mod_id and declared != mod_id:
        report.error("compat.mismatch", rel, f"neoforge modId {declared!r} != bundle mod_id {mod_id!r}", "make both ids identical")


def validate_project(root, report=None):
    """Validate a native mod project. Returns a `Report`; `report.ok` is the verdict."""
    report = report or Report()
    root = Path(root)
    if not root.is_dir():
        report.error("usage.root_missing", str(root), "project directory does not exist", "pass an existing directory")
        return report

    # --- bundle ----------------------------------------------------------
    bundle_path = root / BUNDLE_FILE
    bundle = None
    if not bundle_path.is_file():
        report.error(
            "bundle.missing",
            BUNDLE_FILE,
            f"missing {BUNDLE_FILE}",
            f"run `tools/megu_mod_dev.py init <mod_id> --out {root}` or add the bundle document",
        )
    else:
        bundle = _read_json(bundle_path, report, BUNDLE_FILE)
        if bundle is not None:
            validator = BundleValidator(report)
            validator.validate(bundle)
            report.mod_id = validator.mod_id

    # --- package manifest -------------------------------------------------
    manifest_path = root / MANIFEST_FILE
    manifest = None
    if not manifest_path.is_file():
        report.error(
            "manifest.missing",
            MANIFEST_FILE,
            f"missing {MANIFEST_FILE}",
            "add a [mod] table with id, name, version, mc_version_range, authors, license, description",
        )
    else:
        try:
            manifest = parse_toml(manifest_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, ValueError) as error:
            report.error("manifest.parse", MANIFEST_FILE, f"invalid TOML: {error}", "fix the TOML syntax")
            manifest = None
        if manifest is not None:
            _validate_manifest(manifest, bundle, report)

    # --- project tree -----------------------------------------------------
    file_count = 0
    for path in project_files(root):
        file_count += 1
        if file_count > MAX_PROJECT_FILES:
            report.error("path.too_many_files", str(root), f"more than {MAX_PROJECT_FILES} files", "pack the project and inspect the archive instead")
            break
        rel = path.relative_to(root).as_posix()
        if path.is_symlink() and not _inside(root, path.resolve()):
            report.error("path.symlink_escape", rel, "symlink resolves outside the project root", "remove the symlink or keep its target inside the project")
            continue
        suffix = path.suffix.lower()
        if suffix in UNSAFE_EXTENSIONS:
            report.error(
                "path.unsafe_extension",
                rel,
                f"unsafe payload extension '{suffix}'",
                "native mods are data-only; remove executable/native/Java artifacts",
            )
            continue
        if suffix == ".json":
            data = _read_json(path, report, rel)
            if data is None:
                continue
            parts = rel.split("/")
            if len(parts) >= 4 and parts[0] == "data" and parts[2] == "recipe":
                _check_recipe(data, rel, report)
            elif len(parts) >= 5 and parts[0] == "data" and parts[2] == "tags":
                _check_tag(data, rel, report)
            elif len(parts) == 4 and parts[0] == "assets" and parts[2] == "lang":
                _check_lang(data, rel, report)
            elif len(parts) == 4 and parts[0] == "assets" and parts[2] == "blockstates":
                _check_blockstate(data, rel, report)
            elif rel == "pack.mcmeta":
                _check_pack_mcmeta(data, rel, report)
            elif rel == "fabric.mod.json":
                _check_fabric_manifest(data, rel, report, report.mod_id)
        elif rel == "META-INF/neoforge.mods.toml":
            try:
                data = parse_toml(path.read_text(encoding="utf-8"))
            except (OSError, UnicodeDecodeError, ValueError) as error:
                report.error("compat.manifest", rel, f"invalid TOML: {error}", "fix the NeoForge manifest syntax")
                continue
            _check_neoforge_manifest(data, rel, report, report.mod_id)
        elif len(rel.split("/")) == 3 and rel.split("/")[0] == "shaderpacks" and path.name == "megu-shader.toml":
            try:
                parse_toml(path.read_text(encoding="utf-8"))
            except (OSError, UnicodeDecodeError, ValueError) as error:
                report.error("shader.parse", rel, f"invalid TOML: {error}", "fix the shader pack manifest syntax")

    # --- packaging sanity -------------------------------------------------
    has_data = (root / "data").is_dir()
    if has_data and not (root / "pack.mcmeta").is_file():
        report.warning("pack_mcmeta.missing", "pack.mcmeta", "data/ present but pack.mcmeta is missing", "add pack.mcmeta so the datapack loads")
    mod_id = report.mod_id
    if mod_id and (root / "data").is_dir():
        namespace = mod_id.split(":", 1)[0] if ":" in mod_id else mod_id
        if not (root / "data" / namespace).is_dir():
            report.warning(
                "layout.namespace",
                f"data/{namespace}",
                f"no data/{namespace}/ directory for mod id {mod_id!r}",
                "create data/<namespace>/recipe, data/<namespace>/tags, and assets/<namespace>/lang",
            )
        lang_dir = root / "assets" / namespace / "lang"
        if lang_dir.is_dir() and not (lang_dir / "en_us.json").is_file():
            report.warning("lang.missing_en_us", f"assets/{namespace}/lang/en_us.json", "no en_us.json locale", "add en_us.json as the fallback locale")
    return report


def _validate_manifest(manifest, bundle, report):
    if not isinstance(manifest, dict):
        report.error("manifest.field", MANIFEST_FILE, "manifest root must be a table", "add a [mod] table")
        return
    schema = manifest.get("schema")
    if schema != PACKAGE_SCHEMA:
        report.error("manifest.schema", MANIFEST_FILE, f"schema must be {PACKAGE_SCHEMA!r}, found {schema!r}", f"set schema = \"{PACKAGE_SCHEMA}\"")
    mod = manifest.get("mod")
    if not isinstance(mod, dict):
        report.error("manifest.field", MANIFEST_FILE, "missing [mod] table", "add [mod] with id, name, version, mc_version_range, authors, license, description")
        return
    for key in ("id", "name", "version", "mc_version_range", "license", "description"):
        if not isinstance(mod.get(key), str) or not mod.get(key, "").strip():
            report.error("manifest.field", f"{MANIFEST_FILE}#/mod/{key}", f"'{key}' must be a non-empty string", f"set mod.{key}")
    authors = mod.get("authors")
    if not isinstance(authors, list) or not authors or not all(isinstance(author, str) and author.strip() for author in authors):
        report.error("manifest.field", f"{MANIFEST_FILE}#/mod/authors", "'authors' must be a non-empty array of strings", "list at least one author")
    mod_id = mod.get("id")
    if isinstance(mod_id, str):
        if ":" in mod_id or not SAFE_MOD_ID.match(mod_id):
            report.error("mod_id.invalid", f"{MANIFEST_FILE}#/mod/id", f"mod id {mod_id!r} is not a safe id", "use lowercase [a-z0-9_.-], starting with a letter, no ':'")
        if bundle is not None:
            bundle_id = bundle.get("mod_id") if isinstance(bundle, dict) else None
            if isinstance(bundle_id, str) and bundle_id != mod_id:
                report.error(
                    "manifest.mismatch",
                    f"{MANIFEST_FILE}#/mod/id",
                    f"manifest id {mod_id!r} != bundle mod_id {bundle_id!r}",
                    "make megu-mod.toml and megu-mod-bundle.json declare the same id",
                )
    version = mod.get("version")
    if isinstance(version, str) and not SEMVER.match(version):
        report.error("manifest.version", f"{MANIFEST_FILE}#/mod/version", f"version {version!r} is not SemVer", "use MAJOR.MINOR.PATCH, e.g. 0.1.0")
    declared_range = mod.get("mc_version_range")
    if isinstance(declared_range, str):
        reason = version_range_error(declared_range)
        if reason:
            report.error("manifest.version_range", f"{MANIFEST_FILE}#/mod/mc_version_range", f"{declared_range!r} is not a version range: {reason}", "use e.g. \"[1.21.1,)\"")
        if bundle is not None and isinstance(bundle.get("mc_version_range"), str):
            if bundle["mc_version_range"] != declared_range:
                report.error(
                    "manifest.mismatch",
                    f"{MANIFEST_FILE}#/mod/mc_version_range",
                    f"manifest range {declared_range!r} != bundle range {bundle['mc_version_range']!r}",
                    "declare the same mc_version_range in both files",
                )
    bundle_table = manifest.get("bundle")
    if not isinstance(bundle_table, dict):
        report.error("manifest.field", f"{MANIFEST_FILE}#/bundle", "missing [bundle] table", "add [bundle] with file = \"megu-mod-bundle.json\"")
    else:
        filename = bundle_table.get("file")
        if filename != BUNDLE_FILE:
            report.error("manifest.field", f"{MANIFEST_FILE}#/bundle/file", f"'file' must be {BUNDLE_FILE!r}", f"set file = \"{BUNDLE_FILE}\"")
        if isinstance(filename, str) and (".." in filename.split("/") or filename.startswith("/")):
            report.error("path.traversal", f"{MANIFEST_FILE}#/bundle/file", f"bundle path {filename!r} escapes the project", "keep the bundle file at the project root")


# ---------------------------------------------------------------------------
# Templates
# ---------------------------------------------------------------------------


def pack_mcmeta(mc_version, name):
    return {
        "pack": {
            "pack_format": PACK_FORMATS.get(mc_version, 48),
            "description": f"{name} native data ({mc_version})",
        }
    }


def bundle_document(mod_id, mc_version, creatures=(), channels=()):
    return {
        "schema": BUNDLE_SCHEMA,
        "mod_id": mod_id,
        "mc_version_range": f"[{mc_version},)",
        "portals": [],
        "progression_barriers": [],
        "biome_regions": [],
        "creatures": list(creatures),
        "custom_channels": list(channels),
    }


def manifest_document(mod_id, name, mc_version, description, license_name="MIT", authors=("Example Author",)):
    return {
        "schema": PACKAGE_SCHEMA,
        "mod": {
            "id": mod_id,
            "name": name,
            "version": "0.1.0",
            "mc_version_range": f"[{mc_version},)",
            "authors": list(authors),
            "license": license_name,
            "description": description,
        },
        "bundle": {"file": BUNDLE_FILE},
    }


def render_manifest_toml(manifest):
    mod = manifest["mod"]
    lines = [
        "# Native Megu mod package manifest. Read by tools/megu_mod_dev.py;",
        "# the server consumes megu-mod-bundle.json.",
        f"schema = {dump_toml_string(manifest['schema'])}",
        "",
        "[mod]",
        f"id = {dump_toml_string(mod['id'])}",
        f"name = {dump_toml_string(mod['name'])}",
        f"version = {dump_toml_string(mod['version'])}",
        f"mc_version_range = {dump_toml_string(mod['mc_version_range'])}",
        "authors = [" + ", ".join(dump_toml_string(author) for author in mod["authors"]) + "]",
        f"license = {dump_toml_string(mod['license'])}",
        f"description = {dump_toml_string(mod['description'])}",
        "",
        "[bundle]",
        f"file = {dump_toml_string(manifest['bundle']['file'])}",
        "",
    ]
    return "\n".join(lines)


def render_neoforge_toml(manifest):
    mod = manifest["mod"]
    return "\n".join(
        [
            f'modLoader = "javafml"',
            f'loaderVersion = "[4,)"',
            f"license = {dump_toml_string(mod['license'])}",
            "",
            "[[mods]]",
            f"modId = {dump_toml_string(mod['id'])}",
            f"version = {dump_toml_string(mod['version'])}",
            f"displayName = {dump_toml_string(mod['name'])}",
            f"description = {dump_toml_string(mod['description'])}",
            "",
            f"[[dependencies.{mod['id']}]]",
            'modId = "neoforge"',
            'type = "required"',
            'versionRange = "[21.1,)"',
            'ordering = "NONE"',
            'side = "BOTH"',
            "",
            f"[[dependencies.{mod['id']}]]",
            'modId = "minecraft"',
            'type = "required"',
            f"versionRange = {dump_toml_string(mod['mc_version_range'])}",
            'ordering = "NONE"',
            'side = "BOTH"',
            "",
        ]
    )


def render_fabric_manifest(manifest):
    mod = manifest["mod"]
    document = {
        "schemaVersion": 1,
        "id": mod["id"],
        "version": mod["version"],
        "name": mod["name"],
        "description": mod["description"],
        "license": mod["license"],
        "environment": "*",
        "depends": {
            "minecraft": mod["mc_version_range"],
            "fabricloader": ">=0.16.0",
            "java": ">=21",
        },
    }
    return json.dumps(document, indent=2, ensure_ascii=False) + "\n"


JA_LABELS = {
    "ruby": "ルビー",
    "ruby_block": "ルビーブロック",
    "gem_press": "宝石プレス",
    "ruby_boar": "ルビーイノシシ",
}


def _label(token):
    return " ".join(part.capitalize() for part in token.split("_"))


def _lang_entries(mod_id, name, blocks, entities, items):
    entries = {f"itemGroup.{mod_id}.main": name}
    for token in blocks:
        entries[f"block.{mod_id}.{token}"] = _label(token)
        entries[f"item.{mod_id}.{token}"] = _label(token)
    for token in items:
        entries[f"item.{mod_id}.{token}"] = _label(token)
    for token in entities:
        entries[f"entity.{mod_id}.{token}"] = _label(token)
    return entries


def _lang_ja_entries(mod_id, name, blocks, entities, items):
    entries = {f"itemGroup.{mod_id}.main": name}
    for token in blocks:
        label = JA_LABELS.get(token, _label(token))
        entries[f"block.{mod_id}.{token}"] = label
        entries[f"item.{mod_id}.{token}"] = label
    for token in items:
        entries[f"item.{mod_id}.{token}"] = JA_LABELS.get(token, _label(token))
    for token in entities:
        entries[f"entity.{mod_id}.{token}"] = JA_LABELS.get(token, _label(token))
    return entries


STARTER_FILES = {
    "pack.mcmeta": None,
    "data/{id}/recipe/ruby_block.json": {
        "type": "minecraft:crafting_shaped",
        "pattern": ["RRR", "RRR", "RRR"],
        "key": {"R": {"item": "{id}:ruby"}},
        "result": {"id": "{id}:ruby_block", "count": 1},
    },
    "data/{id}/recipe/ruby.json": {
        "type": "minecraft:crafting_shapeless",
        "ingredients": [{"item": "{id}:ruby_block"}],
        "result": {"id": "{id}:ruby", "count": 9},
    },
    "data/{id}/recipe/ruby_from_smelting.json": {
        "type": "minecraft:smelting",
        "ingredient": {"item": "minecraft:redstone_block"},
        "result": {"id": "{id}:ruby", "count": 1},
        "experience": 0.7,
        "cookingtime": 200,
    },
    "data/c/tags/item/gems/ruby.json": {"replace": False, "values": ["{id}:ruby"]},
    "assets/{id}/blockstates/ruby_block.json": {
        "variants": {"": {"model": "{id}:block/ruby_block"}}
    },
    "assets/{id}/models/block/ruby_block.json": {
        "parent": "minecraft:block/cube_all",
        "textures": {"all": "{id}:block/ruby_block"},
    },
    "assets/{id}/models/item/ruby.json": {
        "parent": "minecraft:item/generated",
        "textures": {"layer0": "{id}:item/ruby"},
    },
    "assets/{id}/models/item/ruby_block.json": {"parent": "{id}:block/ruby_block"},
    "assets/{id}/lang/en_us.json": None,
    "assets/{id}/lang/ja_jp.json": None,
}

MACHINE_FILES = dict(STARTER_FILES)
MACHINE_FILES.update(
    {
        "data/{id}/recipe/gem_press.json": {
            "type": "minecraft:crafting_shaped",
            "pattern": ["III", "IRI", "III"],
            "key": {"I": {"item": "minecraft:iron_ingot"}, "R": {"item": "{id}:ruby"}},
            "result": {"id": "{id}:gem_press", "count": 1},
        },
        "data/{id}/recipe/press_ruby_block.json": {
            "type": "{id}:pressing",
            "ingredient": {"item": "{id}:ruby"},
            "result": {"id": "{id}:ruby_block", "count": 1},
            "processing_time": 100,
        },
        "assets/{id}/blockstates/gem_press.json": {
            "variants": {"": {"model": "{id}:block/gem_press"}}
        },
        "assets/{id}/models/block/gem_press.json": {
            "parent": "minecraft:block/cube_all",
            "textures": {"all": "{id}:block/gem_press"},
        },
        "assets/{id}/models/item/gem_press.json": {"parent": "{id}:block/gem_press"},
    }
)

CREATURE_FILES = dict(STARTER_FILES)
CREATURE_FILES.update(
    {
        "data/{id}/tags/item/ruby_boar_food.json": {
            "replace": False,
            "values": ["#c:gems/ruby", "minecraft:apple"],
        },
        "assets/{id}/models/item/ruby_boar_spawn_egg.json": {
            "parent": "minecraft:item/template_spawn_egg"
        },
    }
)

SHADER_FILES = {
    "pack.mcmeta": None,
    "shaderpacks/{id}/megu-shader.toml": None,
    "shaderpacks/{id}/final.wgsl": None,
    "assets/{id}/lang/en_us.json": None,
    "assets/{id}/lang/ja_jp.json": None,
}

TEMPLATES = {
    "starter": {
        "name": "Ruby Starter",
        "description": "Native ruby mod: block, item, shaped/smelting recipes, unified #c:gems/ruby tag, and two locales.",
        "creatures": (),
        "channels": (),
        "blocks": ("ruby_block",),
        "entities": (),
        "items": ("ruby",),
        "files": STARTER_FILES,
        "readme": """# {name} (`{id}`)

Native Megu mod. No Gradle, no JDK, no Mixin: `megu-mod-bundle.json` is the
declarative mod document and `data/` + `assets/` are standard Minecraft data
and resource-pack trees.

```sh
python3 tools/megu_mod_dev.py validate .
python3 tools/megu_mod_dev.py pack . --deterministic
python3 tools/megu_mod_dev.py watch . --mods-dir ~/megu-server/mods
```

Add textures under `assets/{id}/textures/{block,item}/` (this starter ships
models and blockstates only; no Mojang assets are bundled).
""",
    },
    "machine": {
        "name": "Gem Press Machine",
        "description": "Native machine mod: crafted press block, custom pressing recipe, and a sync channel id.",
        "creatures": (),
        "channels": ("{id}:press_sync",),
        "blocks": ("ruby_block", "gem_press"),
        "entities": (),
        "items": ("ruby",),
        "files": MACHINE_FILES,
        "readme": """# {name} (`{id}`)

Adds a `{id}:gem_press` block and a namespaced `{id}:pressing` recipe type.
Item/fluid/energy transfer between blocks is provided by
`mod-adapter-core/src/capability_bus.rs` (`BlockCapabilityBus`); register a
handler for the press from a native adapter rather than shipping Java code.

```sh
python3 tools/megu_mod_dev.py validate .
python3 tools/megu_mod_dev.py watch . --mods-dir ~/megu-server/mods --once
```
""",
    },
    "creature": {
        "name": "Ruby Boar",
        "description": "Native creature mod: tameable ruby boar, food tag, spawn-egg model, and unified ruby tag.",
        "creatures": ("{id}:ruby_boar",),
        "channels": (),
        "blocks": ("ruby_block",),
        "entities": ("ruby_boar",),
        "items": ("ruby",),
        "files": CREATURE_FILES,
        "readme": """# {name} (`{id}`)

Declares a tameable, rideable `{id}:ruby_boar` in the bundle `creatures`
section (movement, tame/breed items, healing). Taming and breeding run on
`mod-adapter-core/src/creature_primitives.rs`; no JVM is involved.

```sh
python3 tools/megu_mod_dev.py validate .
python3 tools/megu_mod_dev.py pack . --deterministic
```
""",
    },
    "shader": {
        "name": "Ruby Shader Pack",
        "description": "Native WGSL shader pack scaffold (`megu-shader.toml` + final.wgsl) with a minimal data bundle.",
        "creatures": (),
        "channels": (),
        "blocks": (),
        "entities": (),
        "items": (),
        "files": SHADER_FILES,
        "readme": """# {name} (`{id}`)

Native Megu shader pack: `shaderpacks/{id}/megu-shader.toml` declares the
passes and `final.wgsl` is the fragment/vertex module. Drop the packed
`.mmod` into the server `mods/` directory and the pack into `shaderpacks/`.

```sh
python3 tools/megu_mod_dev.py validate .
python3 tools/megu_mod_dev.py pack . --deterministic
```

The WGSL here is the minimal full-screen triangle from
`docs/mod-author-guide.md`; replace it with the real pass graph when the
render graph inputs are known.
""",
    },
}


def creature_entry(mod_id):
    return {
        "entity_type": f"{mod_id}:ruby_boar",
        "max_health": 20.0,
        "tame_items": [f"#{mod_id}:ruby_boar_food"],
        "tame_chance": 0.35,
        "breed_items": [f"#{mod_id}:ruby_boar_food"],
        "heal_items": [{"item": "#c:gems/ruby", "heal": 2.0}],
        "baby_growth_ticks": DEFAULT_BABY_GROWTH_TICKS,
        "breed_cooldown_ticks": DEFAULT_BREED_COOLDOWN_TICKS,
        "supports_wander_mode": True,
        "can_shoulder_perch": False,
        "rideable_when_tamed": True,
        "movement": {"kind": "land", "ground_speed": 0.25},
    }


def shader_pack_toml(mod_id, name):
    return "\n".join(
        [
            '[pack]',
            f'name = "{name}"',
            'author = "Example Author"',
            'version = "0.1.0"',
            "api_version = 1",
            "requires_compute = false",
            "requires_ssbo = false",
            "supports_lod = false",
            "",
            "[pass.final]",
            'stage = "final"',
            'kind = "graphics"',
            'file = "final.wgsl"',
            "",
        ]
    )


FINAL_WGSL = """@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let xy = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    return vec4<f32>(xy[vertex_index], 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(0.15, 0.25, 0.45, 1.0);
}
"""


# ---------------------------------------------------------------------------
# init
# ---------------------------------------------------------------------------


def schema_source():
    """Locate `sdk/schemas/megu-mod-bundle-v1.schema.json` near this checkout."""
    candidates = [
        Path(__file__).resolve().parents[1] / "sdk" / "schemas" / "megu-mod-bundle-v1.schema.json",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def valid_new_mod_id(mod_id):
    if not SAFE_MOD_ID.match(mod_id or ""):
        return "must match ^[a-z][a-z0-9_.-]{0,63}$ (lowercase, starts with a letter, no ':' or path separators)"
    if ".." in mod_id or "/" in mod_id or "\\" in mod_id:
        return "must not contain path traversal or separators"
    return None


def cmd_init(args):
    mod_id = args.mod_id
    reason = valid_new_mod_id(mod_id)
    if reason:
        message = f"invalid mod id {mod_id!r}: {reason}"
        if args.json:
            print(json.dumps({"ok": False, "mod_id": None, "diagnostics": [
                Diagnostic("mod_id.invalid", mod_id, message, "use e.g. 'ruby_starter'").to_dict()
            ]}, indent=2))
        else:
            print(f"error: {message}", file=sys.stderr)
        return 2
    template = TEMPLATES[args.template]
    out = Path(args.out).expanduser()
    if out.exists() and any(out.iterdir()):
        if not args.force:
            message = f"{out} is not empty; refusing to overwrite"
            if args.json:
                print(json.dumps({"ok": False, "mod_id": mod_id, "diagnostics": [
                    Diagnostic("usage.target_exists", str(out), message, "pass --force or choose another --out").to_dict()
                ]}, indent=2))
            else:
                print(f"error: {message} (pass --force to overwrite)", file=sys.stderr)
            return 2
    name = template["name"]
    description = template["description"]
    manifest = manifest_document(mod_id, name, args.mc_version, description)
    creatures = [creature_entry(mod_id)] if template["creatures"] else []
    channels = [channel.format(id=mod_id) for channel in template["channels"]]
    bundle = bundle_document(mod_id, args.mc_version, creatures, channels)

    files = {}
    for relative, template_value in template["files"].items():
        target = relative.format(id=mod_id)
        if target.endswith("lang/en_us.json"):
            value = _lang_entries(mod_id, name, template["blocks"], template["entities"], template["items"])
        elif target.endswith("lang/ja_jp.json"):
            value = _lang_ja_entries(mod_id, name, template["blocks"], template["entities"], template["items"])
        elif target == "pack.mcmeta":
            value = pack_mcmeta(args.mc_version, name)
        elif target.endswith("megu-shader.toml"):
            files[target] = shader_pack_toml(mod_id, name)
            continue
        elif target.endswith(".wgsl"):
            files[target] = FINAL_WGSL
            continue
        elif template_value is None:
            continue
        else:
            value = json.loads(json.dumps(template_value).replace("{id}", mod_id))
        files[target] = json.dumps(value, indent=2, ensure_ascii=False) + "\n"

    files[MANIFEST_FILE] = render_manifest_toml(manifest)
    files[BUNDLE_FILE] = json.dumps(bundle, indent=2, ensure_ascii=False) + "\n"
    files["README.md"] = template["readme"].replace("{id}", mod_id).replace("{name}", name)
    if args.compat_jar:
        files["META-INF/neoforge.mods.toml"] = render_neoforge_toml(manifest)
        files["fabric.mod.json"] = render_fabric_manifest(manifest)

    out.mkdir(parents=True, exist_ok=True)
    for relative, text in sorted(files.items()):
        target = out / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    source = schema_source()
    if source is not None:
        schema_target = out / "schemas" / source.name
        schema_target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, schema_target)
        editor = out / ".vscode" / "settings.json"
        editor.parent.mkdir(parents=True, exist_ok=True)
        editor.write_text(
            json.dumps(
                {
                    "json.schemas": [
                        {
                            "fileMatch": [f"/{BUNDLE_FILE}", f"**/{BUNDLE_FILE}"],
                            "url": "./schemas/megu-mod-bundle-v1.schema.json",
                        }
                    ]
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )

    report = validate_project(out)
    if args.json:
        print(json.dumps({"ok": report.ok, "mod_id": mod_id, "out": str(out), "template": args.template,
                          "files": sorted(files), "diagnostics": [d.to_dict() for d in report.diagnostics]}, indent=2))
    else:
        print(f"created {len(files)} files in {out} (template: {args.template})")
        if args.compat_jar:
            print("included NeoForge + Fabric manifests (--compat-jar)")
        print(f"next: python3 tools/megu_mod_dev.py validate {out}")
    return 0 if report.ok else 1


# ---------------------------------------------------------------------------
# validate
# ---------------------------------------------------------------------------


def cmd_validate(args):
    report = validate_project(Path(args.dir))
    if args.json:
        print(json.dumps(report.to_dict(), indent=2))
    else:
        verdict = "OK" if report.ok else "FAILED"
        print(report.render(f"{verdict}: {Path(args.dir)}"))
    return 0 if report.ok else 1


# ---------------------------------------------------------------------------
# pack
# ---------------------------------------------------------------------------


def collect_pack_entries(root, compat_jar):
    """Relative archive names for a package, sorted, with any escape rejected."""
    entries = []
    for name in PACK_FILES:
        path = root / name
        if path.is_file() and path.suffix.lower() not in UNSAFE_EXTENSIONS:
            entries.append(name)
    for pattern in ("LICENSE*", "COPYING*", "NOTICE*"):
        entries.extend(sorted(path.name for path in root.glob(pattern) if path.is_file()))
    for top in PACK_ROOTS:
        directory = root / top
        if not directory.is_dir():
            continue
        for path in sorted(directory.rglob("*")):
            if not path.is_file():
                continue
            relative = path.relative_to(root)
            if any(part in SKIP_DIRS for part in relative.parts):
                continue
            if path.suffix.lower() in UNSAFE_EXTENSIONS:
                continue
            entries.append(relative.as_posix())
    if compat_jar:
        for name in ("fabric.mod.json", "META-INF/neoforge.mods.toml"):
            if (root / name).is_file():
                entries.append(name)
        meta = root / "META-INF"
        if meta.is_dir():
            for path in sorted(meta.rglob("*")):
                if path.is_file() and path.relative_to(root).as_posix() not in entries:
                    entries.append(path.relative_to(root).as_posix())
    unique = []
    for name in entries:
        if name not in unique:
            unique.append(name)
    return sorted(unique)


def _generated_entries(root, compat_jar):
    """Manifest texts to synthesize when the project has no Java manifests."""
    if not compat_jar:
        return {}
    manifest_path = root / MANIFEST_FILE
    if not manifest_path.is_file():
        return {}
    try:
        manifest = parse_toml(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, ValueError):
        return {}
    if not isinstance(manifest, dict) or not isinstance(manifest.get("mod"), dict):
        return {}
    generated = {}
    if not (root / "fabric.mod.json").is_file():
        generated["fabric.mod.json"] = render_fabric_manifest(manifest)
    if not (root / "META-INF" / "neoforge.mods.toml").is_file():
        generated["META-INF/neoforge.mods.toml"] = render_neoforge_toml(manifest)
    return generated


def _zip_time(path):
    """Source mtime clamped into the ZIP 1980..2107 range."""
    try:
        stamp = time.localtime(path.stat().st_mtime)[:6]
    except OSError:
        return FIXED_ZIP_TIME
    if (stamp[0], stamp[1], stamp[2]) < (1980, 1, 1):
        return FIXED_ZIP_TIME
    return stamp


def build_archive(root, out_path, compat_jar, deterministic):
    """Write a ZIP atomically; return (entries, bytes, sha256, sha512)."""
    generated = _generated_entries(root, compat_jar)
    entries = collect_pack_entries(root, compat_jar)
    for name in generated:
        if name not in entries:
            entries.append(name)
    entries = sorted(entries)
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    part = out_path.with_name(out_path.name + ".part")
    try:
        with zipfile.ZipFile(part, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name in entries:
                if name in generated:
                    payload = generated[name].encode("utf-8")
                else:
                    source = root / name
                    if source.is_symlink() and not _inside(root, source.resolve()):
                        raise ValueError(f"{name} resolves outside the project root")
                    payload = source.read_bytes()
                info = zipfile.ZipInfo(name, date_time=FIXED_ZIP_TIME if deterministic else _zip_time(root / name))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o644 << 16
                archive.writestr(info, payload)
        os.replace(part, out_path)
    finally:
        if part.exists():
            part.unlink()
    payload = out_path.read_bytes()
    return entries, len(payload), hashlib.sha256(payload).hexdigest(), hashlib.sha512(payload).hexdigest()


def _inside(root, candidate):
    """True when `candidate` resolves inside `root`."""
    try:
        Path(candidate).resolve().relative_to(Path(root).resolve())
    except ValueError:
        return False
    return True


def default_out_path(root, compat_jar):
    manifest_path = root / MANIFEST_FILE
    mod_id = root.name
    version = "0.1.0"
    try:
        manifest = parse_toml(manifest_path.read_text(encoding="utf-8"))
        mod = manifest.get("mod") if isinstance(manifest, dict) else None
        if isinstance(mod, dict):
            mod_id = mod.get("id", mod_id) or mod_id
            version = mod.get("version", version) or version
    except (OSError, UnicodeDecodeError, ValueError):
        pass
    suffix = ".jar" if compat_jar else ".mmod"
    return root / "dist" / f"{mod_id}-{version}{suffix}"


def _pack(root, out_path, compat_jar, deterministic):
    entries, size, sha256, sha512 = build_archive(root, out_path, compat_jar, deterministic)
    return {
        "out": str(out_path),
        "bytes": size,
        "entries": entries,
        "sha256": sha256,
        "sha512": sha512,
    }


def cmd_pack(args):
    root = Path(args.dir)
    report = validate_project(root)
    if not report.ok:
        if args.json:
            print(json.dumps({"ok": False, "out": None, "diagnostics": [d.to_dict() for d in report.diagnostics]}, indent=2))
        else:
            print(report.render(f"refusing to pack: {root}"))
        return 1
    out = Path(args.out) if args.out else default_out_path(root, args.compat_jar)
    if args.compat_jar and out.suffix != ".jar":
        report.warning("pack.suffix", str(out), "--compat-jar normally writes a .jar", "use --out dist/<mod_id>-<version>.jar")
    try:
        summary = _pack(root, out, args.compat_jar, args.deterministic)
    except (OSError, ValueError) as error:
        if args.json:
            print(json.dumps({"ok": False, "out": None, "diagnostics": [
                Diagnostic("pack.failed", str(out), str(error), "check the output path and project permissions").to_dict()
            ]}, indent=2))
        else:
            print(f"error: packing failed: {error}", file=sys.stderr)
        return 1
    if args.json:
        summary["ok"] = True
        summary["diagnostics"] = [d.to_dict() for d in report.diagnostics]
        print(json.dumps(summary, indent=2))
    else:
        for diagnostic in report.diagnostics:
            print(f"  {diagnostic.severity}: [{diagnostic.code}] {diagnostic.path}: {diagnostic.message}")
        print(f"packed {len(summary['entries'])} entries -> {summary['out']} ({summary['bytes']} bytes)")
        print(f"sha256 {summary['sha256']}")
        print(f"sha512 {summary['sha512']}")
        print("drop it into the server mods/ directory; Modrinth accepts the sha256/sha512 above")
    return 0


# ---------------------------------------------------------------------------
# watch
# ---------------------------------------------------------------------------


def _fingerprint(root):
    state = {}
    for name in (MANIFEST_FILE, BUNDLE_FILE, "pack.mcmeta", "README.md"):
        path = root / name
        if path.is_file():
            stat = path.stat()
            state[name] = (stat.st_mtime_ns, stat.st_size)
    for pattern in ("LICENSE*", "COPYING*", "NOTICE*"):
        for path in root.glob(pattern):
            if path.is_file():
                stat = path.stat()
                state[path.name] = (stat.st_mtime_ns, stat.st_size)
    for top in PACK_ROOTS:
        directory = root / top
        if not directory.is_dir():
            continue
        for path in directory.rglob("*"):
            if path.is_file():
                stat = path.stat()
                state[path.relative_to(root).as_posix()] = (stat.st_mtime_ns, stat.st_size)
    return state


def sync_once(root, mods_dir, compat_jar, deterministic=True):
    """Validate, then atomically install `<mod_id>-dev.mmod`; returns (ok, summary, report)."""
    report = validate_project(root)
    if not report.ok:
        return False, None, report
    manifest = parse_toml((root / MANIFEST_FILE).read_text(encoding="utf-8"))
    mod_id = manifest["mod"]["id"]
    suffix = ".jar" if compat_jar else ".mmod"
    target = Path(mods_dir) / f"{mod_id}-dev{suffix}"
    try:
        summary = _pack(root, target, compat_jar, deterministic)
    except (OSError, ValueError) as error:
        report.error("watch.failed", str(target), str(error), "check the mods directory permissions")
        return False, None, report
    return True, summary, report


def cmd_watch(args):
    root = Path(args.dir)
    mods_dir = Path(args.mods_dir)
    if args.interval <= 0:
        print("error: --interval must be > 0", file=sys.stderr)
        return 2
    mods_dir.mkdir(parents=True, exist_ok=True)
    if args.once:
        ok, summary, report = sync_once(root, mods_dir, args.compat_jar)
        if args.json:
            payload = report.to_dict()
            payload["ok"] = ok
            payload["out"] = summary["out"] if summary else None
            payload["sha256"] = summary["sha256"] if summary else None
            print(json.dumps(payload, indent=2))
        elif ok:
            print(f"synced {summary['out']} ({summary['bytes']} bytes)")
            print(f"sha256 {summary['sha256']}")
        else:
            print(report.render(f"not synced: {root}"))
        return 0 if ok else 1

    stamp = lambda: time.strftime("%H:%M:%S")
    previous = None
    last_ok = None
    print(f"watching {root} -> {mods_dir} every {args.interval}s (Ctrl-C to stop)")
    while True:
        state = _fingerprint(root)
        if previous is None or state != previous:
            ok, summary, report = sync_once(root, mods_dir, args.compat_jar)
            if ok:
                last_ok = summary["sha256"]
                print(f"[{stamp()}] synced {summary['out']} sha256={summary['sha256'][:16]}…")
            else:
                print(f"[{stamp()}] validation failed; keeping the previous package")
                print(report.render(f"  {root}"))
            previous = state
        try:
            time.sleep(args.interval)
        except KeyboardInterrupt:
            print(f"\nstopped; last good sha256 {last_ok or '(none)'}")
            return 0


# ---------------------------------------------------------------------------
# doctor
# ---------------------------------------------------------------------------


def cmd_doctor(args):
    checks = []

    def add(name, ok, detail, fix_hint):
        checks.append({"name": name, "status": "OK" if ok else "MISSING", "detail": detail, "fix_hint": "" if ok else fix_hint})

    version = sys.version_info
    add("python", version >= (3, 9), f"{version.major}.{version.minor}.{version.micro}", "install Python 3.9 or newer")
    add("tomllib", _tomllib is not None, "tomllib available" if _tomllib else "fallback TOML subset parser", "Python 3.11+ enables full TOML support")
    megu = shutil.which("megu")
    add("megu", megu is not None, megu or "not on PATH", "build with cargo build --bin megu, or run the dev tool standalone")
    checker = shutil.which("megu-mod-check")
    add("megu-mod-check", checker is not None, checker or "not on PATH", "cargo build --bin megu-mod-check enables JAR pre-flight checks")
    schema = schema_source()
    add("schema", schema is not None, str(schema) if schema else "sdk/schemas/megu-mod-bundle-v1.schema.json not found", "run from a full checkout so IDE autocomplete can resolve the schema")
    example = Path(__file__).resolve().parents[1] / "sdk" / "examples" / "ruby-starter"
    add("example", example.is_dir(), str(example) if example.is_dir() else "sdk/examples/ruby-starter missing", "the starter example is missing from this checkout")

    ok = all(check["status"] == "OK" for check in checks if check["name"] in ("python", "schema"))
    if args.json:
        print(json.dumps({"ok": ok, "tool_version": TOOL_VERSION, "checks": checks}, indent=2))
    else:
        print(f"megu mod dev doctor (tool {TOOL_VERSION})")
        for check in checks:
            print(f"  {check['status']:<7} {check['name']}: {check['detail']}")
            if check["fix_hint"]:
                print(f"          fix: {check['fix_hint']}")
        print("ready" if ok else "incomplete: fix the MISSING lines above")
    return 0 if ok else 1


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser():
    parser = argparse.ArgumentParser(
        prog="megu_mod_dev.py",
        description="Scaffold, validate, pack, and hot-reload native `.mmod` Megu mods (stdlib only, no cargo).",
        epilog="See docs/mod-sdk-guide.md and sdk/AGENTS.md for the full authoring contract.",
    )
    parser.add_argument("--version", action="version", version=f"megu_mod_dev {TOOL_VERSION}")
    subparsers = parser.add_subparsers(dest="command", required=True)

    init = subparsers.add_parser("init", help="scaffold a complete native mod workspace")
    init.add_argument("mod_id", help="lowercase mod id, e.g. ruby_starter")
    init.add_argument("--out", required=True, help="target project directory")
    init.add_argument("--template", choices=sorted(TEMPLATES), default="starter", help="starter|machine|creature|shader")
    init.add_argument("--mc-version", default="1.21.1", help="Minecraft target version (default 1.21.1)")
    init.add_argument("--compat-jar", action="store_true", help="also emit NeoForge + Fabric Java manifests")
    init.add_argument("--json", action="store_true", help="machine-readable report")
    init.add_argument("--force", action="store_true", help="write into a non-empty directory")
    init.set_defaults(func=cmd_init)

    validate = subparsers.add_parser("validate", help="validate bundle, manifest, and project trees")
    validate.add_argument("dir", help="project directory")
    validate.add_argument("--json", action="store_true", help="machine-readable {ok, mod_id, diagnostics}")
    validate.set_defaults(func=cmd_validate)

    pack = subparsers.add_parser("pack", help="validate then build a deterministic package")
    pack.add_argument("dir", help="project directory")
    pack.add_argument("--out", help="output path (default dist/<mod_id>-<version>.mmod)")
    pack.add_argument("--deterministic", action="store_true", help="fixed ZIP timestamp and sorted entries")
    pack.add_argument("--compat-jar", action="store_true", help="write a .jar with generated Java manifests")
    pack.add_argument("--json", action="store_true", help="machine-readable digest report")
    pack.set_defaults(func=cmd_pack)

    watch = subparsers.add_parser("watch", help="repack into a server mods/ directory on change")
    watch.add_argument("dir", help="project directory")
    watch.add_argument("--mods-dir", required=True, help="server mods/ directory")
    watch.add_argument("--interval", type=float, default=0.5, help="poll interval in seconds (default 0.5)")
    watch.add_argument("--once", action="store_true", help="run exactly one sync cycle (CI/tests)")
    watch.add_argument("--compat-jar", action="store_true", help="write <mod_id>-dev.jar instead of .mmod")
    watch.add_argument("--json", action="store_true", help="machine-readable report for --once")
    watch.set_defaults(func=cmd_watch)

    doctor = subparsers.add_parser("doctor", help="check the developer environment")
    doctor.add_argument("--json", action="store_true", help="machine-readable report")
    doctor.set_defaults(func=cmd_doctor)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
