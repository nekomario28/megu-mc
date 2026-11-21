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
                            or any `data/*/dimension/*.json` declaring a custom
                            Java ChunkGenerator (CUSTOM_CHUNK_GENERATOR)
  BuiltInSuperseded       : optimization mod whose function mc-spine already
                            implements natively (BUILTIN_SUPERSEDED_PERF_MODS);
                            checked before every other step, skipped
                            automatically, never ShimCheck-scanned, never
                            staged, never counted toward Tier 3 fallback

Clean-room note: matching a mod id against BUILTIN_SUPERSEDED_PERF_MODS only
*skips loading* a user-supplied JAR — it copies, links, bundles, or derives
nothing from that mod. All built-in optimizations in mc-spine, client-port,
and client-app are 100% clean-room original Rust implementations.

Dimension-adding mods shipping `data/<ns>/dimension_type/*.json` plus
`data/<ns>/dimension/*.json` are summarised under `dimension_mods` in the
JSON report; pure data-driven ones (vanilla noise/flat/void generators, no
custom ChunkGenerator) stay Tier 1 and launch with zero JVM.

Every entry also passes a Minecraft version gate (after the superseded check,
before every other tier check — classify_mod_for_version in fallback.rs): a
declared neoforge.mods.toml/mods.toml `versionRange`, fabric.mod.json
`depends.minecraft` range, or pure-data pack.mcmeta `pack_format` that does
not cover --mc-version (default 1.21.1, mirroring McVersion::CURRENT; the
latest-vanilla target 26.3 works too: `--mc-version 26.3`) yields
Tier 3 with MC_VERSION_INCOMPATIBLE / MC_VERSION_RANGE_INVALID /
PACK_FORMAT_INCOMPATIBLE plus a plain-language reason, so a mod built for
another Minecraft release never silently executes broken native code. Both
the legacy scalar `pack_format`/`supported_formats` schema and the 26.x
`(major, minor)` `min_format`/`max_format` tuples are recognized in
pack.mcmeta. The declared range and verdict are recorded per assignment in
the JSON report.

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
from collections import namedtuple
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
    -1: "BuiltInSuperseded",
}
# Sentinel tier for mods skipped as built-in-superseded (not a JVM tier).
TIER_SUPERSEDED = -1

# Exact strings from mod-adapter-core/src/fallback.rs so CLI output matches
# the Rust startup report 1:1.
REASON_FLAGSHIP = "handled by flagship native Rust adapter"
REASON_UNIVERSAL = "handled by zero-JVM universal adapter / offload profile"
REASON_SHIM = "verified compatible with bounded S0-S4 JVM shim"
REASON_FALLBACK_DEFAULT = "unsupported Mixin/AT or Minecraft API surface"
REASON_SUPERSEDED = "already covered by mc-spine built-in optimizations"

# Exact mirror of BUILTIN_SUPERSEDED_PERF_MODS in
# mod-adapter-core/src/fallback.rs: mod ids whose optimization surface
# mc-spine already implements natively (both Fabric and NeoForge/Forge ids,
# compared case-insensitively). These jars are skipped before ShimCheck so
# their heavy Mixins can never force Tier 3 NeoForge fallback.
BUILTIN_SUPERSEDED_PERF_MODS = frozenset({
    # Rendering / mesh / LOD.
    "sodium", "embeddium", "rubidium", "xenon", "magnesium",
    "immediatelyfast", "entityculling", "moreculling", "distanthorizons",
    "voxy", "bobby", "farsight", "indium", "reeses-sodium-options",
    "sodium-extra", "embeddium_extra",
    # Server tick / lighting / memory / startup.
    "lithium", "radium", "canary", "starlight", "phosphor", "scalpel",
    "ferritecore", "modernfix", "noisium", "c2me", "krypton", "lazydfu",
    "memoryleakfix", "alternate-current", "clumps", "saturn", "smoothboot",
    "fastload", "dashloader", "servercore",
})
REASON_REFUSE_STRICT = "strict policy rejected unsupported Tier 3 mod(s)"
REASON_REFUSE_QUARANTINE = (
    "refusing to quarantine mod(s) whose blocks exist in the saved world; "
    "switch to delegate fallback or set force_drop_saved_blocks=true"
)

CODE_MISSING_SHIMCHECK = "MISSING_SHIMCHECK_REPORT"
CODE_UNSCANNABLE = "UNSCANNABLE_JAVA_MOD"
CODE_CUSTOM_CHUNK_GENERATOR = "CUSTOM_CHUNK_GENERATOR"
CODE_MC_VERSION_INCOMPATIBLE = "MC_VERSION_INCOMPATIBLE"
CODE_MC_VERSION_RANGE_INVALID = "MC_VERSION_RANGE_INVALID"
CODE_PACK_FORMAT_INCOMPATIBLE = "PACK_FORMAT_INCOMPATIBLE"

# `dimension/` `generator.type` values the native Track 2 dynamic-dimension
# path executes with zero JVM (mirrors `GeneratorKind` in
# mod-adapter-core/src/dimension_mod.rs). Anything else is a modded Java
# ChunkGenerator and routes the mod to Tier 3 fallback.
NATIVE_DIMENSION_GENERATORS = frozenset(
    ("minecraft:noise", "minecraft:flat", "minecraft:debug"))

IDENTIFIER = re.compile(rb"[a-z0-9_.\-]{1,64}:[a-z0-9_/.\-]{1,128}")
TABLE_HEADER = re.compile(r"^\s*\[\[?\s*([^\]]+)\s*\]?\]")
MODID_LINE = re.compile(r"""^\s*modId\s*=\s*['"]([^'"]+)['"]""")
VERSION_RANGE_LINE = re.compile(
    r"""^\s*versionRange\s*=\s*['"]([^'"]*)['"]""")


