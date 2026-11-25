#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Megu mod porter: convert a Java mod or Modrinth modpack into a native Megu project.

`port` reads a Fabric / NeoForge / Forge `.jar`, a Modrinth `.mrpack`, or an
already-extracted mod directory **without ever unpacking it to disk**, lifts
every data-driven resource into the standard `data/` + `assets/` trees, and
synthesizes `megu-mod.toml`, `megu-mod-bundle.json`, and `pack.mcmeta` — a
project `tools/megu_mod_dev.py validate` accepts verbatim. `--format
compat-jar|both` additionally writes a Modrinth-uploadable `.jar` carrying the
generated `META-INF/neoforge.mods.toml` + `fabric.mod.json` next to the native
bundle, reusing the exact `megu_mod_dev.py` packing path.

`analyze` prints the same classification report read-only: loader, mod id,
data/asset coverage, `.class` inventory grouped by detected role, embedded
jar-in-jar payloads, and every member the fail-closed reader rejected.

`--rust-scaffold` parses each `.class` constant pool in pure Python (no JVM,
no external tools), classifies each type by its Minecraft/loader superclass
and interfaces (custom block, item, block entity, entity, mixin, screen,
network payload, event listener, entrypoint), emits a `rust-port/` crate
skeleton against `sdk/megu-mod-sdk`, and writes `PORT_REPORT.md` with the
auto-ported vs. needs-porting split.

  tools/megu_port_mod.py port <input.jar|input.mrpack|mod_dir> --out <dir>
      [--format mmod|compat-jar|both] [--rust-scaffold] [--json]
  tools/megu_port_mod.py analyze <input.jar|input.mrpack|mod_dir> [--json]