# ---------------------------------------------------------------------------
# Multi-version compatibility engine — exact port of
# mod-adapter-core/src/version_compat.rs. Pure stdlib, fail closed: every
# malformed version or range string raises VersionRangeError (or returns
# None/False) instead of crashing, so a corrupt mod manifest degrades to a
# clean "incompatible" diagnostic rather than a launcher exception.
#
# Supported range syntaxes:
#   NeoForge / Forge / Maven intervals : exact pins [1.21.1], closed/open
#       intervals [1.21.1,1.21.5) / (1.21,1.22], unbounded ends [1.21.1,) /
#       (,1.21.5), comma-separated unions (,1.20.5],[1.21.1,), bare versions.
#   Fabric / SemVer                    : comparators >=1.21.1 <=1.21.4 >1.21
#       <1.22 =1.21.1, tilde ~1.21.1 (>=1.21.1 <1.22.0), caret ^1.21.1
#       (>=1.21.1 <2.0.0), wildcards 1.21.x / 1.21.* / *, space-separated AND
#       predicates (>=1.21.1 <1.21.5), and || OR unions (1.21.1 || 1.21.4).
#       Partial versions cover their whole band (npm convention): bare/`=`
#       `1.21` means `1.21.x`, so `>1.21` is `>=1.22.0`.
# ---------------------------------------------------------------------------

# The Minecraft version this mc-spine runtime implements, mirroring
# McVersion::CURRENT; the default for --mc-version.
DEFAULT_MC_VERSION = (1, 21, 1)

_KEY_MAX = (1 << 64) - 1  # u64::MAX — unbounded interval end.
_ASCII_WS = " \t\n\r\x0c"  # Rust is_ascii_whitespace: ' ' \t \n \x0C \r

# pack_format / DataVersion tables covering the 1.20.4-1.21.5 line mc-spine
# ships profiles for, plus latest vanilla 26.3 (mirrors
# data_pack_format_for / data_version_for).
_DATA_PACK_FORMATS = {
    (1, 20, 4): 26,
    (1, 20, 6): 41,
    (1, 21, 0): 48,
    (1, 21, 1): 48,
    (1, 21, 3): 57,
    (1, 21, 4): 61,
    (1, 21, 5): 71,
    (26, 3, 0): 121,
}
_DATA_VERSIONS = {
    (1, 20, 4): 3700,
    (1, 20, 6): 3839,
    (1, 21, 0): 3953,
    (1, 21, 1): 3955,
    (1, 21, 3): 4082,
    (1, 21, 4): 4189,
    (1, 21, 5): 4325,
    (26, 3, 0): 5023,
}

# Kind values mirror VersionCompatVerdict in version_compat.rs.
VersionVerdict = namedtuple("VersionVerdict", ("kind", "declared", "reason"))


class VersionRangeError(Exception):
    """Fail-closed error from parsing a declared version range."""


def _vkey(version):
    """Total-order key packing (major, minor, patch) into one int."""
    return (version[0] << 32) | (version[1] << 16) | version[2]


def format_mc_version(version):
    """`major.minor.patch` display of a (major, minor, patch) triple."""
    return "%d.%d.%d" % version


def _parse_numeric_version(text):
    """1-3 all-digit components (mirror of parse_numeric_version)."""
    if not text:
        raise VersionRangeError("empty version")
    parts = text.split(".")
    if len(parts) > 3:
        raise VersionRangeError(
            f"version '{text}' has more than three components")
    numbers = [0, 0, 0]
    for i, part in enumerate(parts):
        if not part or not part.isascii() or not part.isdigit():
            raise VersionRangeError(
                f"version '{text}' has non-numeric component '{part}'")
        numbers[i] = int(part)
        if numbers[i] > 0xFFFF:
            raise VersionRangeError(
                f"version '{text}' component '{part}' out of range")
    return tuple(numbers)


def parse_mc_version(text):
    """Parse '1.21.1', '1.21' (=1.21.0), or '1' (=1.0.0); None on invalid."""
    try:
        return _parse_numeric_version(text.strip())
    except VersionRangeError:
        return None


def _parse_spec(text):
    """SemVer-ish version spec -> (lo triple, specified) — parse_spec port.

    `specified` counts the leading numeric components: `1.21.x` -> 2,
    `1.x`/`1` -> 1, `*` -> 0.
    """
    text = text.strip()
    if text[:1] in ("v", "V"):
        text = text[1:]
    text = text.split("+", 1)[0]
    if not text:
        raise VersionRangeError("empty version spec")
    if "-" in text:
        raise VersionRangeError(
            f"prerelease/hyphenated spec '{text}' is unsupported "
            "(use an explicit range)")
    parts = text.split(".")
    if len(parts) > 3:
        raise VersionRangeError(
            f"version spec '{text}' has more than three components")
    lo = [0, 0, 0]
    specified = len(parts)
    saw_wildcard = False
    for i, part in enumerate(parts):
        if not part:
            raise VersionRangeError(
                f"version spec '{text}' has an empty component")
        if part in ("x", "X", "*"):
            saw_wildcard = True
            if specified == len(parts):
                specified = i
            continue
        if saw_wildcard:
            raise VersionRangeError(
                f"version spec '{text}' has a numeric component after "
                "a wildcard")
        if not part.isascii() or not part.isdigit():
            raise VersionRangeError(
                f"version spec '{text}' has non-numeric component '{part}'")
        lo[i] = int(part)
        if lo[i] > 0xFFFF:
            raise VersionRangeError(
                f"version spec '{text}' component '{part}' out of range")
    return lo, specified


def _spec_lo_key(lo):
    return (lo[0] << 32) | (lo[1] << 16) | lo[2]


def _spec_band_hi(lo, specified):
    """Exclusive upper key of the spec band: 1.21.1->1.21.2, 1.21.x->1.22.0."""
    if specified == 0:
        return _KEY_MAX
    if specified == 1:
        return (lo[0] + 1) << 32
    if specified == 2:
        return (lo[0] << 32) | ((lo[1] + 1) << 16)
    return _spec_lo_key(lo) + 1


_PREDICATE_OPS = ("~", "^", ">=", "<=", ">", "<", "=")


def _predicate_interval(tok):
    """One SemVer predicate token -> inclusive [lo, hi] key interval."""
    if tok in ("*", "x", "X"):
        return 0, _KEY_MAX
    op, body = "", tok
    for candidate in _PREDICATE_OPS:
        if tok.startswith(candidate):
            op, body = candidate, tok[len(candidate):]
            break
    if not body:
        raise VersionRangeError(f"predicate '{tok}' is missing a version")
    spec_lo, specified = _parse_spec(body)
    if specified == 0:
        # `*`, `~*`, `^*`, `>=1.x` style: wildcard-only spec matches anything.
        return 0, _KEY_MAX
    lo = _spec_lo_key(spec_lo)
    hi_band = _spec_band_hi(spec_lo, specified)
    if op == ">=":
        return lo, _KEY_MAX
    if op == ">":
        return hi_band, _KEY_MAX
    if op == "<=":
        return (0, lo) if specified == 3 else (0, hi_band - 1)
    if op == "<":
        if lo == 0:
            return 1, 0  # `<0.0.0` matches nothing; empty interval marker.
        return 0, lo - 1
    if op == "=":
        return lo, hi_band - 1
    if op == "~":
        # Tilde keeps the minor band: `~1.21.1` is `>=1.21.1 <1.22.0`,
        # `~1` is `>=1.0.0 <2.0.0`.
        hi = ((spec_lo[0] << 32) | ((spec_lo[1] + 1) << 16)
              if specified >= 2 else (spec_lo[0] + 1) << 32)
        return lo, hi - 1
    if op == "^":
        # Caret allows everything below the leftmost non-zero component's
        # increment: `^1.21.1` <2.0.0, `^0.2.3` <0.3.0, `^0.0.3` <0.0.4,
        # `^0.0` <0.1.0, `^0` <1.0.0.
        if spec_lo[0] > 0:
            hi = (spec_lo[0] + 1) << 32
        elif specified >= 2 and spec_lo[1] > 0:
            hi = (spec_lo[1] + 1) << 16
        elif specified == 3:
            hi = spec_lo[2] + 1
        elif specified == 2:
            hi = 1 << 16
        else:
            hi = 1 << 32
        return lo, hi - 1
    # Bare spec: a full `1.21.1` pins exactly, a partial `1.21`/`1` covers
    # its whole band (`1.21.x`-style), matching SemVer range convention.
    return lo, hi_band - 1


def _parse_maven_member(text, i):
    """One bracketed Maven interval member starting at text[i] ('[' or '(').

    Returns (lo, hi, next_index) with lo/hi inclusive version keys and
    next_index just past the closer.
    """
    open_incl = text[i] == "["
    close = -1
    for j in range(i + 1, len(text)):
        if text[j] in "])":
            close = j
            break
    if close < 0:
        raise VersionRangeError(f"unterminated interval in '{text}'")
    close_incl = text[close] == "]"
    inner = text[i + 1:close]
    parts = inner.split(",")
    if len(parts) == 1:
        pin = parts[0].strip()
        if not pin:
            raise VersionRangeError(f"empty interval '{inner}' in '{text}'")
        if not open_incl:
            raise VersionRangeError(
                f"exact pin '{inner}' in '{text}' must use square brackets")
        key = _vkey(_parse_numeric_version(pin))
        return key, key, close + 1
    if len(parts) == 2:
        lo_s, hi_s = parts[0].strip(), parts[1].strip()
        if lo_s:
            key = _vkey(_parse_numeric_version(lo_s))
            lo = key if open_incl else key + 1
        else:
            lo = 0
        if hi_s:
            key = _vkey(_parse_numeric_version(hi_s))
            hi = key if close_incl else max(key - 1, 0)
        else:
            hi = _KEY_MAX
        return lo, hi, close + 1
    raise VersionRangeError(
        f"interval '{inner}' in '{text}' has more than two bounds")


def _parse_maven_range(text):
    """Maven/NeoForge range -> union of inclusive version-key intervals."""
    intervals = []
    i, n = 0, len(text)
    while True:
        while i < n and text[i] in _ASCII_WS:
            i += 1
        if i >= n:
            if not intervals:
                raise VersionRangeError("empty version range")
            raise VersionRangeError(f"trailing separator in '{text}'")
        if text[i] in "[(":
            lo, hi, i = _parse_maven_member(text, i)
            intervals.append((lo, hi))
        else:
            # Bare member inside the list: exact pin (`(,1.0],1.2` style).
            end = text.find(",", i)
            end = n if end < 0 else end
            member = text[i:end].strip()
            if not member:
                raise VersionRangeError(f"empty list member in '{text}'")
            key = _vkey(_parse_numeric_version(member))
            intervals.append((key, key))
            i = end
        while i < n and text[i] in _ASCII_WS:
            i += 1
        if i >= n:
            break
        if text[i] != ",":
            raise VersionRangeError(
                f"expected ',' between intervals in '{text}'")
        i += 1
    return intervals