Safety contract (mirrors mod-adapter-core's bounded readers, fail-closed):
  * Zip members with `..`, `.`, empty segments, absolute paths, drive letters,
    backslashes, or NUL bytes are rejected; so are encrypted members.
  * Per-member, per-archive, and aggregate uncompressed byte caps plus a
    member-count cap stop zip bombs before a single byte is decompressed.
  * Jar-in-Jar (`META-INF/jarjar/*.jar`, `META-INF/jars/*.jar`) is folded in
    recursively under bounded depth/count/byte budgets matching
    `loader_extensions::MAX_JAR_IN_JAR_*`.
  * Only the Python standard library is used. No network access, no
    subprocess execution, no temp extraction directories.
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import struct
import sys
import zipfile
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
import megu_mod_dev as dev  # noqa: E402  (sibling tool; conventions are shared)

TOOL_VERSION = "1.0.0"
REPORT_SCHEMA = "megu-port/1"

# --- Bounded reader limits (mirror mod-adapter-core budgets) -----------------
MAX_SOURCE_BYTES = 256 * 1024 * 1024      # input file size ceiling
MAX_MEMBERS = 65_536                      # archive member / dir file count
MAX_MEMBER_BYTES = 64 * 1024 * 1024       # one uncompressed member
MAX_TOTAL_BYTES = 512 * 1024 * 1024       # aggregate uncompressed archive
MAX_COMPRESSION_RATIO = 100               # classic zip-bomb tripwire
MAX_MANIFEST_BYTES = 4 * 1024 * 1024      # loader manifests / JSON we parse
MAX_CLASSES = 32_768                      # .class members we parse
MAX_NESTED_DEPTH = 3                      # loader_extensions::MAX_JAR_IN_JAR_DEPTH
MAX_NESTED_JARS = 64                      # ..._NESTED_JARS
MAX_NESTED_BYTES = 64 * 1024 * 1024       # ..._TOTAL_BYTES

EMBEDDED_JAR_DIRS = ("meta-inf/jarjar/", "meta-inf/jars/")
OVERRIDE_DIRS = ("overrides/", "clientoverrides/", "serveroverrides/")
MODPACK_INDEX = "modrinth.index.json"
CURSEFORGE_INDEX = "manifest.json"

PACKED_DATA_ROOTS = ("data", "assets")
ROOT_COPY_FILES = ("pack.mcmeta",)
ROOT_COPY_GLOBS = ("LICENSE*", "COPYING*", "NOTICE*")


class PortError(Exception):
    """Fatal input problem; reported as a diagnostic with exit code 1."""


class RejectedMember(Exception):
    """One member refused by the fail-closed reader; carries a reason."""


# ---------------------------------------------------------------------------
# Bounded archive reading (zero temp extraction)
# ---------------------------------------------------------------------------


def member_path_error(name):
    """Return a reason string when a member path is unsafe, else None.

    Mirrors modpack.rs `validate_member_path`: reject `..`, `.`, empty
    segments, absolute paths, Windows drive letters, backslashes, NUL.
    """
    if not name:
        return "empty member name"
    if "\x00" in name:
        return "NUL byte in member name"
    if "\\" in name:
        return "backslash path separator"
    if name.startswith("/"):
        return "absolute member path"
    if re.match(r"^[A-Za-z]:", name):
        return "drive-letter member path"
    body = name[:-1] if name.endswith("/") else name
    for segment in body.split("/"):
        if segment in ("", ".", ".."):
            return f"unsafe path segment {segment!r}"
    return None


def _read_member_bytes(archive, info, display):
    """Decompress one member under its byte cap; raises RejectedMember."""
    if info.flag_bits & 0x1:
        raise RejectedMember("encrypted member")
    if info.file_size > MAX_MEMBER_BYTES:
        raise RejectedMember(
            f"uncompressed size {info.file_size} exceeds {MAX_MEMBER_BYTES}"
        )
    if (
        info.compress_size > 0
        and info.file_size > info.compress_size * MAX_COMPRESSION_RATIO
        and info.file_size > 1024 * 1024
    ):
        raise RejectedMember(
            f"compression ratio {info.file_size // max(info.compress_size, 1)}:1 "
            "looks like a zip bomb"
        )
    try:
        with archive.open(info) as handle:
            payload = handle.read(MAX_MEMBER_BYTES + 1)
    except (OSError, zipfile.BadZipFile, RuntimeError, EOFError, zlib.error) as error:
        raise RejectedMember(f"cannot decompress: {error}")
    if len(payload) > MAX_MEMBER_BYTES:
        raise RejectedMember(f"member exceeds {MAX_MEMBER_BYTES} bytes while reading")
    return payload


def read_archive(payload, display):
    """Bounded in-memory read of a zip: returns {name: bytes}.

    Raises PortError on structural corruption, unsafe member names, or any
    per-member/aggregate cap being exceeded — fail-closed, never a silent skip.
    """
    if len(payload) > MAX_SOURCE_BYTES:
        raise PortError(f"{display}: archive of {len(payload)} bytes exceeds {MAX_SOURCE_BYTES}")
    try:
        archive = zipfile.ZipFile(io.BytesIO(payload))
        infos = archive.infolist()
    except zipfile.BadZipFile as error:
        raise PortError(f"{display}: not a readable zip archive: {error}")
    if len(infos) > MAX_MEMBERS:
        raise PortError(f"{display}: {len(infos)} members exceeds {MAX_MEMBERS}")
    members = {}
    total = 0
    for info in infos:
        if info.is_dir():
            continue
        error = member_path_error(info.filename)
        if error:
            raise PortError(f"{display}: member {info.filename!r} rejected: {error}")
        try:
            body = _read_member_bytes(archive, info, display)
        except RejectedMember as rejected:
            raise PortError(f"{display}: member {info.filename!r} rejected: {rejected}")
        total += len(body)
        if total > MAX_TOTAL_BYTES:
            raise PortError(
                f"{display}: aggregate {total} uncompressed bytes exceeds {MAX_TOTAL_BYTES}"
            )
        members[info.filename] = body
    archive.close()
    return members


def read_directory(root):
    """Read a mod directory into the same {name: bytes} shape as an archive."""
    root = Path(root)
    if not root.is_dir():
        raise PortError(f"{root}: not a directory")
    resolved_root = root.resolve()
    members = {}
    total = 0
    count = 0
    for directory, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(
            name for name in dirnames if name not in dev.SKIP_DIRS
        )
        for name in sorted(filenames):
            count += 1
            if count > MAX_MEMBERS:
                raise PortError(f"{root}: more than {MAX_MEMBERS} files")
            path = Path(directory) / name
            rel = path.relative_to(root).as_posix()
            error = member_path_error(rel)
            if error:
                raise PortError(f"{root}: member {rel!r} rejected: {error}")
            if path.is_symlink():
                try:
                    path.resolve().relative_to(resolved_root)
                except ValueError:
                    raise PortError(f"{rel}: symlink resolves outside the mod directory")
            try:
                size = path.stat().st_size
            except OSError as error:
                raise PortError(f"{rel}: cannot stat: {error}")
            if size > MAX_MEMBER_BYTES:
                raise PortError(f"{rel}: {size} bytes exceeds {MAX_MEMBER_BYTES}")
            total += size
            if total > MAX_TOTAL_BYTES:
                raise PortError(f"{root}: aggregate exceeds {MAX_TOTAL_BYTES}")
            try:
                members[rel] = path.read_bytes()
            except OSError as error:
                raise PortError(f"{rel}: cannot read: {error}")
    return members


# ---------------------------------------------------------------------------
# Minimal .class parser (constant pool only — no JVM required)
# ---------------------------------------------------------------------------


class ClassFormatError(Exception):
    """Not a parseable .class file."""


_CP_SKIP = {3: 4, 4: 4, 7: 2, 8: 2, 9: 4, 10: 4, 11: 4, 12: 4,
            15: 3, 16: 2, 17: 4, 18: 4, 19: 2, 20: 2}


class _Reader:
    def __init__(self, data):
        self.data = data
        self.pos = 0

    def take(self, count):
        if self.pos + count > len(self.data):
            raise ClassFormatError("truncated class file")
        chunk = self.data[self.pos:self.pos + count]
        self.pos += count
        return chunk

    def u1(self):
        return self.take(1)[0]

    def u2(self):
        return struct.unpack(">H", self.take(2))[0]

    def u4(self):
        return struct.unpack(">I", self.take(4))[0]


def parse_class(data):
    """Parse one .class payload; returns a dict of the fields we classify on.

    Reads the constant pool (UTF-8 and Class entries resolved), this/super
    class names, direct interfaces, and method names. Anything else is skipped
    by length. Raises ClassFormatError on bad magic, unknown tags, or
    truncation — callers record it in `rejected`, never crash.
    """
    reader = _Reader(data)
    if reader.u4() != 0xCAFEBABE:
        raise ClassFormatError("missing 0xCAFEBABE magic")
    minor = reader.u2()
    major = reader.u2()
    cp_count = reader.u2()
    pool = [None] * cp_count  # 1-indexed
    index = 1
    while index < cp_count:
        tag = reader.u1()
        if tag == 1:
            length = reader.u2()
            pool[index] = (tag, reader.take(length))
        elif tag in (5, 6):
            reader.take(8)
            index += 1  # long/double occupy two pool slots
        elif tag in _CP_SKIP:
            pool[index] = (tag, reader.take(_CP_SKIP[tag]))
        else:
            raise ClassFormatError(f"unknown constant-pool tag {tag}")
        index += 1

    def utf8(index):
        entry = pool[index] if 0 < index < cp_count else None
        if not entry or entry[0] != 1:
            raise ClassFormatError(f"constant #{index} is not UTF-8")
        return entry[1].decode("utf-8", errors="replace")

    def class_name(index):
        if index == 0:
            return None
        entry = pool[index] if 0 < index < cp_count else None
        if not entry or entry[0] != 7:
            raise ClassFormatError(f"constant #{index} is not a Class")
        return utf8(struct.unpack(">H", entry[1])[0])

    reader.u2()  # access_flags
    this_name = class_name(reader.u2())
    super_name = class_name(reader.u2())
    interfaces = [class_name(reader.u2()) for _ in range(reader.u2())]
    for _ in range(reader.u2()):  # fields
        reader.take(6)
        for _ in range(reader.u2()):
            reader.take(2)
            reader.take(reader.u4())
    methods = []
    for _ in range(reader.u2()):
        reader.take(2)  # access_flags
        name = utf8(reader.u2())
        reader.take(2)  # descriptor_index
        for _ in range(reader.u2()):
            reader.take(2)
            reader.take(reader.u4())
        if name not in ("<init>", "<clinit>"):
            methods.append(name)
    return {
        "name": this_name,
        "super": super_name,
        "interfaces": interfaces,
        "methods": methods,
        "major": major,
        "minor": minor,
        "constant_strings": [
            entry[1].decode("utf-8", errors="replace")
            for entry in pool
            if entry and entry[0] == 1
        ],
    }


# ---------------------------------------------------------------------------
# Loader manifests and .class classification
# ---------------------------------------------------------------------------


def _simple(internal):
    """`com/example/RubyBlock$Inner` -> `ruby_block$inner`-friendly simple name."""
    if not internal:
        return ""
    return internal.rsplit("/", 1)[-1]


def _lower_tail(internal):
    return _simple(internal).lower()


ENTITY_SUPER_SUFFIXES = (
    "entity", "mob", "animal", "monster", "fish", "wateranimal", "ghast",
    "golem", "dragon", "serpent", "slime", "villager", "horse", "tameable",
    "ageablemob", "pathfindermob", "flyingmob", "ambientcreature",
)
BLOCK_SUPER_SUFFIXES = ("block", "baseentityblock", "abstractblock")
ITEM_SUPER_SUFFIXES = ("item",)
BLOCKENTITY_SUFFIXES = ("blockentity", "tileentity", "machineentity")
SCREEN_SUFFIXES = ("screen", "abstractcontainerscreen", "gui", "guiwidget")
ENTRYPOINT_IFACES = (
    "modinitializer", "clientmodinitializer", "dedicatedservermodinitializer",
    "serverinitializer", "modloadingtoshare",
)
NETWORK_SUFFIXES = ("packet", "payload", "custompacketpayload", "message",
                    "packethandler", "networkmessage")
LISTENER_SUFFIXES = ("listener", "callback", "handler", "event", "subscriber")


def classify_class(info):
    """Map a parsed .class onto the role it plays for porting purposes.

    Ordered by specificity: a Mixin can extend anything, so it wins; a
    BlockEntity check precedes Block because `BaseEntityBlock` names contain
    both. Returns one kind string.
    """
    name = info.get("name") or ""
    simple = _lower_tail(name)
    super_name = info.get("super") or ""
    super_tail = _lower_tail(super_name)
    super_path = super_name.lower()
    iface_tails = {_lower_tail(iface) for iface in info.get("interfaces", [])}
    cp_text = "\x00".join(info.get("constant_strings", [])).lower()

    if "mixin" in simple or "org/spongepowered/mixin" in cp_text:
        return "mixin"
    if super_tail.endswith(BLOCKENTITY_SUFFIXES) or (
        "menuprovider" in iface_tails or "worldlycontainer" in iface_tails
    ):
        return "block_entity"
    if super_tail in ("livingentity", "mob", "entity", "animal") or (
        any(super_tail.endswith(suffix) for suffix in ENTITY_SUPER_SUFFIXES)
        and "entity" in super_path
    ) or any(tail.endswith(("entity", "mob", "animal")) for tail in iface_tails):
        return "entity"
    if super_tail.endswith(BLOCK_SUPER_SUFFIXES):
        return "block"
    if super_tail.endswith(SCREEN_SUFFIXES) or "screen" in simple:
        return "screen"
    if super_tail.endswith(ITEM_SUPER_SUFFIXES) or (
        simple.endswith("item") and "item" in super_path
    ):
        return "item"
    if iface_tails & set(ENTRYPOINT_IFACES) or simple.endswith("entrypoint"):
        return "entrypoint"
    if simple.endswith(NETWORK_SUFFIXES) or iface_tails & set(NETWORK_SUFFIXES):
        return "network"
    if (
        any(
            tail.endswith(suffix)
            for tail in iface_tails
            for suffix in LISTENER_SUFFIXES
        )
        or simple.endswith("listener")
    ):
        return "event_listener"
    if "config" in simple:
        return "config"
    return "other"


def _snake(simple):
    """`RubyBoarEntity` -> `ruby_boar_entity` (then callers strip suffixes)."""
    text = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", simple)
    text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", text)
    text = re.sub(r"[^A-Za-z0-9]+", "_", text).strip("_").lower()
    return text or "unnamed"


def _registry_path(simple, strip_suffixes):
    """Class simple name -> bundle path segment (`RubyBoar` -> `ruby_boar`)."""
    snake = _snake(simple.split("$", 1)[0])
    for suffix in strip_suffixes:
        if snake.endswith(suffix) and len(snake) > len(suffix):
            return snake[: -len(suffix)]
    return snake


def sanitize_mod_id(text, fallback="ported_mod"):
    """Coerce a loader id / file stem into megu's `^[a-z][a-z0-9_.-]{0,63}$`."""
    cleaned = re.sub(r"[^a-z0-9_.-]+", "_", (text or "").strip().lower())
    cleaned = cleaned.strip("_.-")
    if not cleaned or not cleaned[0].isalpha():
        cleaned = "mod_" + cleaned if cleaned else fallback
    if not cleaned[0].isalpha():
        cleaned = "mod_" + cleaned.lstrip("0123456789")
    return cleaned[:64]


def sanitize_semver(text):
    """`1.2` -> `1.2.0`, `${version}` -> `0.1.0`; always valid SemVer."""
    text = (text or "").strip()
    match = re.search(r"\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?", text)
    if match:
        return match.group(0)
    match = re.search(r"(\d+)\.(\d+)", text)
    if match:
        return f"{match.group(1)}.{match.group(2)}.0"
    match = re.search(r"(\d+)", text)
    if match:
        return f"{match.group(1)}.0.0"
    return "0.1.0"


def _first_mc_version(range_text):
    """First `x.y(.z)` in a version range -> PACK_FORMATS lookup key."""
    match = re.search(r"\d+\.\d+(?:\.\d+)?", range_text or "")
    return match.group(0) if match else "1.21.1"


def _safe_range(text, fallback="[1.21.1,)"):
    if isinstance(text, str) and text.strip():
        if dev.version_range_error(text.strip()) is None:
            return text.strip()
    return fallback


def _json_member(members, name):
    """Parse a JSON member when present and small enough; None otherwise."""
    payload = members.get(name)
    if payload is None or len(payload) > MAX_MANIFEST_BYTES:
        return None
    try:
        return json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None


def _toml_member(members, name):
    payload = members.get(name)
    if payload is None or len(payload) > MAX_MANIFEST_BYTES:
        return None
    try:
        return dev.parse_toml(payload.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return None


def _fabric_metadata(doc):
    mod = doc if isinstance(doc, dict) else {}
    deps = mod.get("depends") if isinstance(mod.get("depends"), dict) else {}
    authors = mod.get("authors") or mod.get("contributors") or []
    if isinstance(authors, str):
        authors = [authors]
    authors = [
        a.get("name", "") if isinstance(a, dict) else str(a) for a in authors
    ]
    return {
        "loader": "fabric",
        "id": mod.get("id"),
        "name": mod.get("name") or mod.get("id"),
        "version": mod.get("version"),
        "description": mod.get("description"),
        "license": mod.get("license") if isinstance(mod.get("license"), str) else None,
        "authors": [a for a in authors if a.strip()],
        "mc_version_range": deps.get("minecraft"),
    }


def _neoforge_metadata(doc, loader):
    mods = doc.get("mods") if isinstance(doc, dict) else None
    mod = mods[0] if isinstance(mods, list) and mods and isinstance(mods[0], dict) else {}
    mod_id = mod.get("modId")
    authors = mod.get("authors")
    if isinstance(authors, str):
        authors = [authors]
    mc_range = None
    deps = doc.get("dependencies")
    if isinstance(deps, dict) and mod_id:
        for dep in deps.get(mod_id) or []:
            if isinstance(dep, dict) and dep.get("modId") == "minecraft":
                mc_range = dep.get("versionRange")
    return {
        "loader": loader,
        "id": mod_id,
        "name": mod.get("displayName") or mod_id,
        "version": mod.get("version"),
        "description": mod.get("description"),
        "license": doc.get("license") if isinstance(doc.get("license"), str) else None,
        "authors": [a for a in authors if isinstance(a, str) and a.strip()] if authors else [],
        "mc_version_range": mc_range,
    }


# ---------------------------------------------------------------------------
# Inspection
# ---------------------------------------------------------------------------


class Inspection:
    """Aggregated result of scanning one mod source (plus its nested jars)."""

    def __init__(self, display):
        self.display = display
        self.kind = "jar"
        self.loaders = []
        self.metadata = {}
        self.data_files = {}
        self.asset_files = {}
        self.root_files = {}
        self.classes = []          # {name, super, interfaces, methods, kind, source}
        self.class_rejected = []
        self.embedded_jars = []
        self.rejected = []
        self.namespaces = set()
        self.mrpack_files = []     # declared downloads (not bundled)
        self.mrpack_notes = []
        self.override_files = 0

    def merge_metadata(self, candidate):
        """First manifest wins on scalar fields; loaders accumulate."""
        loader = candidate.get("loader")
        if loader and loader not in self.loaders:
            self.loaders.append(loader)
        for key, value in candidate.items():
            if key == "loader":
                continue
            if value and not self.metadata.get(key):
                self.metadata[key] = value

    def add_member(self, name, payload, source):
        """Route one archive/dir member into data/, assets/, root, or classes."""
        lower = name.lower()
        parts = lower.split("/")
        if name.endswith(".class"):
            if len(self.classes) >= MAX_CLASSES:
                self.class_rejected.append((name, "class cap reached"))
                return
            try:
                info = parse_class(payload)
            except ClassFormatError as error:
                self.class_rejected.append((name, str(error)))
                return
            info["kind"] = classify_class(info)
            info["source"] = source
            info["member"] = name
            self.classes.append(info)
            return
        if len(parts) >= 3 and parts[0] in PACKED_DATA_ROOTS and parts[1]:
            namespace = parts[1]
            if dev.NAMESPACE.match(namespace):
                self.namespaces.add(namespace)
            suffix = Path(name).suffix.lower()
            if suffix in dev.UNSAFE_EXTENSIONS:
                self.rejected.append((name, f"unsafe payload extension '{suffix}'"))
                return
            if parts[0] == "data":
                self.data_files[name] = payload
            else:
                self.asset_files[name] = payload
            return
        if lower in ROOT_COPY_FILES:
            self.root_files[lower] = payload
            return
        if any(lower.startswith(glob.rstrip("*").lower()) for glob in ROOT_COPY_GLOBS) and "/" not in name:
            self.root_files[name] = payload

    # -- serialization -----------------------------------------------------

    def class_counts(self):
        counts = {}
        for info in self.classes:
            counts[info["kind"]] = counts.get(info["kind"], 0) + 1
        return counts

    def data_categories(self):
        counts = {}
        for name in self.data_files:
            parts = name.split("/")
            category = parts[2] if len(parts) > 2 else "(root)"
            counts[category] = counts.get(category, 0) + 1
        return counts

    def asset_categories(self):
        counts = {}
        for name in self.asset_files:
            parts = name.split("/")
            category = parts[2] if len(parts) > 2 else "(root)"
            counts[category] = counts.get(category, 0) + 1
        return counts

    def to_dict(self):
        return {
            "schema": REPORT_SCHEMA,
            "input": self.display,
            "kind": self.kind,
            "loaders": self.loaders or ["unknown"],
            "mod": self.metadata,
            "namespaces": sorted(self.namespaces),
            "data_files": sum(self.data_categories().values()),
            "asset_files": sum(self.asset_categories().values()),
            "data_categories": self.data_categories(),
            "asset_categories": self.asset_categories(),
            "class_kinds": self.class_counts(),
            "classes_total": len(self.classes),
            "embedded_jars": self.embedded_jars,
            "rejected_members": [
                {"member": name, "reason": reason}
                for name, reason in self.rejected + self.class_rejected
            ],
            "mrpack_files": self.mrpack_files,
            "mrpack_notes": self.mrpack_notes,
        }


def _is_embedded_jar(name):
    lower = name.lower()
    return lower.endswith(".jar") and any(
        lower.startswith(prefix) for prefix in EMBEDDED_JAR_DIRS
    )


def _embedded_budget():
    return {"count": 0, "bytes": 0}


def inspect_members(members, display, inspection, depth=0, budget=None, source=None):
    """Fold one member map into `inspection` (recursing into embedded jars)."""
    budget = budget if budget is not None else _embedded_budget()
    source = source or display
    for name in sorted(members):
        lower = name.lower()
        if depth == 0 and (
            lower == "fabric.mod.json"
            or lower in ("meta-inf/mods.toml", "meta-inf/neoforge.mods.toml")
            or lower == "mcmod.info"
            or lower == "quilt.mod.json"
        ):
            continue  # consumed by the metadata pass below
        if _is_embedded_jar(name):
            if depth + 1 > MAX_NESTED_DEPTH:
                inspection.rejected.append((name, f"nested jar deeper than {MAX_NESTED_DEPTH}"))
                continue
            if budget["count"] >= MAX_NESTED_JARS:
                inspection.rejected.append((name, f"more than {MAX_NESTED_JARS} nested jars"))
                continue
            if budget["bytes"] + len(members[name]) > MAX_NESTED_BYTES:
                inspection.rejected.append((name, f"nested jars exceed {MAX_NESTED_BYTES} bytes"))
                continue
            budget["count"] += 1
            budget["bytes"] += len(members[name])
            try:
                nested = read_archive(members[name], name)
            except PortError as error:
                inspection.rejected.append((name, str(error)))
                continue
            inspection.embedded_jars.append(name)
            inspect_members(nested, name, inspection, depth + 1, budget, name)
            continue
        inspection.add_member(name, members[name], source)
    if depth == 0:
        _ingest_manifests(members, inspection)


def _ingest_manifests(members, inspection):
    fabric = _json_member(members, "fabric.mod.json")
    if fabric is not None:
        inspection.merge_metadata(_fabric_metadata(fabric))
    neoforge = _toml_member(members, "META-INF/neoforge.mods.toml")
    if neoforge is not None:
        inspection.merge_metadata(_neoforge_metadata(neoforge, "neoforge"))
    forge = _toml_member(members, "META-INF/mods.toml")
    if forge is not None:
        inspection.merge_metadata(_neoforge_metadata(forge, "forge"))
    quilt = _json_member(members, "quilt.mod.json")
    if isinstance(quilt, dict) and isinstance(quilt.get("quilt_loader"), dict):
        loader = quilt["quilt_loader"]
        inspection.merge_metadata({
            "loader": "quilt",
            "id": loader.get("id"),
            "name": (loader.get("metadata") or {}).get("name") or loader.get("id"),
            "version": loader.get("version"),
            "description": (loader.get("metadata") or {}).get("description"),
            "license": None,
            "authors": [],
            "mc_version_range": None,
        })
    mcmod = _json_member(members, "mcmod.info")
    entry = mcmod[0] if isinstance(mcmod, list) and mcmod else mcmod
    if isinstance(entry, dict):
        inspection.merge_metadata({
            "loader": "forge",
            "id": entry.get("modid"),
            "name": entry.get("name") or entry.get("modid"),
            "version": entry.get("version"),
            "description": entry.get("description"),
            "license": None,
            "authors": entry.get("authorList") or [],
            "mc_version_range": None,
        })


def inspect_archive_path(path):
    """Open a .jar/.mrpack/.zip file and return a populated Inspection."""
    path = Path(path)
    if not path.is_file():
        raise PortError(f"{path}: no such file")
    try:
        size = path.stat().st_size
    except OSError as error:
        raise PortError(f"{path}: cannot stat: {error}")
    if size > MAX_SOURCE_BYTES:
        raise PortError(f"{path}: {size} bytes exceeds {MAX_SOURCE_BYTES}")
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise PortError(f"{path}: cannot read: {error}")
    members = read_archive(payload, str(path))
    inspection = Inspection(str(path))
    names = {name.lower() for name in members}
    if MODPACK_INDEX in names or (
        path.suffix.lower() in (".mrpack",) or (
            path.suffix.lower() == ".zip" and CURSEFORGE_INDEX in names
        )
    ):
        if MODPACK_INDEX in names:
            inspection.kind = "mrpack"
            _inspect_mrpack(members, inspection)
        else:
            inspection.kind = "curseforge-pack"
            _inspect_curseforge_pack(members, inspection)
        return inspection
    inspection.kind = "jar"
    inspect_members(members, str(path), inspection)
    return inspection


def _override_members(members):
    """`overrides/...` entries keyed by their path relative to the override root."""
    out = {}
    for name, payload in members.items():
        lower = name.lower()
        for prefix in OVERRIDE_DIRS:
            if lower.startswith(prefix):
                out[name[len(prefix):]] = payload
                break
    return out


def _inspect_mrpack(members, inspection):
    index = _json_member(members, MODPACK_INDEX)
    if not isinstance(index, dict):
        raise PortError(f"{inspection.display}: modrinth.index.json missing or invalid")
    if index.get("formatVersion") != 1:
        raise PortError(
            f"{inspection.display}: unsupported modrinth.index.json formatVersion "
            f"{index.get('formatVersion')!r}"
        )
    deps = index.get("dependencies") if isinstance(index.get("dependencies"), dict) else {}
    loader = "unknown"
    for key in ("neoforge", "forge", "fabric-loader", "quilt-loader"):
        if key in deps:
            loader = {"fabric-loader": "fabric", "quilt-loader": "quilt"}.get(key, key)
            break
    inspection.merge_metadata({
        "loader": loader,
        "id": index.get("name"),
        "name": index.get("name"),
        "version": index.get("versionId"),
        "description": index.get("summary"),
        "license": None,
        "authors": [],
        "mc_version_range": f"[{deps['minecraft']},)" if deps.get("minecraft") else None,
    })
    files = index.get("files") if isinstance(index.get("files"), list) else []
    for entry in files:
        if not isinstance(entry, dict):
            continue
        rel = entry.get("path")
        error = member_path_error(rel) if isinstance(rel, str) else "path is not a string"
        record = {
            "path": rel,
            "env": entry.get("env"),
            "sha512": (entry.get("hashes") or {}).get("sha512")
            if isinstance(entry.get("hashes"), dict) else None,
        }
        if error:
            record["rejected"] = error
        inspection.mrpack_files.append(record)
    inspection.mrpack_notes.append(
        f"{len(files)} declared download(s); fetch them from Modrinth and run "
        "`port` on each .jar individually — only overrides/ content is bundled here."
    )
    overrides = _override_members(members)
    embedded = {n: p for n, p in overrides.items() if n.lower().endswith(".jar")}
    for name in sorted(embedded):
        payload = overrides[name]
        try:
            nested = read_archive(payload, f"overrides/{name}")
        except PortError as error:
            inspection.rejected.append((f"overrides/{name}", str(error)))
            continue
        inspection.embedded_jars.append(f"overrides/{name}")
        inspect_members(nested, f"overrides/{name}", inspection, 1, source=f"overrides/{name}")
    for name, payload in overrides.items():
        if name.lower().endswith(".jar"):
            continue
        if name.lower().startswith(("data/", "assets/")):
            inspection.add_member(name, payload, "overrides")
        else:
            inspection.override_files += 1
    inspection.mrpack_notes.append(
        f"{inspection.override_files} override file(s) outside data//assets "
        "(configs, options) left for manual review."
    )


def _inspect_curseforge_pack(members, inspection):
    index = _json_member(members, CURSEFORGE_INDEX)
    if not isinstance(index, dict):
        raise PortError(f"{inspection.display}: manifest.json missing or invalid")
    minecraft = index.get("minecraft") if isinstance(index.get("minecraft"), dict) else {}
    loaders = minecraft.get("modLoaders") or []
    loader = "unknown"
    for entry in loaders:
        if isinstance(entry, dict) and entry.get("primary") and entry.get("id"):
            loader = str(entry["id"]).split("-", 1)[0]
    inspection.merge_metadata({
        "loader": loader,
        "id": index.get("name"),
        "name": index.get("name"),
        "version": index.get("version"),
        "description": None,
        "license": None,
        "authors": [index.get("author")] if index.get("author") else [],
        "mc_version_range": f"[{minecraft.get('version')},)" if minecraft.get("version") else None,
    })
    files = index.get("files") if isinstance(index.get("files"), list) else []
    for entry in files:
        inspection.mrpack_files.append({"path": entry, "curseforge": True})
    inspection.mrpack_notes.append(
        f"{len(files)} declared CurseForge file id(s); resolve and port each .jar "
        "individually — only overrides/ content is bundled here."
    )
    overrides = _override_members(members)
    for name, payload in overrides.items():
        if name.lower().endswith(".jar"):
            try:
                nested = read_archive(payload, f"overrides/{name}")
            except PortError as error:
                inspection.rejected.append((f"overrides/{name}", str(error)))
                continue
            inspection.embedded_jars.append(f"overrides/{name}")
            inspect_members(nested, f"overrides/{name}", inspection, 1, source=f"overrides/{name}")
        elif name.lower().startswith(("data/", "assets/")):
            inspection.add_member(name, payload, "overrides")
        else:
            inspection.override_files += 1


_GRADLE_RESOURCES = "src/main/resources/"


def inspect_input(path):
    """Dispatch on input kind: directory, .mrpack/.zip pack, or .jar."""
    path = Path(path)
    if path.is_dir():
        inspection = Inspection(str(path))
        inspection.kind = "dir"
        members = read_directory(path)
        if not any(name.startswith(("data/", "assets/")) for name in members):
            # Gradle layout: real mod resources live under src/main/resources/.
            stripped = {
                name[len(_GRADLE_RESOURCES):]: payload
                for name, payload in members.items()
                if name.startswith(_GRADLE_RESOURCES)
            }
            if stripped:
                members = stripped
        inspect_members(members, str(path), inspection)
        return inspection
    if not path.exists():
        raise PortError(f"{path}: no such file or directory")
    return inspect_archive_path(path)


# ---------------------------------------------------------------------------
# Project synthesis
# ---------------------------------------------------------------------------


def synthesized_creatures(inspection, mod_id):
    """Detected entity classes -> declarative `creatures[]` bundle entries."""
    creatures = {}
    for info in inspection.classes:
        if info["kind"] != "entity":
            continue
        path = _registry_path(_simple(info["name"]), ("_entity", "_mob"))
        creatures.setdefault(path, {
            "entity_type": f"{mod_id}:{path}",
            "max_health": 20.0,
            "movement": {"kind": "land", "ground_speed": 0.2},
        })
        if len(creatures) >= dev.MAX_SECTION_ENTRIES:
            break
    return list(creatures.values())


def synthesized_channels(inspection, mod_id):
    """Detected network payload classes -> `custom_channels[]` ids."""
    channels = []
    seen = set()
    for info in inspection.classes:
        if info["kind"] != "network":
            continue
        path = _registry_path(
            _simple(info["name"]),
            ("_packet", "_payload", "_message", "_handler", "_networkmessage"),
        )
        candidate = f"{mod_id}:{path}"
        if candidate in seen or dev.resource_location_error(candidate):
            continue
        seen.add(candidate)
        channels.append(candidate)
        if len(channels) >= dev.MAX_SECTION_ENTRIES:
            break
    return channels


def build_project_files(inspection):
    """Manifest + bundle + pack.mcmeta for the ported project."""
    meta = inspection.metadata
    mod_id = sanitize_mod_id(meta.get("id") or Path(inspection.display).stem)
    version = sanitize_semver(meta.get("version"))
    mc_range = _safe_range(meta.get("mc_version_range"))
    name = (meta.get("name") or mod_id).strip() or mod_id
    authors = meta.get("authors") or ["ported with megu_port_mod.py"]
    license_name = (meta.get("license") or "All Rights Reserved").strip() or "All Rights Reserved"
    description = (
        (meta.get("description") or "").strip()
        or f"Ported from {Path(inspection.display).name} ({', '.join(inspection.loaders) or 'unknown loader'})."
    )
    manifest = {
        "schema": dev.PACKAGE_SCHEMA,
        "mod": {
            "id": mod_id,
            "name": name,
            "version": version,
            "mc_version_range": mc_range,
            "authors": authors,
            "license": license_name,
            "description": description,
        },
        "bundle": {"file": dev.BUNDLE_FILE},
    }
    creatures = synthesized_creatures(inspection, mod_id)
    channels = synthesized_channels(inspection, mod_id)
    bundle = dev.bundle_document(mod_id, _first_mc_version(mc_range), creatures, channels)
    # bundle_document forces `[<mc>,)`; honour the manifest's richer range.
    bundle["mc_version_range"] = mc_range
    pack = inspection.root_files.get("pack.mcmeta")
    if pack is None:
        pack = (json.dumps(dev.pack_mcmeta(_first_mc_version(mc_range), name), indent=2) + "\n").encode("utf-8")
    return manifest, bundle, pack, {
        "mod_id": mod_id,
        "version": version,
        "name": name,
        "mc_version_range": mc_range,
    }


def render_project_readme(inspection, ids):
    loaders = ", ".join(inspection.loaders) or "unknown loader"
    return f"""# {ids['name']} (ported)

Ported from `{Path(inspection.display).name}` ({loaders}) by
`tools/megu_port_mod.py`. See `PORT_REPORT.md` for what was lifted
automatically and what still needs a Rust port.

```sh
python3 tools/megu_mod_dev.py validate . --json
python3 tools/megu_mod_dev.py pack . --deterministic
python3 tools/megu_mod_dev.py pack . --deterministic --compat-jar   # Modrinth-ready .jar
```
"""


def render_port_report(inspection, ids, scaffold):
    """PORT_REPORT.md: auto-ported coverage vs. classes needing a Rust port."""
    counts = inspection.class_counts()
    total = len(inspection.classes)
    bundle_entities = len(synthesized_creatures(inspection, ids["mod_id"]))
    bundle_channels = len(synthesized_channels(inspection, ids["mod_id"]))
    lifted = bundle_entities + bundle_channels
    data_count = len(inspection.data_files)
    asset_count = len(inspection.asset_files)
    auto = data_count + asset_count + lifted
    grand = auto + total
    percent = (auto * 100.0 / grand) if grand else 100.0
    lines = [
        f"# Port report — {ids['name']} `{ids['mod_id']}`",
        "",
        f"- input: `{inspection.display}` ({inspection.kind}, "
        f"loaders: {', '.join(inspection.loaders) or 'unknown'})",
        f"- data files ported: {data_count} across {len(inspection.data_categories())} categories",
        f"- asset files ported: {asset_count} across {len(inspection.asset_categories())} categories",
        f"- classes scanned: {total} "
        f"({bundle_entities} entities + {bundle_channels} channels lifted into the bundle)",
        f"- auto-ported share: {percent:.1f}% of ported resources "
        f"({auto} of {grand} resources+classes)",
        f"- embedded jars folded: {len(inspection.embedded_jars)}",
        f"- rejected members: {len(inspection.rejected) + len(inspection.class_rejected)}",
        "",
        "## Classes by detected role",
        "",
        "| kind | count | needs Rust port |",
        "| --- | --- | --- |",
    ]
    needs = {"entity": "partial (bundle creature spec)",
             "network": "partial (bundle custom_channels id)"}
    for kind in sorted(counts):
        lines.append(f"| {kind} | {counts[kind]} | {needs.get(kind, 'yes')} |")
    lines += ["", "## Per-class port map", ""]
    for info in inspection.classes[:2048]:
        hint = SCAFFOLD_HINTS.get(info["kind"], SCAFFOLD_HINTS["other"])
        lines.append(
            f"- `{info['name']}` ({info['kind']}, from `{info['source']}`) — {hint}"
        )
    if len(inspection.classes) > 2048:
        lines.append(f"- ... and {len(inspection.classes) - 2048} more")
    if inspection.rejected or inspection.class_rejected:
        lines += ["", "## Rejected members", ""]
        for name, reason in inspection.rejected + inspection.class_rejected:
            lines.append(f"- `{name}` — {reason}")
    if inspection.mrpack_files:
        lines += ["", "## Declared pack downloads (port each .jar)", ""]
        for record in inspection.mrpack_files[:256]:
            lines.append(f"- `{record.get('path')}`")
    if scaffold:
        lines += [
            "",
            "## Rust scaffold",
            "",
            "`rust-port/` contains a `megu-mod-sdk` crate skeleton: "
            "`build_project()` registers every detected block/item/entity and "
            "each remaining class is a `TODO(port)` stub mapped to the SDK "
            "surface that replaces it. Adjust the `megu-mod-sdk` path in "
            "`rust-port/Cargo.toml` to your checkout.",
        ]
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Rust scaffold
# ---------------------------------------------------------------------------


SCAFFOLD_HINTS = {
    "block": "register with `BlockBuilder::new(\"<id>\")`",
    "item": "register with `ItemBuilder::new(\"<id>\")`",
    "entity": "declared in the bundle `creatures` section; extend with `CreatureBuilder`",
    "block_entity": "use `mod-adapter-core` `BlockCapabilityBus` (item/fluid slots)",
    "mixin": "rewrite as a native event hook; `@Redirect`/`@Overwrite` have no lift",
    "screen": "client-only UI — port to `client-port` UI or drop on dedicated servers",
    "network": "channel id declared in `custom_channels`; port the payload codec",
    "event_listener": "map onto megu server lifecycle / world event hooks",
    "entrypoint": "replaced by `build_project()` below",
    "config": "express as datapack/config JSON where possible",
    "other": "review manually — no mechanical mapping",
}

_BUILDERS = {
    "block": ("BlockBuilder", "block"),
    "item": ("ItemBuilder", "item"),
    "entity": ("CreatureBuilder", "creature"),
}


def render_lib_rs(inspection, ids):
    """`rust-port/src/lib.rs`: typed stubs for every detected custom class."""
    used = sorted({_BUILDERS[k][0] for k in _BUILDERS
                   if any(c["kind"] == k for c in inspection.classes)}
                  | {"ModProjectBuilder"})
    lines = [
        "//! Auto-generated port scaffold by `tools/megu_port_mod.py`.",
        "//!",
        "//! Each `TODO(port)` maps one Java class onto the `megu-mod-sdk`",
        "//! surface that replaces it. Delete stubs as you port them.",
        "",
        f"use megu_mod_sdk::builders::{{{', '.join(used)}}};",
        "use megu_mod_sdk::{ModProject, SdkDiagnostic};",
        "",
        "/// Build the ported project. Data-driven content lives in the sibling",
        "/// `data/` + `assets/` trees and the generated `megu-mod-bundle.json`.",
        "pub fn build_project() -> Result<ModProject, Vec<SdkDiagnostic>> {",
        f"    ModProjectBuilder::new(\"{ids['mod_id']}\", \"{ids['version']}\")",
        f"        .display_name(\"{_escape_rust(ids['name'])}\")",
        f"        .mc_version_range(\"{_escape_rust(ids['mc_version_range'])}\")",
    ]
    body = []
    for info in inspection.classes:
        simple = _simple(info["name"])
        java = info["name"].replace("/", ".")
        if info["kind"] in _BUILDERS:
            builder, method = _BUILDERS[info["kind"]]
            strip = ("_entity", "_mob") if info["kind"] == "entity" else ()
            reg_id = _registry_path(simple, strip)
            body.append(
                f"        .{method}({builder}::new(\"{reg_id}\")) // <- java `{java}`"
            )
        else:
            body.append(
                f"        // TODO(port): `{java}` ({info['kind']}) — "
                f"{SCAFFOLD_HINTS.get(info['kind'], SCAFFOLD_HINTS['other'])}"
            )
    if not body:
        body.append("        // no .class files detected — pure data/asset mod")
    lines += body
    lines += ["        .build()", "}", ""]
    return "\n".join(lines)


def _escape_rust(text):
    return str(text).replace("\\", "\\\\").replace('"', '\\"')


def render_cargo_toml(ids):
    return f"""# Auto-generated by tools/megu_port_mod.py.
[package]
name = "{ids['mod_id']}"
version = "{ids['version']}"
edition = "2021"
license = "LGPL-3.0-or-later"

[dependencies]
# Adjust this path to the sdk/megu-mod-sdk crate in your Megu checkout.
megu-mod-sdk = {{ path = "../../../sdk/megu-mod-sdk" }}
"""


def write_rust_scaffold(out_dir, inspection, ids):
    scaffold = Path(out_dir) / "rust-port"
    (scaffold / "src").mkdir(parents=True, exist_ok=True)
    (scaffold / "Cargo.toml").write_text(render_cargo_toml(ids), encoding="utf-8")
    (scaffold / "src" / "lib.rs").write_text(render_lib_rs(inspection, ids), encoding="utf-8")
    return scaffold


# ---------------------------------------------------------------------------
# port / analyze
# ---------------------------------------------------------------------------


def write_project(inspection, out_dir, want_scaffold):
    """Write the megu project tree; returns (manifest, bundle, ids, scaffold)."""
    out_dir = Path(out_dir)
    manifest, bundle, pack, ids = build_project_files(inspection)
    out_dir.mkdir(parents=True, exist_ok=True)
    for name, payload in inspection.data_files.items():
        target = out_dir / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(payload)
    for name, payload in inspection.asset_files.items():
        target = out_dir / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(payload)
    for name, payload in inspection.root_files.items():
        if name == "pack.mcmeta":
            continue
        (out_dir / name).write_bytes(payload)
    (out_dir / dev.MANIFEST_FILE).write_text(
        dev.render_manifest_toml(manifest), encoding="utf-8"
    )
    (out_dir / dev.BUNDLE_FILE).write_text(
        json.dumps(bundle, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    (out_dir / "pack.mcmeta").write_bytes(pack)
    (out_dir / "README.md").write_text(render_project_readme(inspection, ids), encoding="utf-8")
    scaffold = write_rust_scaffold(out_dir, inspection, ids) if want_scaffold else None
    (out_dir / "PORT_REPORT.md").write_text(
        render_port_report(inspection, ids, bool(scaffold)), encoding="utf-8"
    )
    return manifest, bundle, ids, scaffold


def _package_summary(path, entries):
    payload = Path(path).read_bytes()
    return {
        "path": str(path),
        "bytes": len(payload),
        "entries": len(entries),
        "sha256": hashlib.sha256(payload).hexdigest(),
        "sha512": hashlib.sha512(payload).hexdigest(),
    }


def cmd_port(args):
    try:
        inspection = inspect_input(args.input)
    except PortError as error:
        return _emit_error(args, str(error))
    out_dir = Path(args.out)
    if out_dir.exists() and any(out_dir.iterdir()) and not args.force:
        return _emit_error(
            args, f"{out_dir} is not empty; pass --force to port into it anyway"
        )
    manifest, bundle, ids, scaffold = write_project(
        inspection, out_dir, args.rust_scaffold
    )
    report = dev.validate_project(out_dir)
    packages = []
    if report.ok:
        formats = ("mmod", "compat-jar") if args.format == "both" else (args.format,)
        for kind in formats:
            compat = kind == "compat-jar"
            suffix = ".jar" if compat else ".mmod"
            target = out_dir / "dist" / f"{ids['mod_id']}-{ids['version']}{suffix}"
            try:
                entries, _size, _sha256, _sha512 = dev.build_archive(
                    out_dir, target, compat_jar=compat, deterministic=True
                )
            except (OSError, ValueError) as error:
                report.error("pack.failed", str(target), str(error), "check the output path")
                continue
            packages.append(_package_summary(target, entries))
    result = {
        "schema": REPORT_SCHEMA,
        "ok": report.ok and len(packages) == (2 if args.format == "both" else 1),
        "input": inspection.to_dict(),
        "project": {
            "dir": str(out_dir),
            "mod_id": ids["mod_id"],
            "version": ids["version"],
            "validates": report.ok,
        },
        "packages": packages,
        "rust_scaffold": str(scaffold) if scaffold else None,
        "diagnostics": [d.to_dict() for d in report.diagnostics],
    }
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(_render_human_port(result, inspection))
    return 0 if result["ok"] else 1


def _emit_error(args, message):
    payload = {"schema": REPORT_SCHEMA, "ok": False, "error": message}
    if getattr(args, "json", False):
        print(json.dumps(payload, indent=2))
    else:
        print(f"error: {message}", file=sys.stderr)
    return 1


def _render_human_port(result, inspection):
    project = result["project"]
    lines = [
        f"ported {inspection.display} -> {project['dir']} "
        f"(mod_id {project['mod_id']}, loaders: {', '.join(inspection.loaders) or 'unknown'})",
        f"  data files: {len(inspection.data_files)} | "
        f"assets: {len(inspection.asset_files)} | "
        f"classes: {len(inspection.classes)} | "
        f"embedded jars: {len(inspection.embedded_jars)}",
    ]
    for diagnostic in result["diagnostics"]:
        lines.append(
            f"  {diagnostic['severity']}: [{diagnostic['code']}] "
            f"{diagnostic['path']}: {diagnostic['message']}"
        )
    if result["rust_scaffold"]:
        lines.append(f"  rust scaffold: {result['rust_scaffold']}")
    for package in result["packages"]:
        lines.append(f"  packed {package['path']} ({package['bytes']} bytes)")
        lines.append(f"    sha256 {package['sha256']}")
    lines.append(f"  validate: {'OK' if project['validates'] else 'FAILED — see diagnostics'}")
    lines.append("  see PORT_REPORT.md for the per-class port map")
    return "\n".join(lines)


def cmd_analyze(args):
    try:
        inspection = inspect_input(args.input)
    except PortError as error:
        return _emit_error(args, str(error))
    report = inspection.to_dict()
    report["ok"] = True
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(_render_human_analyze(inspection))
    return 0


def _render_human_analyze(inspection):
    lines = [
        f"{inspection.display} ({inspection.kind})",
        f"  loaders: {', '.join(inspection.loaders) or 'unknown'}",
        f"  mod: {inspection.metadata.get('name') or '(unnamed)'} "
        f"id={inspection.metadata.get('id') or '?'} "
        f"version={inspection.metadata.get('version') or '?'}",
        f"  namespaces: {', '.join(sorted(inspection.namespaces)) or '(none)'}",
    ]
    for label, categories in (("data", inspection.data_categories()),
                              ("assets", inspection.asset_categories())):
        if categories:
            detail = ", ".join(f"{k}:{v}" for k, v in sorted(categories.items()))
            lines.append(f"  {label}: {sum(categories.values())} files ({detail})")
    if inspection.classes:
        detail = ", ".join(f"{k}:{v}" for k, v in sorted(inspection.class_counts().items()))
        lines.append(f"  classes: {len(inspection.classes)} ({detail})")
    if inspection.embedded_jars:
        lines.append(f"  embedded jars: {len(inspection.embedded_jars)}")
        for name in inspection.embedded_jars[:16]:
            lines.append(f"    - {name}")
    if inspection.mrpack_files:
        lines.append(f"  declared downloads: {len(inspection.mrpack_files)} (port each .jar)")
    if inspection.rejected or inspection.class_rejected:
        lines.append("  rejected:")
        for name, reason in (inspection.rejected + inspection.class_rejected)[:16]:
            lines.append(f"    - {name}: {reason}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser():
    parser = argparse.ArgumentParser(
        prog="megu_port_mod.py",
        description="Port a Java mod or Modrinth pack into a native Megu project (stdlib only).",
        epilog="See docs/port-your-mod-to-rust.md for the full workflow.",
    )
    parser.add_argument("--version", action="version", version=f"megu_port_mod {TOOL_VERSION}")
    subparsers = parser.add_subparsers(dest="command", required=True)

    port = subparsers.add_parser("port", help="convert a .jar/.mrpack/mod dir into a Megu project")
    port.add_argument("input", help="input .jar, .mrpack, or extracted mod directory")
    port.add_argument("--out", required=True, help="output project directory")
    port.add_argument(
        "--format",
        choices=("mmod", "compat-jar", "both"),
        default="mmod",
        help="package format(s) to emit into <out>/dist (default mmod)",
    )
    port.add_argument(
        "--rust-scaffold",
        action="store_true",
        help="emit rust-port/ crate skeleton + PORT_REPORT.md port map",
    )
    port.add_argument(
        "--force", action="store_true", help="write into a non-empty output directory"
    )
    port.add_argument("--json", action="store_true", help="machine-readable report")
    port.set_defaults(func=cmd_port)

    analyze = subparsers.add_parser(
        "analyze", help="read-only classification report (writes nothing)"
    )
    analyze.add_argument("input", help="input .jar, .mrpack, or extracted mod directory")
    analyze.add_argument("--json", action="store_true", help="machine-readable report")
    analyze.set_defaults(func=cmd_analyze)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