def _parse_semver_range(text):
    """Fabric/SemVer range -> union of inclusive version-key intervals."""
    intervals = []
    for arm in text.split("||"):
        arm = arm.strip()
        if not arm:
            raise VersionRangeError(f"empty '||' arm in '{text}'")
        lo, hi = 0, _KEY_MAX
        tokens = arm.split()
        j = 0
        while j < len(tokens):
            tok = tokens[j]
            # Tolerate `>= 1.21.1`: a lone operator binds the next token.
            if tok in _PREDICATE_OPS and j + 1 < len(tokens):
                tok += tokens[j + 1]
                j += 1
            bound_lo, bound_hi = _predicate_interval(tok)
            lo = max(lo, bound_lo)
            hi = min(hi, bound_hi)
            j += 1
        intervals.append((lo, hi))
    return intervals


class VersionRange:
    """Parsed declared range: a union of inclusive version-key intervals."""

    def __init__(self, declared, intervals):
        self.declared = declared
        self.intervals = intervals

    def contains(self, version):
        key = _vkey(version)
        return any(lo <= key <= hi for lo, hi in self.intervals)

    def overlaps_minor_band(self, version):
        """Whether the range admits any version on version's major.minor line."""
        lo = _vkey((version[0], version[1], 0))
        hi = _vkey((version[0], version[1], 0xFFFF))
        return any(a <= hi and b >= lo for a, b in self.intervals)


def parse_version_range(declared):
    """Parse `declared`: Maven when it opens with '['/'(', else Fabric SemVer.

    Empty or malformed input raises VersionRangeError — fail closed, never a
    crash on a corrupt manifest.
    """
    trimmed = declared.strip()
    if not trimmed:
        raise VersionRangeError("empty version range")
    intervals = (_parse_maven_range(trimmed) if trimmed[0] in "[("
                 else _parse_semver_range(trimmed))
    return VersionRange(trimmed, intervals)


def check_mod_mc_version(target, declared_range):
    """Check one mod's declared range against the server's target version.

    Mirrors check_mod_mc_version in version_compat.rs: returns a
    VersionVerdict with kind "compatible" | "minor_mismatch" |
    "major_mismatch" | "invalid_range".
    """
    declared = declared_range.strip()
    try:
        vrange = parse_version_range(declared_range)
    except VersionRangeError as error:
        return VersionVerdict("invalid_range", declared, str(error))
    if vrange.contains(target):
        return VersionVerdict("compatible", None, None)
    if vrange.overlaps_minor_band(target):
        return VersionVerdict("minor_mismatch", declared, None)
    return VersionVerdict("major_mismatch", declared, None)


def data_pack_format_for(version):
    """pack.mcmeta `pack_format` for version, or None outside the table."""
    return _DATA_PACK_FORMATS.get(version)


def data_version_for(version):
    """Mojang `DataVersion` for version, or None outside the table."""
    return _DATA_VERSIONS.get(version)


def data_pack_format_tuple_for(version):
    """(major, minor) data pack format tuple for version, or None.

    Mirrors data_pack_format_tuple_for in version_compat.rs: the shape
    pack.mcmeta `min_format`/`max_format` use on the 26.x line. Scalar
    pack_format versions report (format, 0): 1.21.1 -> (48, 0),
    26.3 -> (121, 0).
    """
    major = data_pack_format_for(version)
    return (major, 0) if major is not None else None


def check_pack_format(target, pack_format, supported_min=None,
                      supported_max=None):
    """Whether a datapack declaring `pack_format` (and optionally a
    `supported_formats` [min, max] span, as pack.mcmeta allows since 1.20.2)
    is loadable on a server running `target`. Fail closed when `target` has
    no known pack_format."""
    target_pf = data_pack_format_for(target)
    if target_pf is None:
        return False
    if target_pf == pack_format:
        return True
    if supported_min is None and supported_max is None:
        return False
    lo = supported_min if supported_min is not None else 0
    hi = supported_max if supported_max is not None else 0xFFFFFFFF
    return lo <= target_pf <= hi


def check_pack_format_tuple(target, format, min_fmt=None, max_fmt=None):
    """Whether a datapack declaring (major, minor) format tuples — the
    pack.mcmeta `min_format`/`max_format` schema the 26.x line uses — is
    loadable on a server running `target`. Same contract as
    check_pack_format; tuples compare lexicographically."""
    target_fmt = data_pack_format_tuple_for(target)
    if target_fmt is None:
        return False
    if target_fmt == format:
        return True
    if min_fmt is None and max_fmt is None:
        return False
    lo = min_fmt if min_fmt is not None else (0, 0)
    hi = max_fmt if max_fmt is not None else (0xFFFFFFFF, 0xFFFFFFFF)
    return lo <= target_fmt <= hi


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


def _read_limited(read, limit=MAX_DATA_ENTRY_BYTES):
    """Read one archive/directory entry, or None when unreadable/oversized."""
    try:
        payload = read()
    except (OSError, KeyError, RuntimeError):
        return None
    if payload is None or len(payload) > limit:
        return None
    return payload


def _dimension_generator_type(read):
    """`generator.type` of a `data/*/dimension/*.json` file, or None."""
    payload = _read_limited(read)
    if payload is None:
        return None
    try:
        doc = json.loads(payload)
    except ValueError:
        return None
    generator = doc.get("generator") if isinstance(doc, dict) else None
    if not isinstance(generator, dict):
        return None
    gen_type = generator.get("type")
    return gen_type if isinstance(gen_type, str) else None


def _tag_member_ids(read):
    """Concrete (non-`#tag`) member ids of a `data/*/tags/.../*.json` file."""
    payload = _read_limited(read)
    if payload is None:
        return []
    try:
        doc = json.loads(payload)
    except ValueError:
        return []
    values = doc.get("values") if isinstance(doc, dict) else None
    if not isinstance(values, list):
        return []
    members = []
    for value in values:
        if isinstance(value, str):
            member = value
        elif isinstance(value, dict) and isinstance(value.get("id"), str):
            member = value["id"]
        else:
            continue
        # Nested `#ns:tag` references expand through the tag table, not here.
        if not member.startswith("#"):
            members.append(member)
    return members


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


def _mc_version_range_from_toml(text):
    """`versionRange` of the `[[dependencies.*]]` entry declaring
    `modId = "minecraft"` in a neoforge.mods.toml / mods.toml, or None."""
    try:
        import tomllib

        document = tomllib.loads(text)
    except Exception:
        document = None
    if isinstance(document, dict):
        dependencies = document.get("dependencies")
        if isinstance(dependencies, dict):
            for owner in sorted(dependencies):
                entries = dependencies[owner]
                if not isinstance(entries, list):
                    continue
                for entry in entries:
                    if (isinstance(entry, dict)
                            and isinstance(entry.get("modId"), str)
                            and entry["modId"].lower() == "minecraft"
                            and isinstance(entry.get("versionRange"), str)):
                        return entry["versionRange"]
    # Line-scan fallback when tomllib is unavailable or the parse failed.
    in_dep = False
    dep_id = None
    dep_range = None
    for line in text.splitlines():
        header = TABLE_HEADER.match(line)
        if header:
            if (dep_id is not None and dep_id.lower() == "minecraft"
                    and dep_range is not None):
                return dep_range
            name = header.group(1).strip()
            in_dep = "[[" in line and name.startswith("dependencies.")
            dep_id = dep_range = None
            continue
        if not in_dep:
            continue
        match = MODID_LINE.match(line)
        if match:
            dep_id = match.group(1)
            continue
        match = VERSION_RANGE_LINE.match(line)
        if match:
            dep_range = match.group(1)
    if (dep_id is not None and dep_id.lower() == "minecraft"
            and dep_range is not None):
        return dep_range
    return None


def _mc_version_range_from_fabric(doc):
    """`depends.minecraft` from a parsed fabric.mod.json, or None.

    A string is one range; a list is a set of alternatives (OR per the
    Fabric Loader spec), joined as a `||` union for the SemVer parser.
    """
    depends = doc.get("depends") if isinstance(doc, dict) else None
    if not isinstance(depends, dict):
        return None
    declared = depends.get("minecraft")
    if isinstance(declared, str):
        return declared
    if isinstance(declared, list):
        parts = [d for d in declared if isinstance(d, str)]
        if parts:
            return " || ".join(parts)
    return None


def _supported_formats_span(value):
    """pack.mcmeta `supported_formats` -> (min, max); each int or None.

    Accepts the three Mojang spellings: a bare int (exact), a `[min, max]`
    list, or `{"min_inclusive": n, "max_inclusive": n}`.
    """
    if isinstance(value, bool):
        return None, None
    if isinstance(value, int):
        return value, value
    if isinstance(value, (list, tuple)):
        lo = value[0] if len(value) >= 1 else None
        hi = value[1] if len(value) >= 2 else None
        lo = lo if isinstance(lo, int) and not isinstance(lo, bool) else None
        hi = hi if isinstance(hi, int) and not isinstance(hi, bool) else None
        return lo, hi
    if isinstance(value, dict):
        lo = value.get("min_inclusive")
        hi = value.get("max_inclusive")
        lo = lo if isinstance(lo, int) and not isinstance(lo, bool) else None
        hi = hi if isinstance(hi, int) and not isinstance(hi, bool) else None
        return lo, hi
    return None, None


def _format_tuple(value):
    """A 26.x pack.mcmeta format bound (`min_format`/`max_format`, or a
    tuple-shaped `pack_format`) -> (major, minor), or None when `value` is
    not a format declaration.

    Minecraft 26.x spells a bound as `[major, minor]` or a bare `major` int.
    """
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return (value, 0)
    if isinstance(value, (list, tuple)) and len(value) >= 1:
        major = value[0]
        minor = value[1] if len(value) >= 2 else 0
        if (isinstance(major, int) and not isinstance(major, bool)
                and isinstance(minor, int) and not isinstance(minor, bool)):
            return (major, minor)
    return None


def _format_tuple_text(fmt):
    """`[major, minor]` display of a (major, minor) format tuple."""
    return f"[{fmt[0]}, {fmt[1]}]"


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
        # Dimension-mod discovery (mirrors `discover_dimension_mod_entries` in
        # mod-adapter-core/src/dimension_mod.rs).
        self.dimension_types = []
        self.declared_dimensions = []
        self.custom_chunk_generators = []
        self.ore_tag_blocks = []
        self.chest_tag_blocks = []
        # Declared Minecraft version range in either loader syntax — Maven
        # interval from neoforge.mods.toml/mods.toml `versionRange`, or SemVer
        # from fabric.mod.json `depends.minecraft` (mirrors DiscoveredMod's
        # `mc_version_range: Option<String>` in fallback.rs). None means the
        # mod declares nothing and the --mc-version gate does not apply.
        self.mc_version_range = None
        # pack.mcmeta metadata for pure-data zips / datapack directories:
        # the legacy scalar pack_format/supported_formats schema, plus the
        # 26.x (major, minor) min_format/max_format tuples.
        self.pack_format = None
        self.pack_format_tuple = None
        self.pack_supported_min = None
        self.pack_supported_max = None
        self.pack_min_format = None
        self.pack_max_format = None
        declared = []
        namespaces = []
        toml_text = None
        fabric_range = None
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
                    if isinstance(doc, dict):
                        if isinstance(doc.get("id"), str):
                            declared.append(doc["id"])
                        fabric_range = _mc_version_range_from_fabric(doc)
                except (OSError, ValueError):
                    pass
            elif lowered == "pack.mcmeta":
                try:
                    doc = json.loads(read())
                except (OSError, ValueError):
                    doc = None
                pack = doc.get("pack") if isinstance(doc, dict) else None
                if isinstance(pack, dict):
                    pack_format = pack.get("pack_format")
                    if (isinstance(pack_format, int)
                            and not isinstance(pack_format, bool)):
                        self.pack_format = pack_format
                    else:
                        # The 26.x spelling `pack_format: [major, minor]`.
                        self.pack_format_tuple = _format_tuple(pack_format)
                    self.pack_supported_min, self.pack_supported_max = (
                        _supported_formats_span(
                            pack.get("supported_formats")))
                    self.pack_min_format = _format_tuple(
                        pack.get("min_format"))
                    self.pack_max_format = _format_tuple(
                        pack.get("max_format"))
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
            if len(parts) >= 4 and parts[0] == "data" and name.endswith(".json"):
                stem = "/".join(parts[3:])[:-len(".json")]
                if parts[2] == "dimension_type":
                    self.dimension_types.append(f"{parts[1]}:{stem}")
                elif parts[2] == "dimension":
                    self.declared_dimensions.append(f"{parts[1]}:{stem}")
                    gen_type = _dimension_generator_type(read)
                    if (gen_type is not None
                            and gen_type not in NATIVE_DIMENSION_GENERATORS):
                        self.custom_chunk_generators.append(gen_type)
                elif (parts[2] == "tags" and len(parts) >= 5
                        and parts[3] in ("block", "blocks")):
                    leaf = stem.rsplit("/", 1)[-1]
                    if leaf.startswith("ores"):
                        self.ore_tag_blocks.extend(_tag_member_ids(read))
                    elif leaf.startswith("chests"):
                        self.chest_tag_blocks.extend(_tag_member_ids(read))
        toml_range = None
        if toml_text is not None:
            declared = _mod_ids_from_toml(toml_text) + declared
            toml_range = _mc_version_range_from_toml(toml_text)
        # A NeoForge/Forge minecraft dependency wins over fabric.mod.json when
        # a jar somehow ships both manifests.
        self.mc_version_range = (
            toml_range if toml_range is not None else fabric_range)
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
        self.is_dimension_mod = bool(self.dimension_types) and bool(
            self.declared_dimensions)
        self.has_custom_chunk_generator = bool(self.custom_chunk_generators)

    def reject(self, code, symbol, detail):
        self.rejection_reasons.append({"code": code, "symbol": symbol, "detail": detail})

    def reason_codes(self):
        return [r["code"] for r in self.rejection_reasons]

    def dimension_report(self):
        """`dimension_mods` JSON report entry for one dimension mod."""
        return {
            "mod_id": self.mod_id,
            "dimensions": sorted(set(self.declared_dimensions)),
            "dimension_types": sorted(set(self.dimension_types)),
            "custom_chunk_generators": sorted(set(self.custom_chunk_generators)),
            "ore_tag_blocks": sorted(set(self.ore_tag_blocks)),
            "chest_tag_blocks": sorted(set(self.chest_tag_blocks)),
        }


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


def is_builtin_superseded(mod):
    """Whether any declared mod id is a built-in-superseded optimization."""
    return bool({m.lower() for m in mod.mod_ids} & BUILTIN_SUPERSEDED_PERF_MODS)


def apply_shimcheck(mods, reports):
    """Fill ShimCheck verdicts into bytecode mods; fail closed when absent."""
    for mod in mods:
        if not mod.has_java_bytecode or is_builtin_superseded(mod):
            # Superseded perf mods are skipped before ShimCheck entirely:
            # their Mixins can never attach a Tier 3 rejection reason.
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
    def __init__(self, mod, tier, reasons, mc_version_verdict=None):
        self.mod_id = mod.mod_id
        self.mod_ids = list(mod.mod_ids)
        self.tier = tier
        self.reasons = reasons
        self.source = str(mod.source)
        self.mc_version_range = getattr(mod, "mc_version_range", None)
        self.mc_version_verdict = (
            mc_version_verdict.kind if mc_version_verdict else None)
        self.pack_format = getattr(mod, "pack_format", None)
        self.pack_min_format = getattr(mod, "pack_min_format", None)
        self.pack_max_format = getattr(mod, "pack_max_format", None)

    def as_dict(self):
        return {
            "mod_id": self.mod_id,
            "tier": self.tier,
            "tier_name": TIER_NAMES[self.tier],
            "source": self.source,
            "reasons": self.reasons,
            "mc_version_range": self.mc_version_range,
            "mc_version_verdict": self.mc_version_verdict,
            "pack_format": self.pack_format,
            "pack_min_format": self.pack_min_format,
            "pack_max_format": self.pack_max_format,
        }


def _pack_format_verdict(mod, target):
    """PACK_FORMAT_INCOMPATIBLE detail text for a pure-data pack, or None.

    Mirrors check_pack_format / check_pack_format_tuple in version_compat.rs:
    compatible (or undeclared) pack metadata returns None; an incompatible
    declaration returns the operator-facing explanation that follows the
    reason code. Both the legacy scalar `pack_format`/`supported_formats`
    schema and the 26.x `(major, minor)` `min_format`/`max_format` tuples are
    gated; a pack declaring both schemas is compatible when either passes.
    """
    pack_format = getattr(mod, "pack_format", None)
    fmt_tuple = getattr(mod, "pack_format_tuple", None)
    min_fmt = getattr(mod, "pack_min_format", None)
    max_fmt = getattr(mod, "pack_max_format", None)
    if (pack_format is None and fmt_tuple is None
            and min_fmt is None and max_fmt is None):
        return None
    supported_min = getattr(mod, "pack_supported_min", None)
    supported_max = getattr(mod, "pack_supported_max", None)
    if pack_format is not None and check_pack_format(
            target, pack_format, supported_min, supported_max):
        return None
    has_tuple_schema = (fmt_tuple is not None or min_fmt is not None
                        or max_fmt is not None)
    if has_tuple_schema:
        declared_fmt = (fmt_tuple if fmt_tuple is not None
                        else (min_fmt if min_fmt is not None else max_fmt))
        if check_pack_format_tuple(target, declared_fmt, min_fmt, max_fmt):
            return None
    span = ""
    if supported_min is not None or supported_max is not None:
        lo = str(supported_min) if supported_min is not None else "0"
        hi = str(supported_max) if supported_max is not None else "any"
        span = f" (supported_formats {lo}-{hi})"
    declared = ""
    if pack_format is not None:
        declared = f"pack_format {pack_format}{span}"
    if has_tuple_schema:
        lo = _format_tuple_text(min_fmt) if min_fmt is not None else "[0, 0]"
        hi = _format_tuple_text(max_fmt) if max_fmt is not None else "any"
        tuple_text = f"min_format {lo} / max_format {hi}"
        declared = f"{declared} and {tuple_text}" if declared else tuple_text
    target_pf = data_pack_format_for(target)
    target_str = format_mc_version(target)
    if target_pf is None:
        return (f"declares {declared}; server is running "
                f"Minecraft {target_str}, which has no known datapack format")
    return (f"declares {declared}, server is running "
            f"Minecraft {target_str} (pack_format {target_pf})")


def _classify_execution_path(mod, flagship_ids, profile_ids, target):
    """Tier 0/1/2/3 cascade below the superseded catalog and the Minecraft
    version gate (mirrors classify_execution_path in fallback.rs)."""
    owned = set(mod.mod_ids)
    if owned & flagship_ids:
        return 0, [REASON_FLAGSHIP]
    if getattr(mod, "has_custom_chunk_generator", False):
        # A modded Java ChunkGenerator cannot run on the native Track 2
        # dynamic-dimension path or the bounded shim: explicit Tier 3.
        return 3, [CODE_CUSTOM_CHUNK_GENERATOR]
    if not mod.has_java_bytecode:
        # Pure-data zips / datapack directories: gate pack.mcmeta's declared
        # pack_format (plus optional supported_formats span) against the
        # target so a pack built for another data format never silently runs.
        pack_verdict = _pack_format_verdict(mod, target)
        if pack_verdict is not None:
            return 3, [CODE_PACK_FORMAT_INCOMPATIBLE, pack_verdict]
        return 1, [REASON_UNIVERSAL]
    if owned & profile_ids:
        return 1, [REASON_UNIVERSAL]
    if mod.shimcheck_compatible and not mod.has_unsupported_mixin_or_at:
        return 2, [REASON_SHIM]
    reasons = mod.reason_codes() or [REASON_FALLBACK_DEFAULT]
    return 3, reasons


def _classify_full(mod, flagship_ids, profile_ids, target):
    """(tier, reasons, mc_version_verdict) — classify_mod_for_version port."""
    if is_builtin_superseded(mod):
        # Checked before the version gate and every other step: a superseded
        # perf mod built for another Minecraft version (e.g. sodium declaring
        # 1.20.1) stays a clean BuiltInSuperseded skip with zero fallback
        # noise — its range is recorded but never gated on.
        return TIER_SUPERSEDED, [REASON_SUPERSEDED], None
    verdict = None
    declared = getattr(mod, "mc_version_range", None)
    if declared is not None:
        verdict = check_mod_mc_version(target, declared)
        target_str = format_mc_version(target)
        if verdict.kind == "invalid_range":
            return 3, [CODE_MC_VERSION_RANGE_INVALID,
                       f"declares an invalid Minecraft version range "
                       f"'{verdict.declared}' ({verdict.reason}); cannot "
                       f"confirm it supports server {target_str}"], verdict
        if verdict.kind in ("minor_mismatch", "major_mismatch"):
            return 3, [CODE_MC_VERSION_INCOMPATIBLE,
                       f"built for Minecraft {verdict.declared}, server is "
                       f"running {target_str}"], verdict
    tier, reasons = _classify_execution_path(
        mod, flagship_ids, profile_ids, target)
    return tier, reasons, verdict


def classify(mod, flagship_ids, profile_ids, target=DEFAULT_MC_VERSION):
    """Exact port of `classify_mod_for_version` in fallback.rs."""
    tier, reasons, _ = _classify_full(mod, flagship_ids, profile_ids, target)
    return tier, reasons


def plan_startup(mods, flagship_ids, profile_ids, saved_namespaces, policy,
                 load_safe_data_only=False, force_drop_saved_blocks=False,
                 target_mc_version=DEFAULT_MC_VERSION):
    """Exact port of `plan_mod_startup_for_version`; returns a dict plan."""
    assignments = []
    tier3 = []
    superseded = []
    needs_bounded_jvm = False
    for mod in mods:
        tier, reasons, verdict = _classify_full(
            mod, flagship_ids, profile_ids, target_mc_version)
        if tier == 2:
            needs_bounded_jvm = True
        assignment = Assignment(mod, tier, reasons, verdict)
        assignments.append(assignment)
        if tier == 3:
            tier3.append(assignment)
        elif tier == TIER_SUPERSEDED:
            superseded.append(assignment)
    superseded_ids = sorted({a.mod_id for a in superseded})
    mc_version = format_mc_version(target_mc_version)

    if not tier3:
        return {
            "plan": "RunNative",
            "mc_version": mc_version,
            "assignments": assignments,
            "needs_bounded_jvm": needs_bounded_jvm,
            "quarantined_mods": [],
            "load_quarantined_data_only": False,
            "superseded_perf_mods": superseded_ids,
        }
    if policy == "strict":
        return {
            "plan": "RefuseStartup",
            "mc_version": mc_version,
            "reason": REASON_REFUSE_STRICT,
            "blocking_mods": tier3,
            "assignments": assignments,
            "superseded_perf_mods": superseded_ids,
        }
    if policy == "quarantine":
        if not force_drop_saved_blocks:
            endangered = [a for a in tier3 if set(a.mod_ids) & saved_namespaces]
            if endangered:
                return {
                    "plan": "RefuseStartup",
                    "mc_version": mc_version,
                    "reason": REASON_REFUSE_QUARANTINE,
                    "blocking_mods": endangered,
                    "assignments": assignments,
                    "superseded_perf_mods": superseded_ids,
                }
        return {
            "plan": "RunNative",
            "mc_version": mc_version,
            "assignments": [a for a in assignments if a.tier != 3],
            "needs_bounded_jvm": needs_bounded_jvm,
            "quarantined_mods": tier3,
            "load_quarantined_data_only": load_safe_data_only,
            "superseded_perf_mods": superseded_ids,
        }
    if policy == "delegate":
        return {
            "plan": "DelegateToNeoForge",
            "mc_version": mc_version,
            "triggering_mods": tier3,
            "assignments": assignments,
            "superseded_perf_mods": superseded_ids,
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
        if assignment.tier == TIER_SUPERSEDED:
            # Superseded perf mods are never staged.
            continue
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
    env["MC_SPINE_SUPERSEDED_PERF_MODS"] = ",".join(
        plan.get("superseded_perf_mods", []))
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
        "mc_version": plan.get("mc_version"),
        "plan": plan["plan"],
        "needs_bounded_jvm": bool(plan.get("needs_bounded_jvm")),
        "assignments": [a.as_dict() for a in plan["assignments"]],
        "quarantined_mods": [a.as_dict() for a in plan.get("quarantined_mods", [])],
        "triggering_mods": [a.as_dict() for a in plan.get("triggering_mods", [])],
        "blocking_mods": [a.as_dict() for a in plan.get("blocking_mods", [])],
        "superseded_perf_mods": plan.get("superseded_perf_mods", []),
        "refusal_reason": plan.get("reason"),
        "dimension_mods": plan.get("dimension_mods", []),
        "saved_world_namespaces": sorted(plan.get("saved_namespaces", [])),
        "staging_dir": str(plan.get("staging_dir") or ""),
        "dry_run": bool(args.dry_run),
        "launch": launch,
    }


def _print_table(assignments):
    rows = [(a["mod_id"],
             a["tier_name"] if a["tier"] == TIER_SUPERSEDED
             else f"Tier {a['tier']} {a['tier_name']}",
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
    print(f"mc_version  : {report['mc_version']}")
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
    parser.add_argument("--mc-version", type=str, default="1.21.1",
                        help="Minecraft version the server runs (e.g. 1.21.1 "
                             "or 26.3); mod ranges and datapack "
                             "pack_format/min_format are gated against it "
                             "(default: 1.21.1)")
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
        target = parse_mc_version(args.mc_version)
        if target is None:
            raise LauncherError(
                f"invalid --mc-version '{args.mc_version}' "
                "(expected a numeric release like 1.21.1)")

        # Superseded perf mods are skipped before ShimCheck: scanning them
        # would only surface the heavy Mixins we intentionally ignore.
        bytecode_jars = [m.source for m in mods
                         if m.has_java_bytecode and m.source.is_file()
                         and not is_builtin_superseded(m)]
        reports = run_shimcheck(bytecode_jars, args)
        apply_shimcheck(mods, reports)

        plan = plan_startup(
            mods, flagship_ids, set(profile_ids), saved, args.fallback,
            load_safe_data_only=args.quarantine_load_data,
            force_drop_saved_blocks=args.force_drop_saved_blocks,
            target_mc_version=target)
        plan["saved_namespaces"] = saved
        if plan["superseded_perf_mods"]:
            log("Built-in optimizations replace {} mod(s) "
                "(skipped automatically): {}".format(
                    len(plan["superseded_perf_mods"]),
                    ", ".join(plan["superseded_perf_mods"])))
        plan["dimension_mods"] = [
            m.dimension_report() for m in mods if m.is_dimension_mod]

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
